// ログイン: API トークンを貼り、GET /capabilities で確かめてから sessionStorage に置く。

import { h, replace } from '../lib/dom.js';
import { setToken } from '../api/client.js';
import { system } from '../api/resources.js';
import { describeError } from '../lib/ui.js';
import { t, getLang, setLang, LANGS } from '../i18n/index.js';
import { getThemePref, setThemePref } from '../lib/theme.js';
import { formField, segmented, helpHint } from '../ui/index.js';
import { alert } from '../ui/feedback.js';
import { icon } from '../lib/icons.js';

export function renderLogin(root, { message, onLogin }) {
  const input = h('input', {
    type: 'password', name: 'token', autocomplete: 'off', spellcheck: 'false', placeholder: 'senca_…', 'data-testid': 'login-token',
  });
  const err = h('div', { 'aria-live': 'assertive' });
  const btn = h('button', { type: 'submit', class: 'primary lg block', 'data-testid': 'login-submit' }, icon('login'), h('span', null, t('login.submit')));
  const show = h('input', { type: 'checkbox' });
  show.addEventListener('change', () => { input.type = show.checked ? 'text' : 'password'; });
  const tokenField = formField({
    label: t('login.token'),
    input,
    required: true,
    help: helpHint({ title: t('login.tokenHelpTitle'), body: t('login.tokenHelpBody'), guide: '02-quickstart.md' }),
    hint: [t('login.hintPrefix'), h('code', null, '{data-dir}/control/bootstrap-admin-token'), t('login.hintSuffix')],
  });

  const form = h('form', { class: 'form', novalidate: true },
    tokenField,
    h('label', { class: 'check small' }, show, t('login.showToken')),
    err,
    btn);

  form.addEventListener('submit', async (e) => {
    e.preventDefault();
    const token = input.value.trim();
    replace(err);
    tokenField.setError(null);
    if (!token) {
      tokenField.setError(t('login.empty'));
      input.focus();
      return;
    }
    btn.disabled = true;
    btn.setAttribute('aria-busy', 'true');
    btn.querySelector('span').textContent = t('login.checking');
    try {
      const { data } = await system.capabilities({ token, quiet401: true });
      setToken(token);
      input.value = '';
      onLogin(data);
    } catch (ex) {
      const text = ex.status === 401 ? t('login.invalid') : describeError(ex);
      replace(err, alert('error', [text, ex.requestId ? h('div', { class: 'meta' }, `request_id: ${ex.requestId}`) : null], { role: 'alert' }));
      input.focus();
    } finally {
      btn.disabled = false;
      btn.removeAttribute('aria-busy');
      const span = btn.querySelector('span');
      if (span) span.textContent = t('login.submit');
    }
  });

  replace(root,
    h('div', { class: 'login-wrap' },
      h('main', { id: 'main', class: 'login', tabindex: '-1' },
        h('div', { class: 'panel' },
          h('div', { class: 'brand' },
            h('span', { class: 'brand-mark', 'aria-hidden': 'true' }, 'S'),
            h('span', { class: 'brand-text' }, h('strong', null, 'SENCA'), h('small', null, t('shell.product')))),
          message ? alert('info', message) : null,
          h('h1', null, t('login.title')),
          h('p', { class: 'login-sub' }, t('login.lead')),
          form),
        h('div', { class: 'login-tools' },
          segmented({ label: t('shell.language'), size: 'sm', options: LANGS.map(([c, n]) => [c, n]), value: getLang(), onChange: (v) => setLang(v), testid: 'login-lang' }),
          segmented({ label: t('shell.theme'), size: 'sm', iconOnly: true, options: [['system', t('theme.system'), 'monitor'], ['light', t('theme.light'), 'sun'], ['dark', t('theme.dark'), 'moon']], value: getThemePref(), onChange: (v) => setThemePref(v) })))));
  input.focus();
}
