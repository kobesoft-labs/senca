// ログイン・先に済ませる手続き・アカウント・ユーザーの画面で使う部品:
// ログインの画面の枠、パスワードの欄（表示の切り替え・Caps Lock の知らせ・強さの目安）、6 桁のコードの欄、
// QR コード、IdP の印、回復コードの表示、認証アプリの登録（MFA）の流れ、待ち時間の数え下げ。

import { h, replace, s, nextId } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t, tn, getLang, setLang, LANGS } from '../i18n/index.js';
import { getThemePref, setThemePref } from '../lib/theme.js';
import { segmented, alert, button, copyText, tooltip } from '../ui/index.js';
import { describeError, downloadBlob } from '../lib/ui.js';
import { qrMatrix, qrPath } from '../lib/qr.js';
import { passwordStrength, MIN_LENGTH } from '../lib/password.js';
import { auth } from '../api/auth.js';
import { fmtDuration } from '../lib/format.js';

// ---------------------------------------------------------------- 画面の枠（ログイン・手続き）

/**
 * ログインの画面の枠: 左上に名前、右上に言語・明暗（と extra）、真ん中にカード。
 * @param {HTMLElement} root
 * @param {{card: Node|Node[], extra?: Node[], wide?: boolean, testid?: string, foot?: Node}} o
 * @returns {{card: HTMLElement}}
 */
export function authFrame(root, { card, extra, wide, testid, foot }) {
  const cardEl = h('div', { class: ['auth-card', wide && 'wide'].filter(Boolean).join(' ') }, card);
  replace(root,
    h('div', { class: 'auth-page', 'data-testid': testid || 'login-page' },
      h('header', { class: 'auth-top' },
        h('div', { class: 'brand' },
          h('span', { class: 'brand-mark', 'aria-hidden': 'true' }, 'S'),
          h('span', { class: 'brand-text' }, h('strong', null, 'SENCA'), h('small', null, t('shell.product')))),
        h('div', { class: 'auth-tools' },
          extra || null,
          segmented({ label: t('shell.language'), size: 'sm', options: LANGS.map(([c, n]) => [c, n]), value: getLang(), onChange: (v) => setLang(v), testid: 'login-lang' }),
          segmented({ label: t('shell.theme'), size: 'sm', iconOnly: true, options: [['system', t('theme.system'), 'monitor'], ['light', t('theme.light'), 'sun'], ['dark', t('theme.dark'), 'moon']], value: getThemePref(), onChange: (v) => setThemePref(v) }))),
      h('main', { id: 'main', class: 'auth-main', tabindex: '-1' },
        cardEl,
        foot || h('p', { class: 'auth-foot' }, t('auth.footHelp')))));
  return { card: cardEl };
}

/** カードの上の見出し（アイコン・h1・説明）。 */
export function cardHead({ title, lead, iconName, kind }) {
  return h('div', { class: 'auth-head' },
    iconName ? h('div', { class: ['auth-icon', kind].filter(Boolean).join(' ') }, icon(iconName)) : null,
    h('h1', null, title),
    lead ? h('p', { class: 'auth-lead' }, lead) : null);
}

// ---------------------------------------------------------------- パスワードの欄

/**
 * パスワードの欄（目のボタンで表示・非表示、Caps Lock の知らせ）。
 * @param {{autocomplete: string, testid?: string, name?: string, placeholder?: string, required?: boolean}} o
 * @returns {{el: HTMLElement, input: HTMLInputElement, caps: HTMLElement}}
 */
export function passwordInput({ autocomplete, testid, name, placeholder, required = true }) {
  const input = h('input', {
    type: 'password', name: name || 'password', autocomplete, spellcheck: 'false', autocapitalize: 'off', 'data-testid': testid,
    placeholder: placeholder || null, required, maxlength: '1024',
  });
  const eye = h('button', { type: 'button', class: 'ghost icon-only sm pw-eye', 'aria-pressed': 'false', 'aria-label': t('auth.showPassword'), 'data-testid': testid ? `${testid}-reveal` : null }, icon('eye'));
  tooltip(eye, () => eye.getAttribute('aria-label'));
  eye.addEventListener('click', () => {
    const shown = input.type === 'password';
    input.type = shown ? 'text' : 'password';
    eye.setAttribute('aria-pressed', String(shown));
    eye.setAttribute('aria-label', shown ? t('auth.hidePassword') : t('auth.showPassword'));
    replace(eye, icon(shown ? 'eye-off' : 'eye'));
    input.focus();
  });
  const caps = capsLockHint(input);
  return { el: h('div', { class: 'pw-field' }, input, eye), input, caps };
}

/** Caps Lock がオンのときに出す一言（入力欄の下）。 */
export function capsLockHint(input) {
  const el = h('div', { class: 'caps-hint', role: 'status', hidden: true, 'data-testid': 'caps-hint' }, icon('caps-lock', { size: 'sm' }), t('auth.capsLock'));
  const sync = (e) => {
    if (!e.getModifierState) return;
    el.hidden = !e.getModifierState('CapsLock');
  };
  input.addEventListener('keydown', sync);
  input.addEventListener('keyup', sync);
  input.addEventListener('blur', () => { el.hidden = true; });
  return el;
}

const STRENGTH_KIND = ['none', 'weak', 'weak', 'good', 'good'];

/**
 * 強さの目安（4 つの区切り＋言葉）。input の入力のたびに直す。ctx は利用者名・メールアドレスを返す関数。
 * 赤は使わない（取り消せない操作の色のため）: 弱い = 琥珀、強い = 緑。
 * @returns {HTMLElement & {result: () => object}}
 */
export function strengthMeter(input, ctx = () => ({})) {
  const bars = [0, 1, 2, 3].map(() => h('span', { class: 'pw-bar' }));
  const label = h('span', { class: 'pw-label' });
  const tip = h('span', { class: 'pw-tip' });
  const el = h('div', { class: 'pw-strength', 'data-score': '0', 'data-testid': 'password-strength', 'aria-live': 'polite' },
    h('div', { class: 'pw-bars', 'aria-hidden': 'true' }, bars), h('div', { class: 'pw-text' }, label, tip));
  let last = passwordStrength('', ctx());
  const update = () => {
    const r = passwordStrength(input.value, ctx());
    last = r;
    el.dataset.score = String(r.score);
    el.dataset.kind = STRENGTH_KIND[r.score];
    bars.forEach((b, i) => b.classList.toggle('on', r.score > i || (r.score === 0 && input.value && i === 0)));
    if (!input.value) {
      label.textContent = t('auth.strength.empty', { n: MIN_LENGTH });
      tip.textContent = '';
    } else if (r.reason === 'short') {
      label.textContent = tn('auth.strength.short', r.missing);
      tip.textContent = '';
    } else if (r.reason === 'sameAsName') {
      label.textContent = t('auth.strength.sameAsName');
      tip.textContent = '';
    } else if (r.reason === 'long') {
      label.textContent = t('auth.strength.long');
      tip.textContent = '';
    } else {
      label.textContent = t(`auth.strength.${r.score}`);
      tip.textContent = r.score < 3 && r.tips.length ? t(`auth.strength.tip.${r.tips[0]}`) : '';
    }
  };
  input.addEventListener('input', update);
  update();
  el.result = () => last;
  el.update = update;
  return el;
}

// ---------------------------------------------------------------- 6 桁のコード

/**
 * 6 桁のコードの欄。本物の input は 1 つ（貼り付け・自動入力・読み上げが素直に動く）で、上に 6 つの枠を重ねて見せる。
 * 6 桁そろったら onComplete(code)。
 * @returns {HTMLElement & {input: HTMLInputElement, clear: () => void, setInvalid: (on: boolean) => void}}
 */
export function otpInput({ label, testid, onComplete, describedBy }) {
  const input = h('input', {
    type: 'text', inputmode: 'numeric', autocomplete: 'one-time-code', pattern: '[0-9]*', maxlength: '6', class: 'otp-input',
    'aria-label': label, spellcheck: 'false', 'data-testid': testid, 'aria-describedby': describedBy || null,
  });
  const slots = Array.from({ length: 6 }, () => h('span', { class: 'otp-slot' }));
  const el = h('div', { class: 'otp' }, h('div', { class: 'otp-slots', 'aria-hidden': 'true' }, slots.slice(0, 3), h('span', { class: 'otp-sep' }), slots.slice(3)), input);
  let fired = '';
  const sync = () => {
    const v = input.value.replace(/\D/g, '').slice(0, 6);
    if (v !== input.value) input.value = v;
    const focused = document.activeElement === input;
    slots.forEach((sl, i) => {
      sl.textContent = v[i] || '';
      sl.classList.toggle('filled', !!v[i]);
      sl.classList.toggle('active', focused && (i === v.length || (v.length === 6 && i === 5)));
    });
    if (v.length === 6 && fired !== v && onComplete) { fired = v; onComplete(v); }
    if (v.length < 6) fired = '';
  };
  input.addEventListener('input', () => { el.classList.remove('invalid'); sync(); });
  input.addEventListener('focus', sync);
  input.addEventListener('blur', sync);
  // 枠の上のどこを押しても、最後の桁に入れる
  input.addEventListener('click', () => { const n = input.value.length; input.setSelectionRange(n, n); });
  el.input = input;
  el.clear = () => { input.value = ''; fired = ''; sync(); };
  el.setInvalid = (on) => { el.classList.toggle('invalid', !!on); input.setAttribute('aria-invalid', on ? 'true' : 'false'); };
  sync();
  return el;
}

// ---------------------------------------------------------------- QR コード

/** QR コードの SVG（白地に黒。暗い表示でも読めるように白い余白ごと描く）。1 モジュールが整数の画素になる大きさにする。 */
export function qrCode(text, { label, testid, target = 216 } = {}) {
  const m = qrMatrix(text);
  const { d, viewBox } = qrPath(m);
  const svg = s('svg', {
    viewBox: `0 0 ${viewBox} ${viewBox}`, class: 'qr', role: 'img', 'aria-label': label || t('auth.mfa.qrLabel'),
    'shape-rendering': 'crispEdges', 'data-testid': testid, 'data-version': String(m.version),
  },
  s('rect', { width: String(viewBox), height: String(viewBox), fill: '#ffffff' }),
  s('path', { d, fill: '#000000' }));
  const px = Math.max(3, Math.floor(target / viewBox)) * viewBox;
  svg.style.width = `${px}px`;
  svg.style.height = `${px}px`;
  return svg;
}

// ---------------------------------------------------------------- IdP の印

/** 表示名や ID から、Google・Microsoft なら簡単な印、ほかは建物の印。 */
export function providerIcon(p) {
  const name = `${p.id} ${p.display_name || ''}`;
  if (/google/i.test(name)) return icon('brand-google');
  if (/microsoft|entra|azure|office\s*365/i.test(name)) return icon('brand-microsoft');
  return icon('building');
}

// ---------------------------------------------------------------- 回復コード

/**
 * 回復コードの表示（コピー・ダウンロード・「保存した」の確認）。onConfirm は確認のチェックを入れて「完了」を押したとき。
 * @returns {HTMLElement}
 */
export function recoveryCodesBlock(codes, { username, onConfirm, confirmLabel } = {}) {
  const text = () => [t('auth.recovery.fileHead', { user: username || '', host: location.host }), '', ...codes, '', t('auth.recovery.fileFoot')].join('\n');
  const saved = h('input', { type: 'checkbox', 'data-testid': 'recovery-saved' });
  const done = h('button', { type: 'button', class: 'primary', disabled: true, 'data-testid': 'recovery-done' }, confirmLabel || t('auth.recovery.done'));
  saved.addEventListener('change', () => { done.disabled = !saved.checked; });
  done.addEventListener('click', () => { if (saved.checked && onConfirm) onConfirm(); });
  return h('div', { class: 'recovery', 'data-testid': 'recovery-codes' },
    alert('warn', t('auth.recovery.once'), { title: t('auth.recovery.onceTitle') }),
    h('p', { class: 'muted small' }, t('auth.recovery.howto')),
    h('ol', { class: 'recovery-list' }, codes.map((c) => h('li', null, h('code', { 'data-testid': 'recovery-code' }, c)))),
    h('div', { class: 'row' },
      button(t('auth.recovery.copy'), { icon: 'copy', size: 'sm', testid: 'recovery-copy', onClick: () => copyText(codes.join('\n'), t('auth.recovery.codes')) }),
      button(t('auth.recovery.download'), { icon: 'download', size: 'sm', testid: 'recovery-download', onClick: () => downloadBlob(new Blob([text()], { type: 'text/plain' }), `senca-recovery-codes-${(username || 'user').replace(/[^\w.-]/g, '_')}.txt`) })),
    onConfirm ? h('div', { class: 'recovery-confirm' },
      h('label', { class: 'check' }, saved, t('auth.recovery.savedCheck')),
      done) : null);
}

// ---------------------------------------------------------------- 認証アプリの登録

/** base32 の秘密を 4 文字ずつに区切る（手で打つとき読みやすく）。 */
export function groupSecret(secret) {
  return String(secret || '').replace(/(.{4})(?=.)/g, '$1 ');
}

/**
 * 認証アプリの登録の流れ: (今のパスワード) → QR とキー → 確認のコード → 回復コード。
 * password を渡すと最初の確認を飛ばす（ログインの直後など）。onDone(user) は回復コードを保存したと確かめた後。
 * @param {{password?: string|null, username?: string, onDone: (user: object) => void, onCancel?: () => void, compact?: boolean}} o
 * @returns {HTMLElement}
 */
export function mfaEnrollment({ password, username, onDone, onCancel, compact }) {
  const root = h('div', { class: 'mfa-enroll', 'data-testid': 'mfa-enroll' });
  const cancelBtn = () => (onCancel ? button(t('common.cancel'), { onClick: onCancel }) : null);

  const askPassword = (message) => {
    const pw = passwordInput({ autocomplete: 'current-password', testid: 'mfa-password' });
    const err = h('div', { 'aria-live': 'assertive' }, message ? alert('error', message, { role: 'alert' }) : null);
    const id = nextId('f');
    pw.input.id = id;
    const go = h('button', { type: 'submit', class: 'primary', 'data-testid': 'mfa-password-next' }, t('auth.mfa.next'));
    const form = h('form', { class: 'form', novalidate: true },
      h('p', { class: 'muted' }, t('auth.mfa.passwordLead')),
      h('div', { class: 'field' }, h('label', { for: id }, t('auth.currentPassword')), pw.el, pw.caps),
      err,
      h('div', { class: 'auth-actions' }, cancelBtn(), go));
    form.addEventListener('submit', async (e) => {
      e.preventDefault();
      if (!pw.input.value) { pw.input.focus(); return; }
      go.disabled = true;
      try { await start(pw.input.value); } finally { go.disabled = false; }
    });
    replace(root, form);
    pw.input.focus();
  };

  const start = async (pwd) => {
    try {
      const { data } = await auth.totpSetup(pwd);
      showSecret(data);
    } catch (e) {
      if (e.status === 403) askPassword(t('auth.mfa.wrongPassword'));
      else if (e.status === 429) askPassword(describeError(e));
      else if (e.status === 409) replace(root, alert('info', t('auth.mfa.alreadyOn')), h('div', { class: 'auth-actions' }, cancelBtn()));
      else askPassword(describeError(e));
    }
  };

  const showSecret = (setup) => {
    const err = h('div', { 'aria-live': 'assertive' });
    const verify = h('button', { type: 'submit', class: 'primary', 'data-testid': 'mfa-confirm' }, t('auth.mfa.enable'));
    const hintId = nextId('otp-hint');
    let busy = false;
    const submit = async () => {
      const code = otp.input.value;
      replace(err);
      if (code.length !== 6) { otp.setInvalid(true); replace(err, alert('error', t('auth.mfa.needCode'), { role: 'alert', small: true })); otp.input.focus(); return; }
      if (busy) return;
      busy = true;
      verify.disabled = true;
      try {
        const { data } = await auth.totpConfirm(setup.enrollment_token, code);
        showCodes(data.recovery_codes || [], data.user);
      } catch (e) {
        otp.setInvalid(true);
        otp.clear();
        otp.input.focus();
        replace(err, alert('error', e.status === 400 ? t('auth.mfa.badCode') : describeError(e), { role: 'alert', small: true }));
      } finally {
        busy = false;
        verify.disabled = false;
      }
    };
    const otp = otpInput({ label: t('auth.mfa.codeLabel'), testid: 'mfa-code', onComplete: submit, describedBy: hintId });
    const secretText = groupSecret(setup.secret);
    const secretBox = h('div', { class: 'mfa-secret' },
      h('code', { 'data-testid': 'mfa-secret', 'data-secret': setup.secret }, secretText),
      button(t('auth.mfa.copySecret'), { icon: 'copy', size: 'sm', variant: 'ghost', iconOnly: true, onClick: () => copyText(setup.secret, t('auth.mfa.secretLabel')) }));
    const form = h('form', { class: ['mfa-steps', compact && 'compact'].filter(Boolean).join(' '), novalidate: true },
      h('ol', { class: 'steps' },
        h('li', null,
          h('div', { class: 'step-title' }, t('auth.mfa.step1')),
          h('p', { class: 'muted small' }, t('auth.mfa.step1Body'))),
        h('li', null,
          h('div', { class: 'step-title' }, t('auth.mfa.step2')),
          h('div', { class: 'mfa-qr' },
            h('div', { class: 'qr-frame' }, qrCode(setup.otpauth_uri, { testid: 'mfa-qr' })),
            h('div', { class: 'mfa-manual' },
              h('p', { class: 'muted small' }, t('auth.mfa.manual')),
              secretBox,
              h('p', { class: 'subtle small' }, t('auth.mfa.params'))))),
        h('li', null,
          h('div', { class: 'step-title' }, t('auth.mfa.step3')),
          h('p', { class: 'muted small', id: hintId }, t('auth.mfa.step3Body')),
          otp,
          err)),
      h('div', { class: 'auth-actions' }, cancelBtn(), verify));
    form.addEventListener('submit', (e) => { e.preventDefault(); submit(); });
    replace(root, form);
    otp.input.focus({ preventScroll: true });
  };

  const showCodes = (codes, user) => {
    replace(root,
      h('div', { class: 'stack' },
        alert('ok', t('auth.mfa.enabled'), { testid: 'mfa-enabled' }),
        h('h2', { class: 'auth-h2' }, t('auth.recovery.title')),
        recoveryCodesBlock(codes, { username: (user && user.username) || username, onConfirm: () => onDone(user) })));
    const first = root.querySelector('[data-testid="recovery-copy"]');
    if (first) first.focus({ preventScroll: true });
  };

  if (password) start(password);
  else askPassword(null);
  return root;
}

// ---------------------------------------------------------------- 待ち時間

/**
 * 数え下げ（429 の Retry-After）。el に「m:ss」を入れ、0 になったら onEnd。戻り値で止める。
 */
export function countdown(el, seconds, onEnd) {
  const end = Date.now() + seconds * 1000;
  let timer = null;
  const tick = () => {
    const left = Math.max(0, Math.ceil((end - Date.now()) / 1000));
    el.textContent = fmtDuration(left * 1000);
    if (left <= 0) { clearInterval(timer); if (onEnd) onEnd(); }
  };
  tick();
  timer = setInterval(tick, 250);
  return () => clearInterval(timer);
}
