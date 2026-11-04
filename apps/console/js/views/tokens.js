// API トークンの一覧・発行（値は 1 回だけ表示）・失効。tenant:admin が要る。#/tokens?new=1 で発行のダイアログを開く。

import { h, replace, checkbox } from '../lib/dom.js';
import { load, modal, confirmDialog, reportError, errorBox, secretDialog, toast } from '../lib/ui.js';
import { tokens, SCOPES } from '../api/resources.js';
import { fmtTime, fmtRelative, wallToUtcIso, localZone, idemKey } from '../lib/format.js';
import { t } from '../i18n/index.js';
import { button, iconButton, pageHeader, dataTable, pagination, emptyState, badge, formField, helpHint, alert } from '../ui/index.js';
import { onPageKey } from '../shortcuts.js';

const PAGE = 100;

async function createDialog() {
  const name = h('input', { type: 'text', required: true, maxlength: 64, placeholder: t('tokens.namePlaceholder'), autocomplete: 'off' });
  const checks = SCOPES.map(([v]) => checkbox(h('span', null, h('code', null, v), ' ', h('span', { class: 'muted small' }, t(`tokens.scope.${v}`))), v === 'stream:read', { value: v }));
  const expires = h('input', { type: 'datetime-local' });
  const errBox = h('div');
  const key = idemKey();
  const scopeList = h('div', { class: 'stack' }, checks.map((c) => c.el));
  scopeList.style.gap = '4px';
  return modal(t('tokens.createTitle'), () => h('div', { class: 'form' },
    formField({ label: t('common.name'), input: name, required: true, hint: t('tokens.nameHint') }),
    h('fieldset', null, h('legend', null, t('tokens.scopes')), scopeList,
      h('div', { class: 'hint' }, t('tokens.scopesHint'))),
    formField({ label: t('tokens.expires', { zone: localZone() }), input: expires, optional: true, hint: t('tokens.expiresHint') }),
    errBox), {
    icon: 'key',
    testid: 'token-create-dialog',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', onClick: async () => {
        const scopes = checks.filter((c) => c.input.checked).map((c) => c.input.value);
        const problems = [];
        if (!name.value.trim()) problems.push(t('tokens.needName'));
        if (!scopes.length) problems.push(t('tokens.needScope'));
        const exp = expires.value ? wallToUtcIso(expires.value, localZone()) : null;
        if (exp && new Date(exp) <= new Date()) problems.push(t('tokens.needFuture'));
        if (problems.length) {
          replace(errBox, alert('error', h('ul', null, problems.map((p) => h('li', null, p))), { role: 'alert' }));
          return;
        }
        ok.disabled = true;
        try {
          const { data } = await tokens.create({ name: name.value.trim(), scopes, expires_at: exp }, key);
          close(data);
        } catch (e) {
          replace(errBox, errorBox(e, { title: t('tokens.createFailed') }));
        } finally {
          ok.disabled = false;
        }
      } }, t('tokens.issueOk'));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  });
}

export async function render(root) {
  const box = h('div');
  const pager = h('div');
  const cursors = [undefined];
  let page = 0;

  const issue = async () => {
    try {
      const tk = await createDialog();
      if (!tk) return;
      if (tk.token) await secretDialog(t('tokens.issuedTitle'), t('tokens.valueLabel', { name: tk.name }), tk.token, t('tokens.onceWarning'));
      else toast(t('tokens.noValue'), 'warn', 9000);
      refresh();
    } catch (e) {
      reportError(e, t('tokens.issue'));
    }
  };

  const revoke = async (tk) => {
    const ok = await confirmDialog(t('tokens.revokeTitle'), t('tokens.revokeBody', { name: tk.name }), {
      ok: t('tokens.revoke'),
      consequences: [t('tokens.revokeC1'), t('tokens.revokeC2')],
    });
    if (!ok) return;
    try {
      await tokens.remove(tk.id);
      toast(t('tokens.revoked'), 'ok');
      refresh();
    } catch (e) {
      reportError(e, t('tokens.revoke'));
    }
  };

  const refresh = () => load(box, async () => {
    const { data } = await tokens.list({ limit: PAGE, cursor: cursors[page] });
    cursors[page + 1] = data.next_cursor || null;
    replace(pager, pagination({
      page, hasPrev: page > 0, hasNext: !!data.next_cursor,
      onPrev: () => { page -= 1; refresh(); }, onNext: () => { page += 1; refresh(); },
    }));
    if (!data.items.length && page === 0) {
      return emptyState({ icon: 'key', title: t('tokens.emptyTitle'), body: t('tokens.emptyBody'), actions: [button(t('tokens.issue'), { variant: 'primary', icon: 'plus', onClick: issue })] });
    }
    return dataTable({
      caption: t('tokens.caption'),
      sort: { key: 'created', dir: 'desc' },
      rowKey: (x) => x.id,
      columns: [
        { key: 'name', label: t('common.name'), primary: true, sort: (x) => x.name, render: (x) => x.name },
        { label: t('tokens.scopes'), wrap: true, render: (x) => h('div', { class: 'row' }, x.scopes.map((s) => badge(s, s.endsWith('admin') ? 'warn' : null))) },
        { key: 'created', label: t('common.created'), sort: (x) => x.created_at, render: (x) => fmtTime(x.created_at) },
        { key: 'expires', label: t('tokens.col.expires'), sort: (x) => x.expires_at || '9999', render: (x) => (x.expires_at
          ? [fmtTime(x.expires_at), ' ', new Date(x.expires_at) < new Date() ? badge(t('tokens.expired'), 'danger') : h('span', { class: 'muted small' }, fmtRelative(x.expires_at))]
          : t('common.unlimited')) },
        { label: 'ID', render: (x) => h('code', { class: 'muted' }, x.id.slice(0, 10)) },
        { label: t('common.actions'), actions: true, render: (x) => button(t('tokens.revoke'), { size: 'sm', variant: 'danger', attrs: { 'aria-label': t('tokens.revokeLabel', { name: x.name }) }, onClick: () => revoke(x) }) },
      ],
      rows: data.items,
    });
  }, { kind: 'table' });

  root.append(
    pageHeader({
      title: t('nav.tokens'),
      description: t('tokens.lead'),
      help: helpHint({ title: t('nav.tokens'), body: t('tokens.help'), guide: '12-security.md' }),
      actions: [
        iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => refresh() }),
        button(t('tokens.issue'), { variant: 'primary', icon: 'plus', kbd: 'N', testid: 'tokens-create', onClick: issue }),
      ],
    }),
    h('section', { class: 'panel' }, box, pager));
  await refresh();
  if (new URLSearchParams(location.hash.split('?')[1] || '').get('new') === '1') {
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/tokens`);
    issue();
  }
  return onPageKey('n', issue);
}
