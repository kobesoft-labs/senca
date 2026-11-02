// ログイン: API トークンを貼り、GET /capabilities で確かめてから sessionStorage に置く。

import { h, replace, field } from '../lib/dom.js';
import { setToken } from '../api/client.js';
import { system } from '../api/resources.js';
import { describeError } from '../lib/ui.js';

export function renderLogin(root, { message, onLogin }) {
  const input = h('input', {
    type: 'password', name: 'token', autocomplete: 'off', spellcheck: 'false', required: true,
    placeholder: 'senca_…', 'aria-describedby': 'token-hint',
  });
  const err = h('div', { role: 'alert', 'aria-live': 'assertive' });
  const btn = h('button', { type: 'submit', class: 'primary' }, 'ログイン');
  const show = h('input', { type: 'checkbox' });
  show.addEventListener('change', () => { input.type = show.checked ? 'text' : 'password'; });

  const form = h('form', { class: 'form', novalidate: true },
    field('API トークン', input),
    h('div', { class: 'hint', id: 'token-hint' },
      'POST /api/v1/tokens で発行したトークン、または初回起動時の ', h('code', null, '{data-dir}/control/bootstrap-admin-token'),
      '。このタブを閉じると消えます（sessionStorage）。'),
    h('label', { class: 'check' }, show, 'トークンを表示する'),
    err,
    h('div', { class: 'actions' }, btn));

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const token = input.value.trim();
    replace(err);
    if (!token) {
      replace(err, h('div', { class: 'field-error' }, 'トークンを入れてください'));
      input.focus();
      return;
    }
    btn.disabled = true;
    btn.textContent = '確認中…';
    try {
      const { data } = await system.capabilities({ token, quiet401: true });
      setToken(token);
      input.value = '';
      onLogin(data);
    } catch (ex) {
      const text = ex.status === 401 ? 'トークンが正しくないか、期限切れ・失効しています' : describeError(ex);
      replace(err, h('div', { class: 'alert error' }, text, ex.requestId ? h('div', { class: 'meta' }, `request_id: ${ex.requestId}`) : null));
      input.focus();
    } finally {
      btn.disabled = false;
      btn.textContent = 'ログイン';
    }
  });

  replace(root,
    h('main', { id: 'main', class: 'login' },
      h('div', { class: 'panel' },
        h('div', { class: 'brand' }, 'SENCA', h('small', null, 'メディアサーバー 管理画面')),
        message ? h('div', { class: 'alert info' }, message) : null,
        h('h1', null, 'ログイン'),
        form)));
  input.focus();
}
