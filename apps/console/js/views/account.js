// アカウント（#/account、上の帯のアカウントのメニューから）: プロフィール・パスワードの変更・2 段階認証（認証アプリ・回復コード）・
// ログイン中の端末（セッション）の一覧と失効。API トークン・JWT でログインしているときは、分かることだけを出す。

import { h, replace, nextId } from '../lib/dom.js';
import { t, tn } from '../i18n/index.js';
import { load, modal, confirmDialog, reportError, toast, describeError } from '../lib/ui.js';
import { button, pageHeader, panel, kvList, badge, alert, dataTable, segmented, helpHint, iconButton } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { auth } from '../api/auth.js';
import { fmtTime, fmtRelative } from '../lib/format.js';
import { getMe, setMe, displayName, signInMethod, providerName } from '../lib/session.js';
import { parseUserAgent } from '../lib/useragent.js';
import { avatar } from '../shell.js';
import { passwordInput, strengthMeter, mfaEnrollment, recoveryCodesBlock } from './auth-parts.js';

/** セッションの端末の見せ方（アイコン・名前）。 */
export function deviceOf(s) {
  const ua = parseUserAgent(s.user_agent);
  const ic = ua.kind === 'phone' ? 'smartphone' : ua.kind === 'tablet' ? 'tablet' : ua.kind === 'tool' ? 'terminal' : 'laptop';
  const name = ua.browser && ua.os ? t('account.sessions.device', { browser: ua.browser, os: ua.os }) : ua.browser || ua.os || t('account.sessions.unknownDevice');
  return { icon: ic, name };
}

/** ログインの方法（パスワード・SSO と、2 段階認証を通ったか）。 */
export function sessionMethod(s) {
  return [
    s.method === 'oidc' ? badge(t('auth.method.ssoNamed', { name: s.provider ? providerName(s.provider) : 'SSO' }), 'info', { icon: 'building' }) : badge(t('account.sessions.password'), null, { icon: 'lock' }),
    s.mfa ? badge(t('account.sessions.mfa'), 'ok', { icon: 'shield-check' }) : null,
  ];
}

/** セッションの表（アカウント・ユーザーの画面で使う）。onRevoke を渡すと、行ごとの「ログアウトさせる」を付ける。 */
export function sessionsTable(items, { onRevoke, testid = 'sessions-table' } = {}) {
  return dataTable({
    testid,
    caption: t('account.sessions.title'),
    sort: { key: 'seen', dir: 'desc' },
    rowKey: (s) => s.id,
    empty: { icon: 'laptop', title: t('account.sessions.empty') },
    columns: [
      { key: 'device', label: t('account.sessions.col.device'), primary: true, sort: (s) => deviceOf(s).name, render: (s) => {
        const d = deviceOf(s);
        return h('div', { class: 'device-cell' }, h('span', { class: 'device-icon' }, icon(d.icon)),
          h('div', { class: 'device-text' },
            h('div', { class: 'row' }, h('strong', null, d.name), s.current ? badge(t('account.sessions.current'), 'accent', { testid: 'session-current' }) : null),
            h('div', { class: 'small muted' }, s.ip || t('account.sessions.noIp'))));
      } },
      { label: t('account.sessions.col.method'), render: (s) => h('div', { class: 'row' }, sessionMethod(s)) },
      { key: 'created', label: t('account.sessions.col.created'), sort: (s) => s.created_at, render: (s) => h('span', { title: fmtTime(s.created_at) }, fmtRelative(s.created_at)) },
      { key: 'seen', label: t('account.sessions.col.seen'), sort: (s) => s.last_seen_at, render: (s) => h('span', { title: fmtTime(s.last_seen_at) }, s.current ? t('account.sessions.now') : fmtRelative(s.last_seen_at)) },
      { key: 'expires', label: t('account.sessions.col.expires'), sort: (s) => s.idle_expires_at, render: (s) => {
        const end = new Date(s.idle_expires_at) < new Date(s.expires_at) ? s.idle_expires_at : s.expires_at;
        return h('span', { title: t('account.sessions.expiresDetail', { idle: fmtTime(s.idle_expires_at), max: fmtTime(s.expires_at) }) }, fmtRelative(end));
      } },
      onRevoke ? { label: t('common.actions'), actions: true, render: (s) => (s.current ? h('span', { class: 'small subtle' }, t('account.sessions.thisDevice'))
        : button(t('account.sessions.revoke'), { size: 'sm', variant: 'ghost', icon: 'logout', testid: 'session-revoke', attrs: { 'aria-label': t('account.sessions.revokeLabel', { device: deviceOf(s).name }) }, onClick: () => onRevoke(s) })) } : null,
    ].filter(Boolean),
    rows: items,
  });
}

// ---------------------------------------------------------------- ダイアログ

function changePasswordDialog(user) {
  const cur = passwordInput({ autocomplete: 'current-password', testid: 'pw-current' });
  const pw = passwordInput({ autocomplete: 'new-password', testid: 'pw-new' });
  const pw2 = passwordInput({ autocomplete: 'new-password', testid: 'pw-new2' });
  const meter = strengthMeter(pw.input, () => ({ username: user.username, email: user.email }));
  const err = h('div', { 'aria-live': 'assertive' });
  const lab = (text, ctl, extra) => { const id = nextId('f'); ctl.input.id = id; return h('div', { class: 'field' }, h('label', { for: id }, text), ctl.el, extra || null); };
  return modal(t('account.password.changeTitle'), () => h('form', { class: 'form', novalidate: true, onSubmit: (e) => e.preventDefault() },
    h('input', { type: 'text', autocomplete: 'username', value: user.username, hidden: true, readonly: true, 'aria-hidden': 'true', tabindex: '-1' }),
    lab(t('auth.currentPassword'), cur, cur.caps),
    lab(t('auth.newPassword'), pw, [pw.caps, meter]),
    lab(t('auth.confirmPassword'), pw2, pw2.caps),
    alert('info', t('account.password.othersSignedOut'), { small: true }),
    err), {
    icon: 'key',
    testid: 'password-dialog',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'pw-submit', onClick: async () => {
        replace(err);
        const problems = [];
        if (!cur.input.value) problems.push(t('auth.forced.needCurrent'));
        if (!meter.result().ok) problems.push(t('auth.passwordRule'));
        if (pw.input.value !== pw2.input.value) problems.push(t('auth.passwordMismatch'));
        if (pw.input.value && pw.input.value === cur.input.value) problems.push(t('auth.forced.sameAsOld'));
        if (problems.length) { replace(err, alert('error', h('ul', null, problems.map((x) => h('li', null, x))), { role: 'alert', small: true })); return; }
        ok.disabled = true;
        try {
          await auth.changePassword(cur.input.value, pw.input.value);
          close(true);
        } catch (e) {
          replace(err, alert('error', e.status === 403 ? t('auth.forced.wrongCurrent') : describeError(e), { role: 'alert', small: true }));
          if (e.status === 403) cur.input.focus();
        } finally {
          ok.disabled = false;
        }
      } }, t('account.password.submit'));
      return [h('button', { type: 'button', onClick: () => close(false) }, t('common.cancel')), ok];
    },
  });
}

/**
 * 本人確認（パスワードか、認証アプリのコード・回復コード）をしてから run(proof) を呼ぶダイアログ。
 * run が返した値で閉じる。403 は「違います」と出してやり直してもらう。
 */
function proofDialog(title, message, { ok, danger, run, testid, icon: ic }) {
  let mode = 'password';
  const pw = passwordInput({ autocomplete: 'current-password', testid: 'proof-password' });
  const code = h('input', { type: 'text', autocomplete: 'one-time-code', inputmode: 'text', spellcheck: 'false', class: 'mono', 'data-testid': 'proof-code', placeholder: '123456 / xxxx-xxxx-xxxx-xxxx' });
  const pwId = nextId('f');
  const codeId = nextId('f');
  pw.input.id = pwId;
  code.id = codeId;
  const pwField = h('div', { class: 'field' }, h('label', { for: pwId }, t('auth.currentPassword')), pw.el, pw.caps);
  const codeField = h('div', { class: 'field', hidden: true }, h('label', { for: codeId }, t('account.proof.code')), code, h('div', { class: 'hint' }, t('account.proof.codeHint')));
  const err = h('div', { 'aria-live': 'assertive' });
  const seg = segmented({ label: t('account.proof.how'), size: 'sm', options: [['password', t('account.proof.password')], ['code', t('account.proof.useCode')]], value: 'password', onChange: (v) => {
    mode = v;
    pwField.hidden = v !== 'password';
    codeField.hidden = v !== 'code';
    (v === 'password' ? pw.input : code).focus();
  } });
  return modal(title, () => h('form', { class: 'form', novalidate: true, onSubmit: (e) => e.preventDefault() }, h('p', null, message), seg, pwField, codeField, err), {
    icon: ic || (danger ? 'warning' : 'shield'),
    kind: danger ? 'danger' : 'info',
    testid,
    actions: (close) => {
      const b = h('button', { type: 'button', class: danger ? 'danger solid' : 'primary', 'data-testid': 'proof-ok', onClick: async () => {
        replace(err);
        const proof = mode === 'password' ? { password: pw.input.value } : { code: code.value.trim() };
        if (!(proof.password || proof.code)) { (mode === 'password' ? pw.input : code).focus(); return; }
        b.disabled = true;
        try {
          close(await run(proof));
        } catch (e) {
          replace(err, alert('error', e.status === 403 ? t('account.proof.wrong') : describeError(e), { role: 'alert', small: true }));
        } finally {
          b.disabled = false;
        }
      } }, ok);
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), b];
    },
  });
}

// ---------------------------------------------------------------- 画面

export async function render(root) {
  const profileBox = h('div');
  const securityBox = h('div');
  const sessionsBox = h('div');
  let me = getMe();

  const reloadMe = async () => {
    const { data } = await auth.me();
    me = data;
    setMe(data);
    return data;
  };

  const drawProfile = () => {
    const u = me && me.user;
    const m = signInMethod(me);
    const method = m.provider ? t('auth.method.ssoNamed', { name: m.provider }) : t(`auth.method.${m.key}`);
    if (!u) {
      replace(profileBox, panel({ title: t('account.profile'), icon: 'user' },
        h('div', { class: 'profile' }, avatar(me, { size: 'xl' }),
          h('div', { class: 'profile-text' },
            h('div', { class: 'profile-name', 'data-testid': 'account-name' }, displayName(me)),
            h('div', { class: 'muted' }, method))),
        kvList([
          [t('identity.tenant'), me && me.tenant ? h('code', null, me.tenant) : '—'],
          [t('account.subject'), me ? h('code', null, me.subject) : '—'],
          [t('account.scopes'), me && me.scopes ? h('div', { class: 'row' }, me.scopes.map((s) => h('span', { class: 'tag' }, s))) : t('identity.noScopes')],
        ]),
        alert('info', t('account.tokenNote'), { small: true, testid: 'account-token-note' })));
      return;
    }
    replace(profileBox, panel({ title: t('account.profile'), icon: 'user', help: helpHint({ title: t('account.profile'), body: t('account.profileHelp'), guide: '12-security.md' }) },
      h('div', { class: 'profile' }, avatar(me, { size: 'xl' }),
        h('div', { class: 'profile-text' },
          h('div', { class: 'profile-name', 'data-testid': 'account-name' }, displayName(me)),
          h('div', { class: 'muted' }, [u.username, u.email].filter(Boolean).join(' · ')),
          h('div', { class: 'row' }, u.roles.map((r) => badge(t(`users.role.${r}`), r === 'admin' ? 'accent' : null, { icon: r === 'admin' ? 'shield' : null })),
            u.scopes.length ? badge(tn('users.extraScopes', u.scopes.length), 'outline') : null))),
      kvList([
        [t('identity.tenant'), h('code', null, me.tenant)],
        [t('identity.signedInWith'), method],
        [t('account.lastLogin'), u.last_login_at ? fmtTime(u.last_login_at) : '—'],
        [t('account.created'), fmtTime(u.created_at)],
        u.oidc.length ? [t('account.linked'), h('div', { class: 'row' }, u.oidc.map((o) => badge(providerName(o.provider), 'info', { icon: 'building' })))] : null,
      ])));
  };

  const drawSecurity = () => {
    const u = me && me.user;
    if (!u) { replace(securityBox); return; }
    // パスワード
    const pwPanel = u.has_password
      ? panel({ title: t('account.password.title'), icon: 'key', testid: 'account-password' },
        h('p', { class: 'muted' }, u.password_changed_at ? t('account.password.changed', { when: fmtRelative(u.password_changed_at) }) : t('account.password.neverChanged')),
        button(t('account.password.change'), { icon: 'edit', testid: 'account-password-change', onClick: async () => {
          if (await changePasswordDialog(u)) {
            toast(t('account.password.done'), 'ok');
            await reloadMe().catch(() => {});
            drawSecurity();
            refreshSessions();
          }
        } }))
      : panel({ title: t('account.password.title'), icon: 'key', testid: 'account-password' },
        h('p', { class: 'muted' }, t('account.password.none', { provider: (u.oidc[0] && providerName(u.oidc[0].provider)) || 'SSO' })));
    // 2 段階認証
    let mfaPanel;
    if (!u.has_password) {
      mfaPanel = panel({ title: t('account.mfa.title'), icon: 'shield-check', testid: 'account-mfa' }, h('p', { class: 'muted' }, t('account.mfa.idp')));
    } else if (!u.mfa.enabled) {
      mfaPanel = panel({ title: t('account.mfa.title'), icon: 'shield', testid: 'account-mfa', actions: [badge(t('account.mfa.off'), 'outline', { testid: 'mfa-status' })] },
        h('p', { class: 'muted' }, t('account.mfa.offBody')),
        button(t('account.mfa.setup'), { variant: 'primary', icon: 'qr-code', testid: 'account-mfa-setup', onClick: async () => {
          const ok = await modal(t('account.mfa.setupTitle'), (close) => mfaEnrollment({ username: u.username, compact: true, onDone: () => close(true), onCancel: () => close(false) }), { wide: true, icon: 'shield-check', testid: 'mfa-dialog' });
          if (ok) {
            toast(t('account.mfa.turnedOn'), 'ok');
            await reloadMe().catch(() => {});
            drawSecurity();
            drawProfile();
          }
        } }));
    } else {
      const left = u.mfa.recovery_codes_remaining;
      mfaPanel = panel({ title: t('account.mfa.title'), icon: 'shield-check', testid: 'account-mfa', actions: [badge(t('account.mfa.on'), 'ok', { dot: true, testid: 'mfa-status' })] },
        h('p', { class: 'muted' }, t('account.mfa.onBody')),
        kvList([
          [t('account.mfa.since'), u.mfa.enrolled_at ? fmtTime(u.mfa.enrolled_at) : '—'],
          [t('account.mfa.codesLeft'), h('span', { class: left <= 3 ? 'text-warn' : null, 'data-testid': 'mfa-codes-left' }, tn('account.mfa.codesLeftN', left))],
        ]),
        left <= 3 ? alert('warn', t('account.mfa.lowCodes'), { small: true }) : null,
        h('div', { class: 'row' },
          button(t('account.mfa.regenerate'), { icon: 'refresh', testid: 'account-mfa-regenerate', onClick: async () => {
            const codes = await proofDialog(t('account.mfa.regenerateTitle'), t('account.mfa.regenerateBody'), {
              ok: t('account.mfa.regenerateOk'), testid: 'regenerate-dialog', icon: 'refresh',
              run: async (proof) => (await auth.recoveryCodes(proof)).data.recovery_codes,
            });
            if (!codes) return;
            await modal(t('auth.recovery.title'), (close) => recoveryCodesBlock(codes, { username: u.username, onConfirm: () => close(true) }), { wide: true, icon: 'key', kind: 'warn', testid: 'recovery-dialog', dismissible: false });
            await reloadMe().catch(() => {});
            drawSecurity();
          } }),
          button(t('account.mfa.disable'), { variant: 'danger', icon: 'shield-off', testid: 'account-mfa-disable', onClick: async () => {
            const ok = await proofDialog(t('account.mfa.disableTitle'), t('account.mfa.disableBody'), {
              ok: t('account.mfa.disableOk'), danger: true, testid: 'disable-mfa-dialog',
              run: async (proof) => { await auth.mfaDisable(proof); return true; },
            });
            if (!ok) return;
            toast(t('account.mfa.turnedOff'), 'ok');
            await reloadMe().catch(() => {});
            drawSecurity();
          } })));
    }
    replace(securityBox, h('div', { class: 'grid cols-2 account-security' }, pwPanel, mfaPanel));
  };

  const revoke = async (s) => {
    const ok = await confirmDialog(t('account.sessions.revokeTitle'), t('account.sessions.revokeBody', { device: deviceOf(s).name }), { ok: t('account.sessions.revoke'), danger: false, icon: 'logout' });
    if (!ok) return;
    try {
      await auth.revokeSession(s.id);
      toast(t('account.sessions.revoked'), 'ok');
    } catch (e) {
      reportError(e, t('account.sessions.revoke'));
    }
    refreshSessions();
  };

  let sessionItems = [];
  const revokeOthers = async () => {
    const others = sessionItems.filter((s) => !s.current);
    if (!others.length) { toast(t('account.sessions.noOthers'), 'info'); return; }
    const ok = await confirmDialog(t('account.sessions.othersTitle'), tn('account.sessions.othersBody', others.length), { ok: t('account.sessions.othersOk'), danger: false, icon: 'logout' });
    if (!ok) return;
    let failed = 0;
    for (const s of others) {
      try { await auth.revokeSession(s.id); } catch { failed++; }
    }
    toast(failed ? t('account.sessions.othersPartial', { n: failed }) : t('account.sessions.othersDone'), failed ? 'warn' : 'ok');
    refreshSessions();
  };

  const othersBtn = button(t('account.sessions.others'), { icon: 'logout', size: 'sm', testid: 'sessions-revoke-others', onClick: revokeOthers });
  const refreshSessions = () => {
    if (!me || me.auth_method !== 'session') return Promise.resolve();
    return load(sessionsBox, async () => {
      const { data } = await auth.sessions({ limit: 100 });
      sessionItems = data.items;
      othersBtn.disabled = !data.items.some((s) => !s.current);
      return sessionsTable(data.items, { onRevoke: revoke });
    }, { kind: 'table' });
  };

  root.append(
    pageHeader({ title: t('account.title'), description: t('account.lead'), crumbs: [{ label: t('account.title') }] }),
    profileBox,
    securityBox,
    me && me.auth_method === 'session'
      ? panel({ title: t('account.sessions.title'), icon: 'laptop', description: t('account.sessions.lead'), testid: 'account-sessions', flush: true, class: 'sessions-panel',
        actions: [iconButton('refresh', t('common.refresh'), { size: 'sm', onClick: () => refreshSessions() }), othersBtn] }, sessionsBox)
      : null);
  try { await reloadMe(); } catch { /* 前の値で出す */ }
  drawProfile();
  drawSecurity();
  await refreshSessions();
  return null;
}
