// API トークン: 一覧（役割・権限・期限）・発行・失効。tenant:admin が要る。#/tokens?new=1 で発行のダイアログを開く。
//
// 発行は「何に使うか」（役割のひな形 → 権限）から選び、細かい権限は「権限を個別に選ぶ」で変える。期限はひな形（既定 90 日）。
// 値は発行の直後に 1 回だけ見せる。写さずに閉じようとしたら一度止める（ダイアログの beforeClose）。
// 最後に使った時刻は API に無いので出さない。

import { h, replace, debounce } from '../lib/dom.js';
import { load, modal, confirmDialog, reportError, errorBox, toast, copyField } from '../lib/ui.js';
import { tokens, SCOPES, listAll } from '../api/resources.js';
import { fmtTime, fmtRelative, wallToUtcIso, localZone, idemKey } from '../lib/format.js';
import { forgetNames } from '../lib/names.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { listNav, button, setLoading, pageHeader, dataTable, emptyState, badge, formField, helpHint, alert, segmented, searchBox, panel } from '../ui/index.js';
import { onPageKey } from '../shortcuts.js';

/** 役割のひな形（何に使うか → 権限）。 */
export const ROLES = [
  { id: 'encoder', icon: 'send', scopes: ['publish'] },
  { id: 'viewer', icon: 'eye', scopes: ['stream:read', 'recording:read', 'subscribe'] },
  { id: 'operator', icon: 'streams', scopes: ['stream:read', 'stream:write', 'publish', 'subscribe', 'recording:read', 'recording:delete'] },
  { id: 'admin', icon: 'shield', scopes: ['tenant:admin'] },
];
const EXPIRY = [['7d', 7], ['30d', 30], ['90d', 90], ['1y', 365], ['never', null], ['custom', null]];
const SOON = 7 * 86400000;

/** 権限から役割を決める（ひな形と同じなら役割、tenant:admin を含めば admin、ほかは custom）。node:admin は別に数える。 */
export function roleOf(scopes) {
  const s = new Set(scopes.filter((x) => x !== 'node:admin'));
  if (s.has('tenant:admin')) return 'admin';
  const r = ROLES.find((x) => x.scopes.length === s.size && x.scopes.every((y) => s.has(y)));
  return r ? r.id : 'custom';
}

const expired = (x) => x.expires_at && new Date(x.expires_at).getTime() <= Date.now();
const expiringSoon = (x) => x.expires_at && !expired(x) && new Date(x.expires_at).getTime() - Date.now() < SOON;

async function createDialog() {
  const name = h('input', { type: 'text', maxlength: 64, placeholder: t('tokens.namePlaceholder'), autocomplete: 'off', spellcheck: 'false', 'data-testid': 'token-name' });
  const nameField = formField({ label: t('common.name'), input: name, required: true, hint: t('tokens.nameHint') });
  let role = 'viewer';
  const checks = new Map();
  const errBox = h('div');
  const key = idemKey();

  // 役割のカード（radiogroup）
  const roleInputs = [];
  const roleCards = h('div', { class: 'role-cards', role: 'radiogroup', 'aria-label': t('tokens.roleLabel') },
    [...ROLES, { id: 'custom', icon: 'settings', scopes: null }].map((r) => {
      const input = h('input', { type: 'radio', name: 'token-role', value: r.id, checked: r.id === role, 'data-testid': `token-role-${r.id}` });
      roleInputs.push(input);
      input.addEventListener('change', () => { if (input.checked) setRole(r.id); });
      return h('label', { class: 'role-card' },
        input,
        h('span', { class: 'rc-icon' }, icon(r.icon)),
        h('span', { class: 'rc-text' },
          h('strong', null, t(`tokens.role.${r.id}`)),
          h('span', null, t(`tokens.roleBody.${r.id}`)),
          r.scopes ? h('span', { class: 'rc-scopes' }, r.scopes.map((s) => h('code', null, s))) : null));
    }));

  // 権限を個別に
  const scopeList = h('div', { class: 'scope-list' }, SCOPES.map(([v]) => {
    const input = h('input', { type: 'checkbox', value: v, 'data-testid': `token-scope-${v}` });
    checks.set(v, input);
    input.addEventListener('change', () => syncFromChecks());
    return h('label', { class: ['scope-item', v === 'node:admin' && 'is-node'].filter(Boolean).join(' ') }, input,
      h('span', { class: 'si-text' },
        h('span', { class: 'row' }, h('code', null, v), h('strong', null, t(`tokens.scope.${v}`)), v === 'node:admin' || v === 'tenant:admin' ? badge(t('tokens.powerful'), 'warn') : null),
        h('span', { class: 'small muted' }, t(`tokens.scopeHelp.${v}`))));
  }));
  const advanced = h('details', { class: 'scope-details' }, h('summary', null, t('tokens.advanced')), h('p', { class: 'small muted' }, t('tokens.scopesHint')), scopeList);
  const summary = h('p', { class: 'small muted scope-summary', 'aria-live': 'polite' });

  const selected = () => [...checks].filter(([, i]) => i.checked).map(([v]) => v);
  const drawSummary = () => {
    const s = selected();
    replace(summary, s.length ? [t('tokens.willHave'), ' ', s.map((x) => h('code', null, x))] : t('tokens.needScope'));
  };
  function setRole(id) {
    role = id;
    const r = ROLES.find((x) => x.id === id);
    if (r) {
      const node = checks.get('node:admin').checked;
      for (const [v, i] of checks) i.checked = r.scopes.includes(v) || (v === 'node:admin' && node);
    } else {
      advanced.open = true;
    }
    for (const i of roleInputs) i.checked = i.value === id;
    drawSummary();
  }
  function syncFromChecks() {
    role = roleOf(selected());
    for (const i of roleInputs) i.checked = i.value === role;
    drawSummary();
  }
  setRole(role);

  // 期限
  const custom = h('input', { type: 'datetime-local', 'aria-label': t('tokens.expiresCustom', { zone: localZone() }) });
  const customRow = h('div', { class: 'field', hidden: true }, h('label', null, t('tokens.expiresCustom', { zone: localZone() })), custom);
  const exp = segmented({
    label: t('tokens.expires'), value: '90d', testid: 'token-expiry',
    options: EXPIRY.map(([k]) => [k, t(`tokens.expiry.${k}`)]),
    onChange: (v) => { customRow.hidden = v !== 'custom'; if (v === 'custom') custom.focus(); },
  });
  const expiresAt = () => {
    const v = exp.value;
    if (v === 'never') return null;
    if (v === 'custom') return custom.value ? wallToUtcIso(custom.value, localZone()) : undefined;
    const days = EXPIRY.find(([k]) => k === v)[1];
    return new Date(Date.now() + days * 86400000).toISOString();
  };

  return modal(t('tokens.createTitle'), () => h('div', { class: 'form token-form' },
    nameField,
    h('fieldset', null, h('legend', null, t('tokens.roleLabel')), roleCards, advanced, summary),
    h('fieldset', null, h('legend', null, t('tokens.expires')), exp, customRow, h('p', { class: 'hint' }, t('tokens.expiresHint'))),
    errBox), {
    icon: 'key',
    wide: true,
    testid: 'token-create-dialog',
    actions: (close) => {
      const ok = button(t('tokens.issueOk'), { variant: 'primary', icon: 'key', testid: 'token-create-submit', onClick: async () => {
        replace(errBox);
        nameField.setError(null);
        const scopes = selected();
        const problems = [];
        if (!name.value.trim()) { nameField.setError(t('tokens.needName')); name.focus(); return; }
        if (!scopes.length) problems.push(t('tokens.needScope'));
        const e = expiresAt();
        if (e === undefined) problems.push(t('tokens.needDate'));
        else if (e && new Date(e) <= new Date()) problems.push(t('tokens.needFuture'));
        if (problems.length) {
          replace(errBox, alert('error', h('ul', null, problems.map((p) => h('li', null, p))), { role: 'alert' }));
          return;
        }
        setLoading(ok, true);
        try {
          const { data } = await tokens.create({ name: name.value.trim(), scopes, expires_at: e }, key);
          close(data);
        } catch (err) {
          replace(errBox, errorBox(err, { title: t('tokens.createFailed') }));
        } finally {
          setLoading(ok, false);
        }
      } });
      return [button(t('common.cancel'), { onClick: () => close(null) }), ok];
    },
  });
}

/** 発行した値を 1 回だけ見せる。写したか「控えた」を押すまで、Escape・×では閉じない（もう一度押せば閉じる）。 */
function revealDialog(tk) {
  let copied = false;
  let warned = false;
  const warn = h('div', { 'aria-live': 'assertive' });
  const field = copyField(tk.token, t('tokens.valueLabel', { name: tk.name }), { testid: 'token-value' });
  field.querySelector('button:last-child').addEventListener('click', () => { copied = true; replace(warn); });
  field.querySelector('input').addEventListener('copy', () => { copied = true; replace(warn); });
  const guard = () => {
    if (copied || warned) return true;
    warned = true;
    replace(warn, alert('warn', t('tokens.notCopied'), { small: true, testid: 'token-not-copied' }));
    return false;
  };
  const origin = location.origin;
  return modal(t('tokens.issuedTitle'), () => [
    h('p', { class: 'reveal-lead' }, t('tokens.revealLead', { name: tk.name })),
    h('div', { class: 'token-reveal', 'data-testid': 'token-reveal' }, field),
    h('ul', { class: 'reveal-points small' },
      h('li', null, icon('eye-off'), t('tokens.onceWarning')),
      h('li', null, icon('lightbulb'), t('tokens.storeHint'))),
    h('details', { class: 'small' }, h('summary', null, t('tokens.tryIt')),
      h('pre', { class: 'code-sample' }, `curl -H "Authorization: Bearer $SENCA_TOKEN" ${origin}/api/v1/streams`)),
    warn,
  ], {
    icon: 'success', kind: 'info', testid: 'token-reveal-dialog', beforeClose: guard, initialFocus: '.token-reveal button:last-child',
    actions: (close) => [button(t('tokens.done'), { variant: 'primary', testid: 'token-reveal-done', onClick: () => { if (guard()) close(true); } })],
  });
}

export async function render(root) {
  root = root.appendChild(h('div', { class: 'ops-view' }));
  const box = h('div');
  const q0 = new URLSearchParams(location.hash.split('?')[1] || '');
  let filter = ['expiring', 'expired'].includes(q0.get('filter')) ? q0.get('filter') : '';
  let query = '';
  let all = [];

  const issue = async () => {
    try {
      const tk = await createDialog();
      if (!tk) return;
      forgetNames('token');
      if (tk.token) await revealDialog(tk);
      else toast(t('tokens.noValue'), 'warn', 9000);
      refresh();
    } catch (e) {
      reportError(e, t('tokens.issue'));
    }
  };

  const revoke = async (tk) => {
    const powerful = tk.scopes.includes('tenant:admin') || tk.scopes.includes('node:admin');
    const ok = await confirmDialog(t('tokens.revokeTitle', { name: tk.name }), t('tokens.revokeBody'), {
      ok: t('tokens.revoke'),
      consequences: [t('tokens.revokeC1'), t('tokens.revokeC2')],
      requireText: powerful ? tk.name : undefined,
    });
    if (!ok) return;
    try {
      await tokens.remove(tk.id);
      forgetNames('token');
      toast(t('tokens.revoked', { name: tk.name }), 'ok');
      refresh();
    } catch (e) {
      if (e.status === 409) toast(t('tokens.lastAdmin'), 'warn', 9000);
      else reportError(e, t('tokens.revoke'));
    }
  };

  const roleBadge = (x) => {
    const r = roleOf(x.scopes);
    return h('div', { class: 'row' },
      badge(r === 'custom' ? t('tokens.roleCustomShort') : t(`tokens.role.${r}`), r === 'admin' ? 'warn' : r === 'custom' ? 'outline' : 'accent'),
      x.scopes.includes('node:admin') ? badge(t('tokens.nodeOps'), 'warn') : null);
  };

  const expiryCell = (x) => {
    if (!x.expires_at) return h('span', { class: 'muted' }, t('common.unlimited'));
    if (expired(x)) return h('span', { class: 'row' }, badge(t('tokens.expired'), 'outline', { dot: true }), h('span', { class: 'small muted', title: fmtTime(x.expires_at) }, fmtRelative(x.expires_at)));
    return h('span', { class: 'row', title: fmtTime(x.expires_at) }, expiringSoon(x) ? badge(fmtRelative(x.expires_at), 'warn', { icon: 'clock' }) : fmtRelative(x.expires_at));
  };

  const draw = () => {
    const rows = all.filter((x) => (filter === 'expired' ? expired(x) : filter === 'expiring' ? expiringSoon(x) : true))
      .filter((x) => !query || `${x.name} ${x.scopes.join(' ')} ${x.id}`.toLowerCase().includes(query.toLowerCase()));
    return dataTable({
      caption: t('tokens.caption'),
      testid: 'tokens-table',
      sort: { key: 'created', dir: 'desc' },
      rowKey: (x) => x.id,
      columns: [
        { key: 'name', label: t('common.name'), primary: true, sort: (x) => x.name, render: (x) => h('span', { class: 'token-name' }, icon('key'), h('strong', null, x.name)) },
        { key: 'role', label: t('tokens.col.role'), sort: (x) => roleOf(x.scopes), render: roleBadge },
        { label: t('tokens.scopes'), wrap: true, render: (x) => h('div', { class: 'scope-chips' }, x.scopes.map((s) => h('code', { title: t(`tokens.scope.${s}`) }, s))) },
        { key: 'created', label: t('common.created'), sort: (x) => x.created_at, render: (x) => h('span', { title: fmtTime(x.created_at) }, fmtRelative(x.created_at)) },
        { key: 'expires', label: t('tokens.col.expires'), sort: (x) => x.expires_at || '9999', render: expiryCell },
        { label: t('common.actions'), actions: true, render: (x) => button(t('tokens.revoke'), { size: 'sm', variant: 'danger', icon: 'trash', testid: 'token-revoke', attrs: { 'aria-label': t('tokens.revokeLabel', { name: x.name }) }, onClick: () => revoke(x) }) },
      ],
      rows,
      empty: { icon: 'filter', title: t('tokens.noMatch'), body: t('tokens.noMatchBody') },
    });
  };

  const toolbar = h('div', { class: 'table-toolbar' });
  const drawToolbar = () => {
    const n = (fn) => all.filter(fn).length;
    replace(toolbar,
      searchBox({ placeholder: t('tokens.search'), value: query, shortcut: true, testid: 'tokens-search', onInput: debounce((v) => { query = v.trim(); replace(box, draw()); }, 150) }),
      segmented({
        label: t('tokens.filterLabel'), value: filter, size: 'sm', testid: 'tokens-filter',
        options: [['', `${t('common.all')} ${all.length}`], ['expiring', `${t('tokens.filter.expiring')} ${n(expiringSoon)}`], ['expired', `${t('tokens.filter.expired')} ${n(expired)}`]],
        onChange: (v) => { filter = v; replace(box, draw()); },
      }));
  };

  const refresh = () => load(box, async () => {
    all = await listAll(tokens.list);
    if (!all.length) {
      toolbar.hidden = true;
      return emptyState({ icon: 'key', title: t('tokens.emptyTitle'), body: t('tokens.emptyBody'), actions: [button(t('tokens.issue'), { icon: 'plus', onClick: issue })] });
    }
    toolbar.hidden = false;
    drawToolbar();
    return draw();
  }, { kind: 'table', title: t('tokens.loadFailed') });

  root.append(
    pageHeader({
      title: t('nav.tokens'),
      description: t('tokens.lead'),
      help: helpHint({ title: t('nav.tokens'), body: t('tokens.help'), guide: '12-security.md' }),
      actions: [
        button(t('tokens.issue'), { variant: 'primary', icon: 'plus', kbd: 'N', testid: 'tokens-create', onClick: issue }),
      ],
    }),
    panel({ testid: 'tokens-panel' }, toolbar, box));
  await refresh();
  if (q0.get('new') === '1') {
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/tokens`);
    issue();
  }
  const offs = [onPageKey('n', issue), listNav(box, { rows: () => [...box.querySelectorAll('tbody tr')], open: (tr) => { const b = tr.querySelector('[data-testid="token-revoke"]'); if (b) b.focus(); } })];
  return () => offs.forEach((f) => f());
}

