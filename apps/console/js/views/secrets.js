// 秘密値（配信の出力の鍵・SRT のパスフレーズ・クラウドや字幕サービスの API キー）。tenant:admin が要る。
//
// 値は書くだけ: ノードは暗号化して保存し、API は値を返さない。この画面も値を保存・表示しない（送ったら欄を空にする）。
// 参照は `secret:{name}`（入力・出力の credential_ref、字幕の *_credential_ref）。使われている秘密値は消せない（409）。
// 「どこで使っているか」は API に無いので、配信ごとの入力・出力・字幕を読んで調べる（配信が多いときは時間がかかる）。
// 値の差し替え（PATCH は無い）: 使われていなければ消して同じ名前で作り直す。使われていれば新しい名前で作り、入力・出力の参照を
// 付け替えてから古いものを消す（字幕の参照は配信の設定で付け替える）。

import { h, replace, debounce } from '../lib/dom.js';
import { load, modal, confirmDialog, reportError, errorBox, toast, copyField } from '../lib/ui.js';
import { secrets, streams, inputs, outputs, listAll } from '../api/resources.js';
import { fmtTime, fmtRelative, idemKey } from '../lib/format.js';
import { forgetNames } from '../lib/names.js';
import { t, tn } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { listNav, button, setLoading, pageHeader, panel, dataTable, emptyState, formField, alert, helpHint, searchBox, badge, loadingLine } from '../ui/index.js';
import { onPageKey } from '../shortcuts.js';

const NAME_RE = /^[A-Za-z0-9._-]{1,64}$/;
const USES = [
  { id: 'output', icon: 'upload' },
  { id: 'srt', icon: 'shield' },
  { id: 'cloud', icon: 'globe' },
];

/** 値の欄（伏せる。目のボタンで見せる）。 */
function secretInput({ testid, label }) {
  const input = h('input', { type: 'password', autocomplete: 'new-password', spellcheck: 'false', autocapitalize: 'off', 'data-testid': testid });
  const eye = h('button', { type: 'button', class: 'icon-only ghost', 'aria-pressed': 'false', 'aria-label': t('secrets.showValue', { label }) }, icon('eye'));
  eye.addEventListener('click', () => {
    const show = input.type === 'password';
    input.type = show ? 'text' : 'password';
    eye.setAttribute('aria-pressed', String(show));
    replace(eye, icon(show ? 'eye-off' : 'eye'));
    input.focus();
  });
  const wrap = h('div', { class: 'secret-input' }, input, eye);
  return { input, wrap };
}

/** 配信ごとの入力・出力・字幕を読んで、秘密値の名前 → 使っている所 の表を作る。 */
async function scanUsage() {
  const st = await listAll(streams.list);
  const uses = new Map();
  const add = (name, u) => { if (!uses.has(name)) uses.set(name, []); uses.get(name).push(u); };
  const refName = (ref) => (typeof ref === 'string' && ref.startsWith('secret:') ? ref.slice(7) : null);
  let i = 0;
  const worker = async () => {
    while (i < st.length) {
      const s = st[i++];
      if (s.captions) {
        for (const k of ['asr_credential_ref', 'translate_credential_ref']) {
          const n = refName(s.captions[k]);
          if (n) add(n, { kind: 'captions', stream: s.id, streamName: s.name, field: k });
        }
      }
      const [ins, outs] = await Promise.all([inputs.list(s.id).catch(() => null), outputs.list(s.id).catch(() => null)]);
      for (const x of (ins && ins.data.items) || []) { const n = refName(x.credential_ref); if (n) add(n, { kind: 'input', id: x.id, stream: s.id, streamName: s.name, protocol: x.protocol }); }
      for (const x of (outs && outs.data.items) || []) { const n = refName(x.credential_ref); if (n) add(n, { kind: 'output', id: x.id, stream: s.id, streamName: s.name, protocol: x.provider || x.protocol }); }
    }
  };
  await Promise.all(Array.from({ length: Math.min(6, st.length) }, worker));
  return uses;
}

const useText = (u) => t(`secrets.use.${u.kind}`, { protocol: u.protocol || '' });

function usageList(us) {
  return h('ul', { class: 'usage-list' }, us.map((u) => h('li', null,
    icon(u.kind === 'output' ? 'upload' : u.kind === 'input' ? 'download' : 'captions'),
    h('a', { href: `#/streams/${encodeURIComponent(u.stream)}` }, u.streamName), h('span', { class: 'muted small' }, useText(u)))));
}

async function createDialog({ name: presetName = '', title = t('secrets.createTitle'), lockName = false } = {}) {
  const name = h('input', { type: 'text', maxlength: 64, autocomplete: 'off', spellcheck: 'false', placeholder: t('secrets.namePlaceholder'), value: presetName, readonly: lockName || null, 'data-testid': 'secret-name' });
  const { input: value, wrap } = secretInput({ testid: 'secret-value', label: t('secrets.value') });
  const refPreview = h('code', { class: 'ref-preview' });
  const nameField = formField({ label: t('common.name'), input: name, required: true, hint: t('secrets.nameHint') });
  const valueField = formField({ label: t('secrets.value'), input: value, required: true, hint: t('secrets.valueHint') });
  value.replaceWith(wrap);
  wrap.prepend(value);
  const errBox = h('div');
  const key = idemKey();
  const syncRef = () => replace(refPreview, `secret:${name.value || '…'}`);
  name.addEventListener('input', syncRef);
  syncRef();
  return modal(title, () => h('div', { class: 'form' },
    nameField, valueField,
    h('p', { class: 'small muted' }, t('secrets.refWillBe'), ' ', refPreview),
    alert('info', t('secrets.writeOnly'), { small: true }),
    errBox), {
    icon: 'secrets', testid: 'secret-create-dialog', initialFocus: lockName ? '[data-testid="secret-value"]' : undefined,
    actions: (close) => {
      const ok = button(t('common.create'), { variant: 'primary', icon: 'save', testid: 'secret-create-submit', onClick: async () => {
        replace(errBox);
        nameField.setError(null);
        valueField.setError(null);
        let bad = false;
        if (!NAME_RE.test(name.value)) { nameField.setError(t('secrets.nameInvalid')); bad = true; }
        if (!value.value) { valueField.setError(t('secrets.valueEmpty')); bad = true; }
        if (bad) { (NAME_RE.test(name.value) ? value : name).focus(); return; }
        setLoading(ok, true);
        try {
          const { data } = await secrets.create({ name: name.value, value: value.value }, key);
          value.value = '';
          close(data);
        } catch (e) {
          replace(errBox, errorBox(e, { title: t('secrets.createFailed') }));
        } finally {
          setLoading(ok, false);
        }
      } });
      return [button(t('common.cancel'), { onClick: () => { value.value = ''; close(null); } }), ok];
    },
  }).then((v) => { value.value = ''; return v || null; });
}

export async function render(root) {
  root = root.appendChild(h('div', { class: 'ops-view' }));
  const box = h('div');
  const toolbar = h('div', { class: 'table-toolbar' });
  let all = [];
  let usage = null; // Map | null（調べ中）| Error
  let query = '';
  let highlight = null;

  const usesOf = (x) => (usage instanceof Map ? usage.get(x.name) || [] : null);

  const refreshUsage = async () => {
    usage = null;
    redraw();
    try { usage = await scanUsage(); } catch (e) { usage = e; }
    redraw();
  };

  const create = async () => {
    const s = await createDialog();
    if (!s) return;
    forgetNames('secret');
    highlight = s.name;
    toast(t('secrets.created', { ref: s.ref }), 'ok');
    refresh();
  };

  const remove = async (x) => {
    const us = usesOf(x);
    if (us && us.length) {
      await modal(t('secrets.inUseTitle', { name: x.name }), () => [h('p', null, t('secrets.inUseBody')), usageList(us)], {
        icon: 'info', testid: 'secret-in-use', actions: (close) => [button(t('common.close'), { variant: 'primary', onClick: () => close() })],
      });
      return;
    }
    const ok = await confirmDialog(t('secrets.deleteTitle', { name: x.name }), t('secrets.deleteBody'), {
      ok: t('common.delete'), requireText: x.name,
      consequences: [t('secrets.deleteC1'), t('secrets.deleteC2')],
    });
    if (!ok) return;
    try {
      await secrets.remove(x.id);
      forgetNames('secret');
      toast(t('secrets.deleted', { name: x.name }), 'ok');
      refresh();
    } catch (e) {
      if (e.status === 409) {
        toast(t('secrets.inUse', { reason: e.title }), 'warn', 9000);
        refreshUsage();
      } else reportError(e, t('common.delete'));
    }
  };

  /** 値の差し替え。使っている所が無ければ同じ名前で作り直す。あれば新しい名前で作って付け替える。 */
  const rotate = async (x) => {
    const us = usesOf(x) || [];
    if (!us.length) {
      const ok = await confirmDialog(t('secrets.rotateTitle', { name: x.name }), t('secrets.rotateUnusedBody'), { ok: t('secrets.rotateNext'), danger: false, icon: 'rotate-key' });
      if (!ok) return;
      try {
        await secrets.remove(x.id);
      } catch (e) {
        if (e.status === 409) { toast(t('secrets.inUse', { reason: e.title }), 'warn', 9000); refreshUsage(); return; }
        reportError(e, t('secrets.rotate'));
        return;
      }
      const s = await createDialog({ name: x.name, title: t('secrets.rotateNewValue', { name: x.name }), lockName: true });
      if (s) { highlight = s.name; toast(t('secrets.rotated', { name: x.name }), 'ok'); } else toast(t('secrets.rotateAbandoned', { name: x.name }), 'warn', 10000);
      forgetNames('secret');
      refresh();
      return;
    }
    // 使われている: 新しい名前で作り、入力・出力を付け替える
    let n = 2;
    const base = x.name.replace(/-\d+$/, '');
    while (all.some((y) => y.name === `${base}-${n}`)) n += 1;
    const switchable = us.filter((u) => u.kind !== 'captions');
    const manual = us.filter((u) => u.kind === 'captions');
    const proceed = await modal(t('secrets.rotateTitle', { name: x.name }), () => [
      h('p', null, t('secrets.rotateUsedBody', { next: `${base}-${n}` })),
      switchable.length ? [h('h3', { class: 'sub-head' }, t('secrets.rotateWillSwitch')), usageList(switchable)] : null,
      manual.length ? [h('h3', { class: 'sub-head' }, t('secrets.rotateManual')), usageList(manual), h('p', { class: 'small muted' }, t('secrets.rotateManualBody'))] : null,
      alert('info', t('secrets.rotateReconnect'), { small: true }),
    ], {
      icon: 'rotate-key', wide: true,
      actions: (close) => [button(t('common.cancel'), { onClick: () => close(false) }), button(t('secrets.rotateNext'), { variant: 'primary', onClick: () => close(true) })],
    });
    if (!proceed) return;
    const s = await createDialog({ name: `${base}-${n}`, title: t('secrets.rotateNewValue', { name: x.name }) });
    if (!s) return;
    const failed = [];
    for (const u of switchable) {
      try {
        const api = u.kind === 'input' ? inputs : outputs;
        const { etag } = await api.get(u.id);
        await api.update(u.id, { credential_ref: s.ref }, etag);
      } catch (e) {
        failed.push([u, e]);
      }
    }
    forgetNames('secret');
    highlight = s.name;
    if (!failed.length && !manual.length) {
      try {
        await secrets.remove(x.id);
        toast(t('secrets.rotatedSwitched', { old: x.name, name: s.name, n: switchable.length }), 'ok', 8000);
      } catch {
        toast(t('secrets.rotatedKeepOld', { old: x.name, name: s.name }), 'warn', 10000);
      }
    } else if (failed.length) {
      toast(t('secrets.rotatePartial', { n: failed.length, old: x.name }), 'warn', 12000);
    } else {
      toast(t('secrets.rotatedKeepOld', { old: x.name, name: s.name }), 'warn', 10000);
    }
    refresh();
  };

  const usageCell = (x) => {
    if (usage === null) return h('span', { class: 'muted small' }, loadingLine(t('secrets.checking')));
    if (usage instanceof Error) return h('span', { class: 'muted small' }, '—');
    const us = usage.get(x.name) || [];
    if (!us.length) return h('span', { class: 'muted small' }, t('secrets.unused'));
    const streamsN = new Set(us.map((u) => u.stream)).size;
    return h('details', { class: 'usage' }, h('summary', null, badge(tn('secrets.usedBy', streamsN), 'info')), usageList(us));
  };

  const draw = () => {
    const rows = all.filter((x) => !query || x.name.toLowerCase().includes(query.toLowerCase()));
    const tbl = dataTable({
      caption: t('secrets.caption'),
      testid: 'secrets-table',
      sort: { key: 'name', dir: 'asc' },
      rowKey: (x) => x.name,
      columns: [
        { key: 'name', label: t('common.name'), primary: true, sort: (x) => x.name, render: (x) => h('span', { class: 'token-name' }, icon('secrets'), h('strong', null, x.name)) },
        { label: t('secrets.ref'), render: (x) => copyField(x.ref, t('secrets.refLabel', { name: x.name })) },
        { label: t('secrets.usedIn'), wrap: true, render: usageCell },
        { key: 'created', label: t('common.created'), sort: (x) => x.created_at, render: (x) => h('span', { title: fmtTime(x.created_at) }, fmtRelative(x.created_at)) },
        { label: t('common.actions'), actions: true, render: (x) => h('div', { class: 'row' },
          button(t('secrets.rotate'), { size: 'sm', icon: 'rotate-key', testid: 'secret-rotate', attrs: { 'aria-label': t('secrets.rotateLabel', { name: x.name }) }, onClick: () => rotate(x) }),
          button(t('common.delete'), { size: 'sm', variant: 'danger', iconOnly: true, icon: 'trash', testid: 'secret-delete', attrs: { 'aria-label': t('secrets.deleteLabel', { name: x.name }) }, onClick: () => remove(x) })) },
      ],
      rows,
      empty: { icon: 'filter', title: t('secrets.noMatch') },
    });
    if (highlight) {
      const tr = tbl.querySelector(`tr[data-key="${CSS.escape(highlight)}"]`);
      if (tr) { tr.classList.add('just-added'); setTimeout(() => tr.classList.remove('just-added'), 2400); }
      highlight = null;
    }
    return tbl;
  };
  const redraw = () => { if (all.length) replace(box, draw()); };

  const usesPanel = () => h('div', { class: 'use-cards' }, USES.map((u) => h('div', { class: 'use-card' },
    h('span', { class: 'uc-icon' }, icon(u.icon)),
    h('div', null, h('strong', null, t(`secrets.useCase.${u.id}`)), h('p', { class: 'small muted' }, t(`secrets.useCaseBody.${u.id}`))))));

  const refresh = () => load(box, async () => {
    const { data } = await secrets.list();
    all = data.items;
    if (!all.length) {
      toolbar.hidden = true;
      const es = emptyState({
        icon: 'secrets', title: t('secrets.emptyTitle'), body: t('secrets.emptyBody'), testid: 'secrets-empty',
        actions: [button(t('secrets.new'), { icon: 'plus', onClick: create })],
      });
      es.insertBefore(usesPanel(), es.querySelector('.es-actions'));
      return es;
    }
    toolbar.hidden = false;
    replace(toolbar,
      searchBox({ placeholder: t('secrets.search'), value: query, shortcut: true, testid: 'secrets-search', onInput: debounce((v) => { query = v.trim(); redraw(); }, 150) }),
      h('span', { class: 'spacer' }),
      button(t('secrets.recheck'), { variant: 'ghost', size: 'sm', icon: 'refresh', onClick: () => refreshUsage() }));
    if (usage === null || usage instanceof Error) refreshUsage();
    return draw();
  }, { title: t('secrets.loadFailed'), kind: 'table' });

  const hasNew = new URLSearchParams(location.hash.split('?')[1] || '').get('new') === '1';
  root.append(
    pageHeader({
      title: t('nav.secrets'),
      description: t('secrets.lead'),
      help: helpHint({ title: t('nav.secrets'), body: t('secrets.help'), guide: '06-inputs-outputs.md' }),
      actions: [button(t('secrets.new'), { variant: 'primary', icon: 'plus', kbd: 'N', testid: 'secrets-create', onClick: create })],
    }),
    panel({ testid: 'secrets-panel' }, toolbar, box),
    h('details', { class: 'about-secrets' }, h('summary', null, icon('info'), t('secrets.aboutTitle')), usesPanel(), h('p', { class: 'small muted' }, t('secrets.writeOnly'))));
  await refresh();
  if (hasNew) {
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/secrets`);
    create();
  }
  const offs = [onPageKey('n', create), listNav(box, { rows: () => [...box.querySelectorAll('tbody tr')], open: (tr) => { const d = tr.querySelector('details.usage'); if (d) d.open = !d.open; } })];
  return () => offs.forEach((f) => f());
}
