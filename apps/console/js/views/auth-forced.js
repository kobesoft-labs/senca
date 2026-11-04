// 先に済ませる手続き（セッションの pending）: 仮のパスワードの変更（password_change）と、認証アプリの登録（mfa_enrollment）。
// 済むまでほかの画面は出さない（ノードもほかの操作を 403 にする）。右上の「ログアウト」で抜けられる。
// ログインの直後なら、そのとき入れたパスワードを覚えておき、もう一度は聞かない（メモリの中だけ）。

import { h, replace, nextId } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { alert, button } from '../ui/index.js';
import { describeError } from '../lib/ui.js';
import { auth } from '../api/auth.js';
import { authFrame, cardHead, passwordInput, strengthMeter, mfaEnrollment } from './auth-parts.js';

const ORDER = ['password_change', 'mfa_enrollment'];

/**
 * @param {HTMLElement} root
 * @param {{me: object, password?: string|null, onDone: (me: object) => void, onLogout: () => void}} o
 */
export function renderForced(root, { me, password, onDone, onLogout }) {
  let known = password || null;
  const total = ORDER.filter((p) => (me.pending || []).includes(p)).length;
  const user = me.user || {};

  const logoutBtn = button(t('shell.logout'), { icon: 'logout', size: 'sm', variant: 'ghost', testid: 'forced-logout', onClick: onLogout });

  const progress = (step) => (total > 1 ? h('div', { class: 'auth-progress', 'data-testid': 'forced-progress' }, t('auth.forced.progress', { n: step, total })) : null);

  const next = async () => {
    let cur = me;
    try {
      ({ data: cur } = await auth.me());
    } catch (e) {
      if (e.status === 401) { onLogout(); return; }
    }
    me = cur;
    const todo = ORDER.find((p) => (cur.pending || []).includes(p));
    if (!todo) { onDone(cur); return; }
    const step = total - ORDER.filter((p) => (cur.pending || []).includes(p)).length + 1;
    if (todo === 'password_change') passwordStep(step);
    else mfaStep(step);
  };

  const passwordStep = (step, wrongCurrent = false) => {
    const cur = passwordInput({ autocomplete: 'current-password', testid: 'forced-current' });
    const pw = passwordInput({ autocomplete: 'new-password', testid: 'forced-new' });
    const pw2 = passwordInput({ autocomplete: 'new-password', testid: 'forced-new2' });
    const meter = strengthMeter(pw.input, () => ({ username: user.username, email: user.email }));
    const err = h('div', { class: 'auth-error', 'aria-live': 'assertive', 'data-testid': 'forced-error' },
      wrongCurrent ? alert('error', t('auth.forced.wrongCurrent'), { role: 'alert', small: true }) : null);
    const btn = h('button', { type: 'submit', class: 'primary lg block', 'data-testid': 'forced-submit' }, t('auth.forced.passwordSubmit'));
    const lab = (text, ctl, extra) => {
      const id = nextId('f');
      ctl.input.id = id;
      return h('div', { class: 'field' }, h('label', { for: id }, text), ctl.el, extra || null);
    };
    // ログインの直後なら今のパスワードは分かっているので、隠したユーザー名だけ置く（パスワードの管理ソフトのため）
    const form = h('form', { class: 'form auth-form', novalidate: true },
      h('input', { type: 'text', autocomplete: 'username', value: user.username || '', hidden: true, readonly: true, 'aria-hidden': 'true', tabindex: '-1' }),
      known && !wrongCurrent ? null : lab(t('auth.forced.temporary'), cur, cur.caps),
      lab(t('auth.newPassword'), pw, [pw.caps, meter]),
      lab(t('auth.confirmPassword'), pw2, pw2.caps),
      err, btn);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      replace(err);
      const current = known && !wrongCurrent ? known : cur.input.value;
      const problems = [];
      if (!current) problems.push(t('auth.forced.needCurrent'));
      if (!meter.result().ok) problems.push(t('auth.passwordRule'));
      if (pw.input.value !== pw2.input.value) problems.push(t('auth.passwordMismatch'));
      if (pw.input.value && pw.input.value === current) problems.push(t('auth.forced.sameAsOld'));
      if (problems.length) { replace(err, alert('error', h('ul', null, problems.map((x) => h('li', null, x))), { role: 'alert', small: true })); return; }
      btn.disabled = true;
      try {
        await auth.changePassword(current, pw.input.value);
        known = pw.input.value;
        await next();
      } catch (ex) {
        btn.disabled = false;
        if (ex.status === 403) { known = null; passwordStep(step, true); return; }
        replace(err, alert('error', describeError(ex), { role: 'alert', small: true }));
      }
    });
    authFrame(root, {
      testid: 'forced-page',
      extra: [logoutBtn],
      card: [
        progress(step),
        cardHead({ title: t('auth.forced.passwordTitle'), lead: t('auth.forced.passwordLead'), iconName: 'key', kind: 'accent' }),
        h('div', { class: 'auth-who' }, h('span', { class: 'truncate' }, user.display_name || user.username || '')),
        form,
      ],
    });
    const f = form.querySelector('input:not([hidden])');
    if (f) f.focus({ preventScroll: true });
  };

  const mfaStep = (step) => {
    authFrame(root, {
      testid: 'forced-page',
      wide: true,
      extra: [logoutBtn],
      card: [
        progress(step),
        cardHead({ title: t('auth.forced.mfaTitle'), lead: t('auth.forced.mfaLead'), iconName: 'shield-check', kind: 'accent' }),
        mfaEnrollment({ password: known, username: user.username, onDone: () => next() }),
      ],
    });
  };

  next();
}
