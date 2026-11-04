// ログインの画面。GET /auth/providers に合わせて出し分ける:
// - シングルサインオン（IdP ごとのボタン）・利用者名とパスワード・2 段階認証（6 桁のコード・回復コード）
// - API トークン（と JWT）を貼り付けるログイン（控えめな別の入口。トークンは sessionStorage にだけ置く）
// - 利用者がまだいない（setup_required）: 最初の管理者を作る（最初の管理者トークンで POST /users → そのままログイン）
// - OIDC から戻ったところ（#/auth/complete?code=… / ?error=…）: コードを替える・失敗の説明と再試行
// 行き先（ハッシュ）は変えないので、ログインした後は開こうとしていたページが出る。OIDC は return_to で戻す。

import { h, replace, nextId } from '../lib/dom.js';
import { setToken } from '../api/client.js';
import { system } from '../api/resources.js';
import { auth, users } from '../api/auth.js';
import { describeError } from '../lib/ui.js';
import { t } from '../i18n/index.js';
import { alert, button, loadingLine, skeleton } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { authFrame, cardHead, passwordInput, strengthMeter, otpInput, providerIcon, countdown } from './auth-parts.js';
import { returnToFor, rememberProvider, takeProvider, hashFromReturnTo } from '../lib/session.js';

const TENANT_KEY = 'senca.console.tenant';
const storedTenant = () => { try { return localStorage.getItem(TENANT_KEY) || ''; } catch { return ''; } };

/** ログインの応答（status: ok）から、/auth/me と同じ形を作る。 */
export function meFromLogin(r) {
  return { tenant: r.tenant, subject: `user:${r.user.id}`, auth_method: 'session', scopes: r.scopes, user: r.user, session: r.session, pending: r.pending || [] };
}

/** 入力欄とラベル（formField より簡単な、ログインの画面用）。 */
function fieldOf(label, control, { hint, extra, id, labelExtra } = {}) {
  const input = control.input || control;
  if (!input.id) input.id = id || nextId('f');
  const hintEl = hint ? h('div', { class: 'hint', id: `${input.id}-hint` }, hint) : null;
  if (hintEl) input.setAttribute('aria-describedby', hintEl.id);
  return h('div', { class: 'field' },
    labelExtra ? h('div', { class: 'field-label-row' }, h('label', { for: input.id }, label), labelExtra) : h('label', { for: input.id }, label),
    control.el || control, extra || null, hintEl);
}

function submitButton(label, testid) {
  return h('button', { type: 'submit', class: 'primary lg block', 'data-testid': testid }, h('span', { class: 'btn-label' }, label));
}

function busy(btn, on, label) {
  btn.disabled = on;
  if (on) btn.setAttribute('aria-busy', 'true');
  else btn.removeAttribute('aria-busy');
  replace(btn, on ? icon('spinner', { spin: true }) : null, h('span', { class: 'btn-label' }, label));
}

/**
 * @param {HTMLElement} root
 * @param {{message?: string, messageKind?: string, onLogin: (r: {me: object, password?: string}) => void}} o
 */
export function renderLogin(root, { message, messageKind = 'info', onLogin }) {
  let providers = null;
  let stopTimer = null;
  const stop = () => { if (stopTimer) { stopTimer(); stopTimer = null; } };
  const messageBox = () => (message ? alert(messageKind, message, { testid: 'login-message' }) : null);

  const frame = (card, opts = {}) => {
    stop();
    authFrame(root, { card, ...opts });
    const first = root.querySelector('.auth-card [autofocus], .auth-card input:not([type=hidden]):not([readonly]):not([type=checkbox])');
    if (first) first.focus({ preventScroll: true });
  };

  // ------------------------------------------------------------ ログインできた
  const finishSession = (r, password) => {
    setToken(r.token);
    try {
      const tn = r.tenant && r.tenant !== 'default' ? r.tenant : '';
      if (tn) localStorage.setItem(TENANT_KEY, tn); else localStorage.removeItem(TENANT_KEY);
    } catch { /* 残せなくてもよい */ }
    onLogin({ me: meFromLogin(r), password });
  };

  /** 429 の待ち時間を出し、終わるまで押せなくする。 */
  const throttled = (box, btn, e, label) => {
    const secs = Math.max(1, Math.ceil(e.retryAfter ?? 30));
    const time = h('strong', { class: 'num', 'data-testid': 'login-countdown' });
    const which = e.details && e.details.limit === 'login_account' ? t('auth.throttle.account') : t('auth.throttle.ip');
    replace(box, alert('warn', [which, h('br'), t('auth.throttle.wait'), ' ', time], { title: t('auth.throttle.title'), role: 'alert', testid: 'login-throttled' }));
    btn.disabled = true;
    stop();
    stopTimer = countdown(time, secs, () => {
      stopTimer = null;
      btn.disabled = false;
      replace(box, alert('info', t('auth.throttle.done'), { small: true }));
    });
  };

  // ------------------------------------------------------------ 利用者名とパスワード（と SSO）
  const signIn = () => {
    const p = providers;
    const parts = [cardHead({ title: t('auth.signIn.title'), lead: t('auth.signIn.lead') }), messageBox()];
    if (p.oidc.length) {
      parts.push(h('div', { class: 'sso-list' }, p.oidc.map((o) => h('a', {
        class: 'btn lg block sso-btn', href: `${o.start_url}?return_to=${encodeURIComponent(returnToFor())}`, 'data-testid': `sso-${o.id}`,
        onClick: () => rememberProvider(o.id),
      }, providerIcon(o), h('span', { class: 'btn-label' }, t('auth.signIn.sso', { name: o.display_name || o.id }))))));
      if (p.password) parts.push(h('div', { class: 'auth-divider', role: 'separator' }, h('span', null, t('auth.signIn.or'))));
    }
    if (p.password) {
      const user = h('input', { type: 'text', name: 'username', autocomplete: 'username', autocapitalize: 'off', spellcheck: 'false', required: true, 'data-testid': 'login-username', maxlength: '256' });
      const pw = passwordInput({ autocomplete: 'current-password', testid: 'login-password' });
      const tenant = h('input', { type: 'text', name: 'tenant', autocomplete: 'organization', autocapitalize: 'off', spellcheck: 'false', value: storedTenant(), placeholder: 'default', 'data-testid': 'login-tenant', maxlength: '64' });
      const tenantField = fieldOf(t('auth.signIn.tenant'), tenant, { hint: t('auth.signIn.tenantHint') });
      tenantField.hidden = !storedTenant();
      const tenantToggle = h('button', { type: 'button', class: 'link small', 'aria-expanded': String(!tenantField.hidden), 'data-testid': 'login-tenant-toggle' }, t('auth.signIn.otherTenant'));
      tenantToggle.addEventListener('click', () => {
        tenantField.hidden = !tenantField.hidden;
        tenantToggle.setAttribute('aria-expanded', String(!tenantField.hidden));
        if (!tenantField.hidden) tenant.focus();
      });
      const err = h('div', { class: 'auth-error', 'aria-live': 'assertive', 'data-testid': 'login-error' });
      const btn = submitButton(t('auth.signIn.submit'), 'login-submit-password');
      const form = h('form', { class: 'form auth-form', novalidate: true, 'data-testid': 'login-form' },
        fieldOf(t('auth.username'), user),
        fieldOf(t('auth.password'), pw, { extra: pw.caps }),
        tenantField,
        err,
        btn,
        h('div', { class: 'auth-row' }, tenantToggle));
      form.addEventListener('submit', async (e) => {
        e.preventDefault();
        replace(err);
        const username = user.value.trim();
        if (!username || !pw.input.value) {
          replace(err, alert('error', t('auth.signIn.needBoth'), { role: 'alert', small: true }));
          (username ? pw.input : user).focus();
          return;
        }
        busy(btn, true, t('auth.signIn.checking'));
        try {
          const { data } = await auth.login({ username, password: pw.input.value, tenant: tenant.value.trim() || undefined });
          if (data.status === 'mfa_required') mfa({ ...data, username, password: pw.input.value });
          else finishSession(data, pw.input.value);
        } catch (ex) {
          busy(btn, false, t('auth.signIn.submit'));
          if (ex.status === 429) { throttled(err, btn, ex); return; }
          const text = ex.status === 401 ? t('auth.signIn.invalid') : ex.status === 403 ? t('auth.signIn.passwordOff') : describeError(ex);
          replace(err, alert('error', text, { role: 'alert', small: true }));
          pw.input.select();
          pw.input.focus();
        }
      });
      parts.push(form);
    }
    parts.push(h('div', { class: 'auth-alt' },
      h('button', { type: 'button', class: 'link', 'data-testid': 'login-use-token', onClick: () => tokenView() }, icon('key', { size: 'sm' }), t('auth.token.link'))));
    frame(parts);
    if (!p.password && p.oidc.length) root.querySelector('.sso-btn').focus({ preventScroll: true });
  };

  // ------------------------------------------------------------ 2 段階認証
  const mfa = (st) => {
    let mode = 'totp';
    const err = h('div', { class: 'auth-error', 'aria-live': 'assertive', 'data-testid': 'mfa-error' });
    const btn = submitButton(t('auth.mfa.verify'), 'mfa-submit');
    const hintId = nextId('mfa-hint');
    let sending = false;
    const expired = () => st.expires_at && Date.now() > new Date(st.expires_at).getTime() - 2000;
    const send = async () => {
      if (sending) return;
      replace(err);
      if (expired()) { message = t('auth.mfa.expired'); messageKind = 'warn'; signIn(); return; }
      const code = mode === 'totp' ? otp.input.value : null;
      const recoveryCode = mode === 'recovery' ? recovery.value.trim() : null;
      if (mode === 'totp' && code.length !== 6) { otp.setInvalid(true); replace(err, alert('error', t('auth.mfa.needCode'), { role: 'alert', small: true })); otp.input.focus(); return; }
      if (mode === 'recovery' && !recoveryCode) { replace(err, alert('error', t('auth.mfa.needRecovery'), { role: 'alert', small: true })); recovery.focus(); return; }
      sending = true;
      busy(btn, true, t('auth.signIn.checking'));
      try {
        const { data } = await auth.loginMfa({ mfaToken: st.mfa_token, code, recoveryCode });
        finishSession(data, st.password);
      } catch (ex) {
        busy(btn, false, t('auth.mfa.verify'));
        if (ex.status === 429) { throttled(err, btn, ex); return; }
        if (ex.status === 401 && expired()) { message = t('auth.mfa.expired'); messageKind = 'warn'; signIn(); return; }
        replace(err, alert('error', ex.status === 401 ? (mode === 'totp' ? t('auth.mfa.invalid') : t('auth.mfa.invalidRecovery')) : describeError(ex), { role: 'alert', small: true }));
        if (mode === 'totp') { otp.setInvalid(true); otp.clear(); otp.input.focus(); } else { recovery.select(); recovery.focus(); }
      } finally {
        sending = false;
      }
    };
    const otp = otpInput({ label: t('auth.mfa.codeLabel'), testid: 'mfa-code', onComplete: () => send(), describedBy: hintId });
    const recovery = h('input', { type: 'text', autocomplete: 'off', autocapitalize: 'off', spellcheck: 'false', placeholder: 'xxxx-xxxx-xxxx-xxxx', 'data-testid': 'mfa-recovery', class: 'mono', maxlength: '64' });
    const totpPane = h('div', { class: 'mfa-pane' }, h('p', { class: 'muted small', id: hintId }, t('auth.mfa.lead')), otp);
    const recPane = h('div', { class: 'mfa-pane', hidden: true }, fieldOf(t('auth.mfa.recoveryLabel'), recovery, { hint: t('auth.mfa.recoveryHint') }));
    const toggle = h('button', { type: 'button', class: 'link small', 'data-testid': 'mfa-use-recovery' }, t('auth.mfa.useRecovery'));
    toggle.addEventListener('click', () => {
      mode = mode === 'totp' ? 'recovery' : 'totp';
      totpPane.hidden = mode !== 'totp';
      recPane.hidden = mode !== 'recovery';
      replace(err);
      toggle.textContent = mode === 'totp' ? t('auth.mfa.useRecovery') : t('auth.mfa.useApp');
      (mode === 'totp' ? otp.input : recovery).focus();
    });
    const form = h('form', { class: 'form auth-form', novalidate: true, 'data-testid': 'mfa-form' }, totpPane, recPane, err, btn,
      h('div', { class: 'auth-row' }, toggle));
    form.addEventListener('submit', (e) => { e.preventDefault(); send(); });
    frame([
      cardHead({ title: t('auth.mfa.title'), iconName: 'shield-check', kind: 'accent' }),
      h('div', { class: 'auth-who' }, icon('user', { size: 'sm' }), h('span', { class: 'truncate' }, st.username),
        h('button', { type: 'button', class: 'link small', onClick: () => { message = null; signIn(); } }, t('auth.mfa.notYou'))),
      form,
    ], { testid: 'login-page' });
  };

  // ------------------------------------------------------------ API トークン
  const tokenView = () => {
    const tok = passwordInput({ autocomplete: 'off', testid: 'login-token', name: 'token', placeholder: 'senca_…' });
    const err = h('div', { class: 'auth-error', 'aria-live': 'assertive' });
    const btn = submitButton(t('auth.token.submit'), 'login-submit');
    const form = h('form', { class: 'form auth-form', novalidate: true },
      fieldOf(t('login.token'), tok, { hint: [t('login.hintPrefix'), h('code', null, '{data-dir}/control/bootstrap-admin-token'), t('login.hintSuffix')] }),
      err, btn);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      replace(err);
      const token = tok.input.value.trim();
      if (!token) { replace(err, alert('error', t('login.empty'), { role: 'alert', small: true })); tok.input.focus(); return; }
      busy(btn, true, t('login.checking'));
      try {
        let me = null;
        try {
          ({ data: me } = await auth.me({ token, quiet401: true }));
        } catch (ex) {
          if (ex.status !== 404) throw ex;
          await system.capabilities({ token, quiet401: true }); // /auth/me の無い古いノード
        }
        setToken(token);
        tok.input.value = '';
        onLogin({ me });
      } catch (ex) {
        busy(btn, false, t('auth.token.submit'));
        const text = ex.status === 401 ? t('login.invalid') : describeError(ex);
        replace(err, alert('error', [text, ex.requestId ? h('div', { class: 'meta' }, `request_id: ${ex.requestId}`) : null], { role: 'alert', small: true }));
        tok.input.focus();
      }
    });
    const back = providers && (providers.password || providers.oidc.length)
      ? h('div', { class: 'auth-alt' }, h('button', { type: 'button', class: 'link', 'data-testid': 'login-back', onClick: () => (providers.setup_required && providers.password ? setup() : signIn()) },
        icon('arrow-left', { size: 'sm' }), providers.setup_required && providers.password ? t('auth.setup.backToSetup') : t('auth.token.back')))
      : null;
    frame([cardHead({ title: t('auth.token.title'), lead: t('auth.token.lead'), iconName: 'key' }), messageBox(), form, back]);
  };

  // ------------------------------------------------------------ 最初の管理者
  const setup = (stage = 1, bootToken = null) => {
    const stepper = h('ol', { class: 'auth-stepper', 'aria-label': t('auth.setup.steps') },
      [t('auth.setup.step1'), t('auth.setup.step2')].map((label, i) => h('li', {
        class: i + 1 < stage ? 'done' : i + 1 === stage ? 'current' : null, 'aria-current': i + 1 === stage ? 'step' : null,
      }, h('span', { class: 'n' }, i + 1 < stage ? icon('check', { size: 'sm' }) : String(i + 1)), h('span', null, label))));
    const err = h('div', { class: 'auth-error', 'aria-live': 'assertive', 'data-testid': 'setup-error' });
    const alt = h('div', { class: 'auth-alt' }, h('button', { type: 'button', class: 'link', 'data-testid': 'login-use-token', onClick: () => tokenView() }, icon('key', { size: 'sm' }), t('auth.setup.tokenInstead')));
    if (stage === 1) {
      const tok = passwordInput({ autocomplete: 'off', testid: 'setup-token', name: 'token', placeholder: 'senca_…' });
      const btn = submitButton(t('auth.setup.next'), 'setup-next');
      const form = h('form', { class: 'form auth-form', novalidate: true },
        h('p', { class: 'muted small' }, t('auth.setup.tokenLead')),
        fieldOf(t('auth.setup.tokenLabel'), tok, { hint: [t('login.hintPrefix'), h('code', null, '{data-dir}/control/bootstrap-admin-token'), t('login.hintSuffix')] }),
        err, btn);
      form.addEventListener('submit', async (e) => {
        e.preventDefault();
        replace(err);
        const token = tok.input.value.trim();
        if (!token) { replace(err, alert('error', t('login.empty'), { role: 'alert', small: true })); tok.input.focus(); return; }
        busy(btn, true, t('login.checking'));
        try {
          const { data } = await auth.me({ token, quiet401: true });
          if (!(data.scopes || []).includes('tenant:admin')) throw Object.assign(new Error(t('auth.setup.notAdmin')), { notAdmin: true });
          setup(2, token);
        } catch (ex) {
          busy(btn, false, t('auth.setup.next'));
          replace(err, alert('error', ex.notAdmin ? ex.message : ex.status === 401 ? t('login.invalid') : describeError(ex), { role: 'alert', small: true }));
          tok.input.focus();
        }
      });
      frame([cardHead({ title: t('auth.setup.title'), lead: t('auth.setup.lead'), iconName: 'rocket', kind: 'accent' }), stepper, messageBox(), form, alt], { testid: 'login-page' });
      return;
    }
    const user = h('input', { type: 'text', autocomplete: 'username', autocapitalize: 'off', spellcheck: 'false', required: true, maxlength: '64', 'data-testid': 'setup-username', placeholder: 'admin' });
    const name = h('input', { type: 'text', autocomplete: 'name', maxlength: '128', 'data-testid': 'setup-display-name' });
    const email = h('input', { type: 'email', autocomplete: 'email', maxlength: '254', 'data-testid': 'setup-email' });
    const pw = passwordInput({ autocomplete: 'new-password', testid: 'setup-password' });
    const pw2 = passwordInput({ autocomplete: 'new-password', testid: 'setup-password2' });
    const meter = strengthMeter(pw.input, () => ({ username: user.value, email: email.value }));
    user.addEventListener('input', () => meter.update());
    const btn = submitButton(t('auth.setup.create'), 'setup-create');
    const opt = h('span', { class: 'opt' }, t('form.optional'));
    const form = h('form', { class: 'form auth-form', novalidate: true },
      fieldOf(t('auth.username'), user, { hint: t('auth.usernameHint') }),
      h('div', { class: 'cols' }, fieldOf(t('auth.displayName'), name, { labelExtra: opt }), fieldOf(t('auth.email'), email, { labelExtra: opt.cloneNode(true) })),
      fieldOf(t('auth.newPassword'), pw, { extra: [pw.caps, meter] }),
      fieldOf(t('auth.confirmPassword'), pw2, { extra: pw2.caps }),
      err, btn);
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      replace(err);
      const problems = [];
      const username = user.value.trim();
      if (!/^[A-Za-z0-9._@+-]{1,64}$/.test(username)) problems.push(t('auth.usernameInvalid'));
      if (!meter.result().ok) problems.push(t('auth.passwordRule'));
      if (pw.input.value !== pw2.input.value) problems.push(t('auth.passwordMismatch'));
      if (problems.length) { replace(err, alert('error', h('ul', null, problems.map((x) => h('li', null, x))), { role: 'alert', small: true })); return; }
      busy(btn, true, t('auth.setup.creating'));
      try {
        const body = { username, password: pw.input.value, roles: ['admin'] };
        if (name.value.trim()) body.display_name = name.value.trim();
        if (email.value.trim()) body.email = email.value.trim();
        await users.create(body, { token: bootToken });
        const { data } = await auth.login({ username, password: pw.input.value });
        if (data.status === 'mfa_required') mfa({ ...data, username, password: pw.input.value });
        else finishSession(data, pw.input.value);
      } catch (ex) {
        busy(btn, false, t('auth.setup.create'));
        const text = ex.status === 409 ? t('auth.setup.taken') : describeError(ex);
        replace(err, alert('error', text, { role: 'alert', small: true }));
      }
    });
    frame([
      cardHead({ title: t('auth.setup.accountTitle'), lead: t('auth.setup.accountLead'), iconName: 'user-plus', kind: 'accent' }),
      stepper, form,
      h('div', { class: 'auth-alt' }, h('button', { type: 'button', class: 'link', onClick: () => setup(1) }, icon('arrow-left', { size: 'sm' }), t('auth.setup.back'))),
    ], { testid: 'login-page' });
  };

  // ------------------------------------------------------------ 読み込み
  frame([h('div', { class: 'auth-loading' }, skeleton({ kind: 'lines', rows: 4 }))]);
  auth.providers().then(({ data }) => {
    providers = data;
    if (data.setup_required && data.password) setup();
    else if (!data.password && !data.oidc.length) tokenView();
    else signIn();
  }).catch((e) => {
    // /auth/providers の無い古いノード、または接続できない: トークンだけ
    providers = { password: false, oidc: [], setup_required: false };
    if (e.status !== 404) { message = t('login.cannotConnect', { reason: describeError(e) }); messageKind = 'error'; }
    tokenView();
  });
  return { destroy: stop };
}

// ---------------------------------------------------------------- OIDC から戻ったところ

const OIDC_ERRORS = ['invalid_state', 'access_denied', 'idp_error', 'invalid_request', 'invalid_response', 'provider_unavailable', 'token_exchange_failed',
  'invalid_id_token', 'tenant_not_allowed', 'email_not_verified', 'email_domain_not_allowed', 'no_account', 'account_conflict', 'account_disabled',
  'no_role', 'rate_limited', 'server_error', 'code_expired'];

/** 失敗の理由の言葉（辞書の auth.oidcError.<code>）。知らない理由は「その他」。 */
export function oidcErrorKey(code) {
  return OIDC_ERRORS.includes(code) ? code : 'other';
}

/**
 * #/auth/complete?code=… / ?error=…
 * @param {{onLogin: (r) => void, onBack: () => void}} o
 */
export function renderAuthComplete(root, params, { onLogin, onBack }) {
  const code = params.get('code');
  const error = params.get('error');
  const providerId = takeProvider();
  const showError = async (errCode, detail) => {
    let provider = null;
    try {
      const { data } = await auth.providers();
      provider = providerId ? data.oidc.find((o) => o.id === providerId) : data.oidc.length === 1 ? data.oidc[0] : null;
    } catch { /* 名前が分からなくてもよい */ }
    const key = oidcErrorKey(errCode);
    const name = provider ? provider.display_name || provider.id : null;
    const retry = provider && !['account_disabled', 'no_account', 'no_role', 'email_domain_not_allowed', 'account_conflict', 'tenant_not_allowed'].includes(key)
      ? h('a', { class: 'btn primary lg block', href: `${provider.start_url}?return_to=${encodeURIComponent('/console/#/')}`, 'data-testid': 'oidc-retry', onClick: () => rememberProvider(provider.id) },
        icon('refresh'), h('span', { class: 'btn-label' }, t('auth.oidcError.retry', { name })))
      : null;
    authFrame(root, {
      testid: 'login-page',
      card: [
        cardHead({ title: name ? t('auth.oidcError.titleNamed', { name }) : t('auth.oidcError.title'), iconName: 'warning', kind: 'warn' }),
        h('div', { class: 'oidc-error', 'data-testid': 'oidc-error', 'data-code': errCode || '' },
          h('p', null, t(`auth.oidcError.${key}`)),
          h('p', { class: 'muted small' }, t(`auth.oidcError.${key}.next`)),
          detail ? h('p', { class: 'muted small' }, detail) : null,
          h('p', { class: 'subtle small' }, t('auth.oidcError.code'), ' ', h('code', null, errCode || '—'))),
        h('div', { class: 'stack' },
          retry,
          button(t('auth.oidcError.back'), { size: 'lg', class: 'block', testid: 'oidc-back', onClick: onBack })),
      ],
    });
    const f = root.querySelector('[data-testid="oidc-retry"], [data-testid="oidc-back"]');
    if (f) f.focus({ preventScroll: true });
  };
  if (error || !code) { showError(error || 'invalid_request'); return; }
  authFrame(root, { testid: 'login-page', card: [cardHead({ title: t('auth.complete.title'), iconName: 'login', kind: 'accent' }), loadingLine(t('auth.complete.working'))] });
  auth.exchange(code).then(({ data }) => {
    setToken(data.token);
    onLogin({ me: meFromLogin(data), returnTo: hashFromReturnTo(data.return_to) });
  }).catch((e) => {
    if (e.status === 401) showError('code_expired');
    else showError('server_error', describeError(e));
  });
}
