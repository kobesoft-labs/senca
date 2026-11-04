// 使っている途中でセッションが切れたとき（401）の「もう一度ログイン」のダイアログ。
// 今の画面はそのまま残し、入り直せたら待っていた要求を送り直す（js/api/client.js の setReauthHandler）。
// 同じ利用者で入り直す（利用者名は変えられない）。別の人で入るときは「ログアウト」からログインの画面へ。

import { h, replace, nextId } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { modal, alert } from '../ui/index.js';
import { describeError } from '../lib/ui.js';
import { icon } from '../lib/icons.js';
import { setToken } from '../api/client.js';
import { auth } from '../api/auth.js';
import { signInMethod, displayName, returnToFor, rememberProvider, setMe } from '../lib/session.js';
import { passwordInput, otpInput, providerIcon, countdown } from './auth-parts.js';
import { meFromLogin } from './login.js';

/**
 * @param {{me: object}} o 切れる前の主体
 * @returns {Promise<boolean>} 入り直せたら true、ログアウトを選んだら false
 */
export function reauthDialog({ me }) {
  const method = signInMethod(me);
  const user = me.user || null;
  let submit = async () => {};
  let okBtn = null;
  let stopTimer = null;
  const body = h('div', { class: 'reauth', 'data-testid': 'reauth' });
  const err = h('div', { class: 'auth-error', 'aria-live': 'assertive', 'data-testid': 'reauth-error' });
  const showErr = (msg) => replace(err, msg ? alert('error', msg, { role: 'alert', small: true }) : null);

  let finish = () => {};
  const done = (data) => {
    setToken(data.token);
    setMe(meFromLogin(data));
    finish(true);
  };

  const throttled = (e) => {
    const time = h('strong', { class: 'num' });
    replace(err, alert('warn', [t('auth.throttle.wait'), ' ', time], { title: t('auth.throttle.title'), role: 'alert', small: true }));
    if (okBtn) okBtn.disabled = true;
    if (stopTimer) stopTimer();
    stopTimer = countdown(time, Math.max(1, Math.ceil(e.retryAfter ?? 30)), () => { if (okBtn) okBtn.disabled = false; replace(err); });
  };

  const who = h('div', { class: 'auth-who' }, icon('user', { size: 'sm' }), h('span', { class: 'truncate' }, displayName(me)),
    user && user.username !== displayName(me) ? h('span', { class: 'subtle small' }, user.username) : null);

  const passwordStep = () => {
    const pw = passwordInput({ autocomplete: 'current-password', testid: 'reauth-password' });
    const id = nextId('f');
    pw.input.id = id;
    replace(body,
      h('p', null, t('auth.reauth.lead')),
      who,
      h('form', { class: 'form', novalidate: true, onSubmit: (e) => { e.preventDefault(); submit(); } },
        h('input', { type: 'text', autocomplete: 'username', value: user.username, hidden: true, readonly: true, 'aria-hidden': 'true', tabindex: '-1' }),
        h('div', { class: 'field' }, h('label', { for: id }, t('auth.password')), pw.el, pw.caps)),
      err);
    submit = async () => {
      showErr(null);
      if (!pw.input.value) { pw.input.focus(); return; }
      okBtn.disabled = true;
      try {
        const { data } = await auth.login({ username: user.username, password: pw.input.value, tenant: me.tenant !== 'default' ? me.tenant : undefined });
        if (data.status === 'mfa_required') mfaStep(data);
        else done(data);
      } catch (e) {
        if (e.status === 429) { throttled(e); return; }
        showErr(e.status === 401 ? t('auth.reauth.wrongPassword') : describeError(e));
        pw.input.select();
        pw.input.focus();
      } finally {
        if (!stopTimer) okBtn.disabled = false;
      }
    };
    setTimeout(() => pw.input.focus(), 0);
  };

  const mfaStep = (st) => {
    let recovery = false;
    const rec = h('input', { type: 'text', autocomplete: 'off', class: 'mono', placeholder: 'xxxx-xxxx-xxxx-xxxx', 'aria-label': t('auth.mfa.recoveryLabel'), 'data-testid': 'reauth-recovery', hidden: true });
    const otp = otpInput({ label: t('auth.mfa.codeLabel'), testid: 'reauth-code', onComplete: () => submit() });
    const toggle = h('button', { type: 'button', class: 'link small', 'data-testid': 'reauth-use-recovery' }, t('auth.mfa.useRecovery'));
    toggle.addEventListener('click', () => {
      recovery = !recovery;
      otp.hidden = recovery;
      rec.hidden = !recovery;
      toggle.textContent = recovery ? t('auth.mfa.useApp') : t('auth.mfa.useRecovery');
      (recovery ? rec : otp.input).focus();
    });
    replace(body, h('p', null, t('auth.mfa.lead')), who, h('form', { class: 'form', novalidate: true, onSubmit: (e) => { e.preventDefault(); submit(); } }, otp, rec), h('div', null, toggle), err);
    submit = async () => {
      showErr(null);
      okBtn.disabled = true;
      try {
        const { data } = await auth.loginMfa({ mfaToken: st.mfa_token, code: recovery ? null : otp.input.value, recoveryCode: recovery ? rec.value.trim() : null });
        done(data);
      } catch (e) {
        if (e.status === 429) { throttled(e); return; }
        showErr(e.status === 401 ? t('auth.mfa.invalid') : describeError(e));
        otp.clear();
        (recovery ? rec : otp.input).focus();
      } finally {
        if (!stopTimer) okBtn.disabled = false;
      }
    };
    setTimeout(() => otp.input.focus(), 0);
  };

  const ssoStep = async () => {
    replace(body, h('p', null, t('auth.reauth.leadSso')), who, err);
    let p = null;
    try {
      const { data } = await auth.providers();
      p = data.oidc.find((o) => o.id === method.provider) || null;
    } catch { /* 下で知らせる */ }
    if (!p) { showErr(t('auth.reauth.ssoMissing')); okBtn.disabled = true; return; }
    replace(okBtn, providerIcon(p), h('span', { class: 'btn-label' }, t('auth.signIn.sso', { name: p.display_name || p.id })));
    body.append(alert('info', t('auth.reauth.ssoNote'), { small: true }));
    submit = async () => {
      rememberProvider(p.id);
      location.assign(`${p.start_url}?return_to=${encodeURIComponent(returnToFor())}`);
    };
  };

  const tokenStep = () => {
    const tok = passwordInput({ autocomplete: 'off', testid: 'reauth-token', placeholder: 'senca_…' });
    const id = nextId('f');
    tok.input.id = id;
    replace(body, h('p', null, t('auth.reauth.leadToken')),
      h('form', { class: 'form', novalidate: true, onSubmit: (e) => { e.preventDefault(); submit(); } },
        h('div', { class: 'field' }, h('label', { for: id }, t('login.token')), tok.el)), err);
    submit = async () => {
      showErr(null);
      const token = tok.input.value.trim();
      if (!token) { tok.input.focus(); return; }
      okBtn.disabled = true;
      try {
        const { data } = await auth.me({ token, quiet401: true });
        if (data.tenant !== me.tenant) { showErr(t('auth.reauth.otherTenant')); return; }
        setToken(token);
        setMe(data);
        finish(true);
      } catch (e) {
        showErr(e.status === 401 ? t('login.invalid') : describeError(e));
      } finally {
        okBtn.disabled = false;
      }
    };
    setTimeout(() => tok.input.focus(), 0);
  };

  return modal(t('auth.reauth.title'), (close) => {
    finish = (v) => { if (stopTimer) stopTimer(); close(v); };
    return body;
  }, {
    icon: 'lock',
    kind: 'warn',
    testid: 'reauth-dialog',
    dismissible: false,
    actions: (close) => {
      okBtn = h('button', { type: 'button', class: 'primary', 'data-testid': 'reauth-submit', onClick: () => submit() }, t('auth.reauth.submit'));
      const out = h('button', { type: 'button', 'data-testid': 'reauth-logout', onClick: () => { if (stopTimer) stopTimer(); close(false); } }, t('shell.logout'));
      // 中身はボタンができてから作る（ステップがボタンの文字を変えるため）
      setTimeout(() => {
        if (method.key === 'sso') ssoStep();
        else if (method.key === 'apiToken' || method.key === 'jwt' || !user) tokenStep();
        else passwordStep();
      }, 0);
      return [out, okBtn];
    },
  }).then((v) => v === true);
}
