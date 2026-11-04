// 配信の詳細の「同時配信」: この配信を YouTube・Twitch・ほかの RTMP・SRT・クラウドのサービスへ送る出力（outputs）。
//
// - 行き先のひな形（YouTube など）は rtmp_push の URL を埋めるだけ（API は汎用の rtmp_push のまま）。
// - ストリームキーは秘密値（POST /secrets）にして credential_ref で参照する。この画面の中で新しい秘密値を作れる
//   （値は送ったらすぐ欄から消し、画面にも保存場所にも残さない）。rtmp_push は秘密値の値を URL の後ろにつないで送る。
// - 出力の状態（status）はこのノードでの実行の様子。SSE にならないので、見えている間は呼ぶ側が reload() を繰り返す。

import { h, replace, select } from '../lib/dom.js';
import { modal, confirmDialog, toast, reportError, errorBox, load } from '../lib/ui.js';
import { outputs as outputsApi, secrets as secretsApi } from '../api/resources.js';
import { t } from '../i18n/index.js';
import { button, iconButton, emptyState, formField, segmented, choiceCards, alert, toggle } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { ioStatus, PROVIDERS, parseOptions, showOptions } from './stream-io.js';
import { NAME_RE } from './stream-form.js';

/** 行き先のひな形。url は rtmp_push の URL（鍵は秘密値から後ろにつなぐ）。 */
export const DESTINATIONS = [
  { id: 'youtube', protocol: 'rtmp_push', url: 'rtmps://a.rtmps.youtube.com:443/live2', match: /youtube\.com/i, icon: 'play' },
  { id: 'twitch', protocol: 'rtmp_push', url: 'rtmp://live.twitch.tv/app', match: /twitch\.tv/i, icon: 'switcher' },
  { id: 'facebook', protocol: 'rtmp_push', url: 'rtmps://live-api-s.facebook.com:443/rtmp', match: /facebook\.com/i, icon: 'users' },
  { id: 'rtmp', protocol: 'rtmp_push', url: '', icon: 'link' },
  { id: 'srt', protocol: 'srt_push', url: '', icon: 'activity' },
  { id: 'paas', protocol: 'paas', url: '', icon: 'globe' },
];

/** 出力から行き先を推し量る（表示の名前とアイコンに使う）。 */
export function destinationOf(o) {
  if (o.protocol === 'paas') return DESTINATIONS.find((d) => d.id === 'paas');
  if (o.protocol === 'srt_push') return DESTINATIONS.find((d) => d.id === 'srt');
  return DESTINATIONS.find((d) => d.match && d.match.test(o.url || '')) || DESTINATIONS.find((d) => d.id === 'rtmp');
}

function hostOf(url) {
  try { return new URL(url).host; } catch { return url || ''; }
}

/** 秘密値の名前の候補（英数字と . _ -、64 文字まで）。 */
export function secretNameFor(streamName, dest) {
  const base = `${streamName}-${dest}-key`.replace(/[^A-Za-z0-9._-]/g, '-').replace(/^\.+/, '');
  return base.slice(0, 64);
}

async function listSecrets() {
  try {
    const { data } = await secretsApi.list();
    return data.items;
  } catch {
    return null; // tenant:admin が無い
  }
}

/**
 * 出力の追加・変更のダイアログ。
 * @param {{stream: object, cur?: object|null, caps?: object}} o
 */
export async function outputDialog({ stream, cur = null, caps = null }) {
  const saved = await listSecrets();
  // 同じ名前の秘密値があれば -2・-3 … を付ける
  const uniqueName = (d) => {
    const base = secretNameFor(stream.name, d);
    const taken = new Set((saved || []).map((x) => x.name));
    if (!taken.has(base)) return base;
    for (let i = 2; i < 100; i++) { const n = `${base.slice(0, 60)}-${i}`; if (!taken.has(n)) return n; }
    return base;
  };
  const paasOk = !caps || !caps.outputs || caps.outputs.includes('paas');
  const errBox = h('div');
  let dest = cur ? destinationOf(cur) : DESTINATIONS[0];

  // 行き先
  const destCards = cur ? null : choiceCards({
    label: t('io.rs.where'), testid: 'restream-dest', compact: true, value: dest.id,
    options: DESTINATIONS.map((d) => ({
      value: d.id, icon: d.icon, title: t(`io.rs.dest.${d.id}`), description: t(`io.rs.destDesc.${d.id}`),
      disabled: d.id === 'paas' && !paasOk,
    })),
    onChange: (v) => { dest = DESTINATIONS.find((d) => d.id === v); url.value = dest.url; secretName.value = uniqueName(dest.id); sync(); },
  });

  const url = h('input', { type: 'text', value: cur ? cur.url || '' : dest.url, spellcheck: 'false', autocomplete: 'off', 'data-testid': 'restream-url' });
  const urlField = formField({ label: t('io.rs.server'), input: url, required: true });
  const provider = select(PROVIDERS, cur ? cur.provider || 'cloudflare_stream' : 'cloudflare_stream', { disabled: !!cur });
  const provField = formField({ label: t('io.provider'), input: provider });
  const provHint = h('div', { class: 'hint' });

  // 鍵: 新しく貼る（秘密値を作る）・保存した秘密値・（変更のとき）今のまま
  const keyModes = [
    ...(cur && cur.credential_ref ? [['keep', t('io.rs.keyKeep')]] : []),
    ['new', t('io.rs.keyNew')],
    ...(saved === null || saved.length ? [['saved', t('io.rs.keySaved')]] : []),
    ['none', t('io.rs.keyNone')],
  ];
  const keyMode = segmented({ label: t('io.rs.keyHow'), options: keyModes, value: keyModes[0][0], testid: 'restream-key-mode', onChange: () => sync() });
  const keyInput = h('input', { type: 'password', autocomplete: 'new-password', spellcheck: 'false', 'data-testid': 'restream-key' });
  const keyField = formField({ label: t('io.rs.key'), input: keyInput, required: true });
  const secretName = h('input', { type: 'text', maxlength: 64, value: uniqueName(dest.id), spellcheck: 'false', autocomplete: 'off', 'data-testid': 'restream-secret-name' });
  const secretNameField = formField({ label: t('io.rs.secretName'), input: secretName, hint: t('io.rs.secretNameHint') });
  const newBox = h('div', { class: 'stack' }, keyField, secretNameField);
  const savedSel = saved
    ? select(saved.map((x) => [x.ref, x.name]), cur && cur.credential_ref ? cur.credential_ref : saved[0] ? saved[0].ref : '', { 'data-testid': 'restream-secret' })
    : h('input', { type: 'text', placeholder: 'secret:name', value: cur && cur.credential_ref ? cur.credential_ref : '' });
  const savedField = formField({ label: t('io.rs.savedSecret'), input: savedSel, hint: saved ? t('io.credentialHint') : t('io.credentialNoList') });
  const keepNote = h('p', { class: 'hint' }, cur && cur.credential_ref ? t('io.rs.keepNote', { ref: cur.credential_ref }) : '');
  const noneNote = h('p', { class: 'hint' });
  const keyBox = h('fieldset', { class: 'key-box' }, h('legend', null, t('io.rs.keyLegend')), keyMode, newBox, savedField, keepNote, noneNote);

  const enabled = toggle({ label: t('io.rs.enabled'), description: t('io.rs.enabledHint'), checked: cur ? cur.enabled : true, testid: 'restream-enabled' });
  const options = h('textarea', { rows: 3, spellcheck: 'false', placeholder: t('io.optionsPlaceholder') }, showOptions(cur && cur.options));
  const advanced = h('details', { class: 'disclosure' }, h('summary', null, icon('settings'), h('span', null, t('io.rs.advanced'))),
    h('div', { class: 'form' }, formField({ label: t('io.options'), input: options, hint: t('io.rs.optionsHint') })));
  const destHint = h('div', { class: 'hint dest-hint' });

  const sync = () => {
    const p = dest.protocol;
    urlField.hidden = p === 'paas';
    provField.hidden = p !== 'paas';
    provHint.hidden = p !== 'paas';
    provHint.textContent = p === 'paas' ? t(`io.providerHint.${provider.value}`) : '';
    url.placeholder = p === 'srt_push' ? 'srt://receiver.example.com:9000?streamid=…' : 'rtmp://host/app';
    destHint.textContent = t(`io.rs.destHint.${dest.id}`);
    keyField.querySelector('label').textContent = p === 'srt_push' ? t('io.rs.passphrase') : p === 'paas' ? t('io.rs.apiCredential') : t('io.rs.key');
    const m = keyMode.value;
    newBox.hidden = m !== 'new';
    savedField.hidden = m !== 'saved';
    keepNote.hidden = m !== 'keep';
    noneNote.hidden = m !== 'none';
    noneNote.textContent = p === 'rtmp_push' ? t('io.rs.noneRtmp') : p === 'paas' ? t('io.rs.nonePaas') : t('io.rs.noneSrt');
  };
  provider.addEventListener('change', sync);
  sync();

  const save = async (close, ok) => {
    replace(errBox);
    let opts;
    try { opts = parseOptions(options.value); } catch (e) { replace(errBox, alert('error', e.message)); return; }
    const p = dest.protocol;
    if (p !== 'paas' && !url.value.trim()) { urlField.setError(t('io.rs.needUrl')); url.focus(); return; }
    urlField.setError(null);
    const m = keyMode.value;
    if (m === 'new' && !keyInput.value) { keyField.setError(t('io.rs.needKey')); keyInput.focus(); return; }
    keyField.setError(null);
    if (m === 'new' && !NAME_RE.test(secretName.value)) { secretNameField.setError(t('io.rs.badSecretName')); secretName.focus(); return; }
    secretNameField.setError(null);
    if (p === 'paas' && m === 'none') { replace(errBox, alert('error', t('io.rs.nonePaas'))); return; }
    ok.disabled = true;
    try {
      let ref = null;
      if (m === 'new') {
        const value = keyInput.value;
        keyInput.value = ''; // 値は持ち続けない
        try {
          ref = (await secretsApi.create({ name: secretName.value, value })).data.ref;
        } catch (e) {
          if (e.status === 409) { replace(errBox, errorBox(e, { title: t('io.rs.secretExists', { name: secretName.value }) })); keyInput.focus(); return; }
          throw e;
        }
      } else if (m === 'saved') ref = savedSel.value || null;
      else if (m === 'keep') ref = cur.credential_ref;
      let r;
      if (cur) {
        const b = {};
        if (cur.protocol !== 'paas' && url.value.trim() !== (cur.url || '')) b.url = url.value.trim();
        if (ref !== (cur.credential_ref || null)) b.credential_ref = ref;
        if (enabled.input.checked !== cur.enabled) b.enabled = enabled.input.checked;
        if (showOptions(opts) !== showOptions(cur.options)) b.options = opts;
        if (!Object.keys(b).length) { close(null); return; }
        r = await outputsApi.update(cur.id, b, `"${cur.generation}"`);
      } else {
        const b = { protocol: p, enabled: enabled.input.checked, options: opts };
        if (p === 'paas') b.provider = provider.value;
        else b.url = url.value.trim();
        if (ref) b.credential_ref = ref;
        r = await outputsApi.create(stream.id, b);
      }
      close(r.data);
    } catch (e) {
      replace(errBox, errorBox(e, { title: t('io.saveFailed') }));
    } finally {
      ok.disabled = false;
    }
  };

  return modal(cur ? t('io.rs.editTitle') : t('io.rs.addTitle'), () => [
    h('div', { class: 'form' },
      destCards,
      cur ? h('div', { class: 'dest-fixed' }, icon(dest.icon), h('strong', null, t(`io.rs.dest.${dest.id}`))) : null,
      destHint,
      urlField, provField, provHint,
      keyBox,
      enabled.el,
      advanced),
    errBox,
  ], {
    wide: true,
    icon: 'send-arrow',
    testid: 'restream-dialog',
    initialFocus: cur ? '[data-testid="restream-url"]' : '[data-testid="restream-dest"] input:checked',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'restream-save' }, cur ? t('common.save') : t('io.rs.addOk'));
      ok.addEventListener('click', () => save(close, ok));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  });
}

/**
 * 同時配信の一覧と操作。
 * @param {() => object} getStream
 * @param {object} caps
 * @returns {{el: HTMLElement, reload: () => Promise<any>, count: () => number}}
 */
export function restreamSection(getStream, caps) {
  const box = h('div', { 'data-testid': 'restream-list' });
  let items = [];
  const add = async () => {
    const r = await outputDialog({ stream: getStream(), caps });
    if (r) { toast(t('io.rs.added'), 'ok'); reload(); }
  };
  const addBtn = (variant = 'secondary') => button(t('io.rs.add'), { variant, icon: 'plus', testid: 'restream-add', onClick: add });

  const row = (o) => {
    const d = destinationOf(o);
    const live = getStream().observed_state === 'live';
    const sw = toggle({
      label: h('span', { class: 'visually-hidden' }, t('io.rs.enableFor', { name: t(`io.rs.dest.${d.id}`) })),
      checked: o.enabled, testid: 'restream-toggle',
      onChange: async (on) => {
        try { await outputsApi.update(o.id, { enabled: on }, `"${o.generation}"`); toast(on ? t('io.rs.enabledToast') : t('io.rs.disabledToast'), 'ok'); } catch (e) { reportError(e, on ? t('io.enable') : t('io.disable')); }
        reload();
      },
    });
    const waitingNote = o.enabled && !live ? h('div', { class: 'small muted' }, t('io.rs.waitsForLive')) : null;
    return h('li', { class: 'dest', 'data-output': o.id, 'data-protocol': o.protocol },
      h('div', { class: 'dest-icon', 'aria-hidden': 'true' }, icon(d.icon)),
      h('div', { class: 'dest-main' },
        h('div', { class: 'dest-title' }, h('strong', null, d.id === 'rtmp' || d.id === 'srt' ? hostOf(o.url) || t(`io.rs.dest.${d.id}`) : t(`io.rs.dest.${d.id}`)),
          o.provider ? h('span', { class: 'tag' }, o.provider) : h('span', { class: 'tag' }, o.protocol)),
        o.url ? h('code', { class: 'dest-url' }, o.url) : null,
        h('div', { class: 'dest-meta small muted' }, icon('key', { size: 'sm' }), o.credential_ref ? h('span', null, o.credential_ref) : h('span', null, t('io.rs.noKey')))),
      h('div', { class: 'dest-status' }, ioStatus(o.status, o.enabled), waitingNote),
      h('div', { class: 'dest-actions' }, sw.el,
        iconButton('edit', t('io.rs.editFor', { name: o.url || o.provider || o.protocol }), { size: 'sm', testid: 'restream-edit', onClick: async () => { if (await outputDialog({ stream: getStream(), cur: o, caps })) { toast(t('io.saved.output'), 'ok'); reload(); } } }),
        iconButton('trash', t('io.deleteLabel', { what: t('io.output'), protocol: o.protocol }), { size: 'sm', variant: 'danger', testid: 'restream-delete', onClick: async () => {
          const target = o.url || o.provider || o.protocol;
          if (!(await confirmDialog(t('io.deleteTitle.output'), t('io.deleteBody', { target }), {
            ok: t('common.delete'), consequences: [o.protocol === 'paas' ? t('io.deletePaas') : t('io.deleteConseq.output'), o.credential_ref ? t('io.rs.deleteKeepsSecret', { ref: o.credential_ref }) : null].filter(Boolean),
          }))) return;
          try { await outputsApi.remove(o.id); toast(t('common.deleted'), 'ok'); reload(); } catch (e) { reportError(e, t('common.delete')); }
        } })));
  };

  const reload = () => load(box, async () => {
    const { data } = await outputsApi.list(getStream().id);
    items = data.items;
    if (!items.length) {
      return emptyState({
        icon: 'send-arrow', title: t('io.rs.emptyTitle'), body: t('io.rs.emptyBody'), testid: 'restream-empty',
        actions: [addBtn('primary')],
      });
    }
    return h('div', null,
      h('ul', { class: 'dest-list' }, items.map(row)),
      h('div', { class: 'toolbar mt-4' }, addBtn()));
  }, { kind: 'lines' });

  // 見えている間の読み直し: 出力の作りが同じなら状態の欄だけ差し替える（スイッチ・ボタンのフォーカスを奪わない）
  const shape = (list) => JSON.stringify(list.map((o) => [o.id, o.generation, o.enabled, o.url, o.credential_ref]));
  const refreshStatus = async () => {
    if (box.dataset.loaded !== '1') return reload();
    let data;
    try { ({ data } = await outputsApi.list(getStream().id)); } catch { return null; }
    if (shape(data.items) !== shape(items)) { items = data.items; return reload(); }
    items = data.items;
    const live = getStream().observed_state === 'live';
    for (const o of items) {
      const li = box.querySelector(`li[data-output="${CSS.escape(o.id)}"] .dest-status`);
      if (li) replace(li, ioStatus(o.status, o.enabled), o.enabled && !live ? h('div', { class: 'small muted' }, t('io.rs.waitsForLive')) : null);
    }
    return null;
  };

  return { el: box, reload, refreshStatus, count: () => items.length };
}
