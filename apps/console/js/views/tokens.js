// API トークンの一覧・発行（値は 1 回だけ表示）・失効。tenant:admin が要る。

import { h, replace, table, badge, field, checkbox } from '../lib/dom.js';
import { load, modal, confirmDialog, reportError, errorBox, secretDialog, toast } from '../lib/ui.js';
import { tokens, SCOPES } from '../api/resources.js';
import { fmtTime, fmtRelative, wallToUtcIso, localZone, idemKey } from '../lib/format.js';

const PAGE = 100;

async function createDialog() {
  const name = h('input', { type: 'text', required: true, maxlength: 64, placeholder: '例: obs-studio-a' });
  const checks = SCOPES.map(([v, label]) => checkbox(h('span', null, h('code', null, v), ' ', h('span', { class: 'muted small' }, label)), v === 'stream:read', { value: v }));
  const expires = h('input', { type: 'datetime-local' });
  const errBox = h('div');
  const key = idemKey();
  return modal('API トークンを発行', () => h('div', { class: 'form' },
    field('名前', name),
    h('fieldset', null, h('legend', null, '権限'), h('div', { style: { display: 'grid', gap: '4px' } }, checks.map((c) => c.el)),
      h('div', { class: 'hint' }, '自分が持っていない権限は渡せません（403）。必要な最小限にしてください。')),
    field(`期限（${localZone()}、空なら無期限）`, expires),
    errBox), {
    actions: (close) => {
      const ok = h('button', { class: 'primary', onClick: async () => {
        const scopes = checks.filter((c) => c.input.checked).map((c) => c.input.value);
        const problems = [];
        if (!name.value.trim()) problems.push('名前を入れてください');
        if (!scopes.length) problems.push('権限を 1 つ以上選んでください');
        const exp = expires.value ? wallToUtcIso(expires.value, localZone()) : null;
        if (exp && new Date(exp) <= new Date()) problems.push('期限は未来にしてください');
        if (problems.length) {
          replace(errBox, h('div', { class: 'alert error', role: 'alert' }, problems.join('。')));
          return;
        }
        ok.disabled = true;
        try {
          const { data } = await tokens.create({ name: name.value.trim(), scopes, expires_at: exp }, key);
          close(data);
        } catch (e) {
          replace(errBox, errorBox(e, { title: '発行できませんでした' }));
        } finally {
          ok.disabled = false;
        }
      } }, '発行');
      return [h('button', { onClick: () => close(null) }, 'やめる'), ok];
    },
  });
}

export async function render(root) {
  const box = h('div');
  const pager = h('div', { class: 'pager' });
  const cursors = [undefined];
  let page = 0;

  const refresh = () => load(box, async () => {
    const { data } = await tokens.list({ limit: PAGE, cursor: cursors[page] });
    cursors[page + 1] = data.next_cursor || null;
    replace(pager,
      h('button', { class: 'small', disabled: page === 0, onClick: () => { page -= 1; refresh(); } }, '← 前へ'),
      h('button', { class: 'small', disabled: !data.next_cursor, onClick: () => { page += 1; refresh(); } }, '次へ →'));
    return table([
      { label: '名前', render: (t) => t.name },
      { label: '権限', render: (t) => h('div', { class: 'row' }, t.scopes.map((s) => badge(s, s.endsWith('admin') ? 'warn' : ''))) },
      { label: '作成', render: (t) => fmtTime(t.created_at) },
      { label: '期限', render: (t) => (t.expires_at
        ? [fmtTime(t.expires_at), ' ', new Date(t.expires_at) < new Date() ? badge('期限切れ', 'danger') : h('span', { class: 'muted small' }, fmtRelative(t.expires_at))]
        : '無期限') },
      { label: 'ID', render: (t) => h('code', { class: 'muted' }, t.id.slice(0, 10)) },
      { label: '', render: (t) => h('button', { class: 'small danger', 'aria-label': `${t.name} を失効`, onClick: async () => {
        if (!(await confirmDialog('トークンを失効', `「${t.name}」を失効します。このトークンを使っている道具は使えなくなります。今ログインに使っているトークンなら、ログイン画面へ戻ります。`, { ok: '失効' }))) return;
        try {
          await tokens.remove(t.id);
          toast('失効しました', 'ok');
          refresh();
        } catch (e) {
          reportError(e, '失効');
        }
      } }, '失効') },
    ], data.items, { empty: 'トークンはありません', caption: 'API トークンの一覧' });
  });

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, 'API トークン'), h('span', { class: 'spacer' }),
      h('button', { onClick: () => refresh() }, '再読み込み'),
      h('button', { class: 'primary', onClick: async () => {
        try {
          const t = await createDialog();
          if (!t) return;
          if (t.token) await secretDialog('トークンを発行しました', `「${t.name}」のトークン`, t.token, 'このトークンはもう表示されません。今すぐ安全な場所に控えてください。');
          else toast('発行しましたが、値を受け取れませんでした（再送の応答）。必要なら失効して発行し直してください', 'warn', 9000);
          refresh();
        } catch (e) {
          reportError(e, '発行');
        }
      } }, '＋ トークンを発行')),
    h('section', { class: 'panel' }, box, pager));
  await refresh();
  return null;
}
