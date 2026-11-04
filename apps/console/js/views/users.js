// ユーザー（#/users、tenant:admin が要る）: 一覧（検索・役割・状態・2 段階認証・ログインの方法で絞る）、追加（役割の説明・仮のパスワードを 1 度だけ見せる）、
// 役割の変更（最後の管理者の 409 を説明する）、無効・有効、パスワードの再設定、2 段階認証を外す、セッションの一覧と失効、削除（名前を入れて確かめる）。
// SSE の user.created・updated・deleted で読み直す。#/users?new=1 で追加のダイアログ、#/users?user={id} で変更のダイアログを開く。

import { h, replace, nextId, debounce } from '../lib/dom.js';
import { t, tn } from '../i18n/index.js';
import { load, modal, confirmDialog, reportError, toast, describeError, copyText } from '../lib/ui.js';
import { button, iconButton, pageHeader, dataTable, emptyState, badge, alert, searchBox, helpHint, attachMenu, copyField, statusPill } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { users, auth, ROLES, ROLE_SCOPES, USER_SCOPES, rolePreset, isAdminUser } from '../api/auth.js';
import { fmtTime, fmtRelative, idemKey } from '../lib/format.js';
import { getMe, hasScope, initials, providerName } from '../lib/session.js';
import { refreshOn } from '../lib/live.js';
import { onPageKey } from '../shortcuts.js';
import { generatePassword } from '../lib/password.js';
import { passwordInput, strengthMeter } from './auth-parts.js';
import { sessionsTable } from './account.js';

const ROLE_ICON = { admin: 'shield', operator: 'switcher', viewer: 'eye', custom: 'settings' };
const ROLE_BADGE = { admin: 'accent', operator: 'info', viewer: null };

const userName = (u) => u.display_name || u.username;

/** 利用者のセル（頭文字・名前・利用者名とメールアドレス）。 */
function userCell(u, { me, onOpen }) {
  const self = me && me.user && me.user.id === u.id;
  return h('div', { class: 'user-cell' },
    h('span', { class: ['avatar', 'md', u.disabled && 'off'].filter(Boolean).join(' '), 'aria-hidden': 'true' }, initials(userName(u))),
    h('div', { class: 'user-text' },
      h('div', { class: 'row' },
        h('button', { type: 'button', class: 'link user-name', 'data-testid': `user-open-${u.username}`, onClick: () => onOpen(u) }, userName(u)),
        self ? badge(t('users.you'), 'accent', { testid: 'user-you' }) : null),
      h('div', { class: 'small muted truncate' }, [u.display_name ? u.username : null, u.email && u.email !== u.username ? u.email : null].filter(Boolean).join(' · ') || ' ')));
}

function roleBadges(u) {
  const out = u.roles.map((r) => badge(t(`users.role.${r}`), ROLE_BADGE[r], { icon: r === 'admin' ? 'shield' : null }));
  if (u.scopes.length) out.push(badge(tn('users.extraScopes', u.scopes.length), 'outline', { title: u.scopes.join(', ') }));
  if (!out.length) out.push(h('span', { class: 'subtle' }, t('users.noRole')));
  return h('div', { class: 'row' }, out);
}

function methodBadges(u) {
  return h('div', { class: 'row' },
    u.has_password ? badge(t('users.method.password'), null, { icon: 'lock' }) : null,
    u.oidc.map((o) => badge(providerName(o.provider), 'info', { icon: 'building', title: t('users.method.ssoLinked', { provider: providerName(o.provider) }) })),
    !u.has_password && !u.oidc.length ? h('span', { class: 'subtle small' }, t('users.method.none')) : null);
}

function mfaBadge(u) {
  if (u.mfa.enabled) return badge(t('users.mfa.on'), 'ok', { icon: 'shield-check' });
  if (!u.has_password) return h('span', { class: 'subtle small', title: t('users.mfa.idpTitle') }, t('users.mfa.idp'));
  return badge(t('users.mfa.off'), 'outline');
}

function stateBadge(u) {
  if (u.disabled) return statusPill('disabled', { label: t('users.state.disabled') });
  if (u.must_change_password) return badge(t('users.state.mustChange'), 'warn', { dot: true });
  return statusPill('ok', { label: t('users.state.active') });
}

// ---------------------------------------------------------------- 役割

/**
 * 役割のカード（ラジオ）と、「個別に選ぶ」のときの権限のチェック。
 * @returns {HTMLElement & {value: () => {roles: string[], scopes: string[]}, preset: () => string}}
 */
function roleChooser({ roles = ['viewer'], scopes = [], canGrant }) {
  const name = nextId('role');
  const initial = roles.length === 1 && !scopes.length ? roles[0] : roles.length || scopes.length ? 'custom' : 'viewer';
  const checks = USER_SCOPES.map((sc) => {
    const allowed = canGrant(sc);
    const input = h('input', { type: 'checkbox', value: sc, checked: scopes.includes(sc) || roles.some((r) => ROLE_SCOPES[r].includes(sc)), disabled: !allowed });
    return { sc, input, el: h('label', { class: ['check', !allowed && 'muted'].filter(Boolean).join(' '), title: allowed ? null : t('users.scopeNotYours') },
      input, h('span', null, h('code', null, sc), ' ', h('span', { class: 'muted small' }, t(`users.scope.${sc}`)))) };
  });
  const custom = h('div', { class: 'scope-list', hidden: initial !== 'custom' }, h('div', { class: 'hint' }, t('users.customHint')), checks.map((c) => c.el));
  const cards = [...ROLES, 'custom'].map((r) => {
    const input = h('input', { type: 'radio', name, value: r, checked: r === initial, 'data-testid': `role-${r}`, disabled: r === 'admin' && !canGrant('tenant:admin') });
    input.addEventListener('change', () => { custom.hidden = r !== 'custom'; });
    return h('label', { class: 'role-card' }, input,
      h('span', { class: 'role-icon' }, icon(ROLE_ICON[r])),
      h('span', { class: 'role-text' },
        h('strong', null, t(r === 'custom' ? 'users.role.custom' : `users.role.${r}`)),
        h('span', { class: 'small muted' }, t(`users.roleDesc.${r}`))));
  });
  const el = h('fieldset', { class: 'role-chooser' }, h('legend', null, t('users.roleLabel')), h('div', { class: 'role-cards' }, cards), custom);
  el.preset = () => el.querySelector(`input[name="${name}"]:checked`).value;
  el.value = () => {
    const p = el.preset();
    if (p !== 'custom') return { roles: [p], scopes: [] };
    return { roles: [], scopes: checks.filter((c) => c.input.checked).map((c) => c.sc) };
  };
  return el;
}

// ---------------------------------------------------------------- ダイアログ

/** 失敗の説明。最後の管理者（409）は専用のダイアログ、権限の不足（403）は言い換える。 */
async function explain(e, u, what) {
  if (e && e.status === 409 && u && isAdminUser(u)) {
    await modal(t('users.lastAdmin.title'), () => [
      h('p', null, t('users.lastAdmin.body', { name: userName(u) })),
      h('p', { class: 'muted' }, t('users.lastAdmin.next')),
    ], { icon: 'shield', kind: 'warn', testid: 'last-admin-explain', actions: (close) => [h('button', { type: 'button', class: 'primary', 'data-testid': 'last-admin-ok', onClick: () => close(true) }, t('users.lastAdmin.ok'))] });
    return;
  }
  if (e && e.status === 403) { toast(t('users.errors.forbidden'), 'error', 9000); return; }
  reportError(e, what);
}

function revealDialog({ user, password, title, lead }) {
  const loginUrl = `${location.origin}/console/`;
  const note = () => t('users.reveal.note', { url: loginUrl, user: user.username, password });
  return modal(title, () => [
    h('p', null, lead),
    alert('warn', t('users.reveal.once'), { small: true }),
    h('div', { class: 'form' },
      h('div', { class: 'field' }, h('div', { class: 'label' }, t('auth.username')), copyField(user.username, t('auth.username'))),
      h('div', { class: 'field' }, h('div', { class: 'label' }, t('users.reveal.password')), copyField(password, t('users.reveal.password'), { testid: 'temp-password' })),
      h('p', { class: 'hint' }, t('users.reveal.mustChange'))),
    button(t('users.reveal.copyNote'), { icon: 'copy', size: 'sm', testid: 'reveal-copy-note', onClick: () => copyText(note(), t('users.reveal.noteLabel')) }),
  ], {
    icon: 'key', kind: 'warn', testid: 'reveal-dialog', dismissible: false,
    actions: (close) => [h('button', { type: 'button', class: 'primary', 'data-testid': 'reveal-done', onClick: () => close(true) }, t('users.reveal.done'))],
  });
}

function createDialog({ canGrant, sso }) {
  const username = h('input', { type: 'text', autocomplete: 'off', autocapitalize: 'off', spellcheck: 'false', maxlength: '64', 'data-testid': 'user-username' });
  const display = h('input', { type: 'text', autocomplete: 'off', maxlength: '128', 'data-testid': 'user-display-name' });
  const email = h('input', { type: 'email', autocomplete: 'off', maxlength: '254', 'data-testid': 'user-email' });
  const roles = roleChooser({ roles: ['viewer'], canGrant });
  const modeName = nextId('pwmode');
  const pw = passwordInput({ autocomplete: 'new-password', testid: 'user-password' });
  const meter = strengthMeter(pw.input, () => ({ username: username.value, email: email.value }));
  const mustChange = h('input', { type: 'checkbox', checked: true, 'data-testid': 'user-must-change' });
  const pwId = nextId('f');
  pw.input.id = pwId;
  const setBox = h('div', { class: 'stack', hidden: true },
    h('div', { class: 'field' }, h('label', { for: pwId }, t('auth.password')), pw.el, pw.caps, meter));
  const mustBox = h('label', { class: 'check' }, mustChange, t('users.create.mustChange'));
  const modes = [['temp', t('users.create.pwTemp'), t('users.create.pwTempHint')], ['set', t('users.create.pwSet'), t('users.create.pwSetHint')],
    ['none', t('users.create.pwNone'), sso ? t('users.create.pwNoneHint') : t('users.create.pwNoneNoSso')]];
  const modeInputs = modes.map(([v]) => h('input', { type: 'radio', name: modeName, value: v, checked: v === 'temp', 'data-testid': `pwmode-${v}` }));
  const mode = () => modeInputs.find((i) => i.checked).value;
  const syncMode = () => { setBox.hidden = mode() !== 'set'; mustBox.hidden = mode() === 'none'; };
  modeInputs.forEach((i) => i.addEventListener('change', syncMode));
  const err = h('div', { 'aria-live': 'assertive' });
  const key = idemKey();
  const opt = () => h('span', { class: 'opt' }, t('form.optional'));
  const lab = (text, input, { hint, optional } = {}) => {
    input.id = input.id || nextId('f');
    const hid = hint ? `${input.id}-hint` : null;
    if (hid) input.setAttribute('aria-describedby', hid);
    return h('div', { class: 'field' }, h('div', { class: 'field-label-row' }, h('label', { for: input.id }, text), optional ? opt() : null), input, hint ? h('div', { class: 'hint', id: hid }, hint) : null);
  };
  return modal(t('users.create.title'), () => h('form', { class: 'form', novalidate: true, onSubmit: (e) => e.preventDefault() },
    lab(t('auth.username'), username, { hint: t('auth.usernameHint') }),
    h('div', { class: 'cols' }, lab(t('auth.displayName'), display, { optional: true }), lab(t('auth.email'), email, { optional: true, hint: sso ? t('users.create.emailHint') : null })),
    roles,
    h('fieldset', { class: 'pw-modes' }, h('legend', null, t('users.create.signIn')),
      modes.map(([v, label, hint], i) => h('label', { class: 'radio-row' }, modeInputs[i], h('span', null, h('span', null, label), h('small', { class: 'muted' }, hint)))),
      setBox, mustBox),
    err), {
    icon: 'user-plus', wide: true, testid: 'user-create-dialog',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'user-create-submit', onClick: async () => {
        replace(err);
        const problems = [];
        const name = username.value.trim();
        if (!/^[A-Za-z0-9._@+-]{1,64}$/.test(name)) problems.push(t('auth.usernameInvalid'));
        const rv = roles.value();
        if (!rv.roles.length && !rv.scopes.length) problems.push(t('users.needRole'));
        if (mode() === 'set' && !meter.result().ok) problems.push(t('auth.passwordRule'));
        if (problems.length) { replace(err, alert('error', h('ul', null, problems.map((x) => h('li', null, x))), { role: 'alert', small: true })); return; }
        const body = { username: name, ...rv };
        if (!body.scopes.length) delete body.scopes;
        if (!body.roles.length) delete body.roles;
        if (display.value.trim()) body.display_name = display.value.trim();
        if (email.value.trim()) body.email = email.value.trim();
        let temp = null;
        if (mode() === 'temp') { temp = generatePassword(); body.password = temp; body.must_change_password = mustChange.checked; }
        if (mode() === 'set') { body.password = pw.input.value; body.must_change_password = mustChange.checked; }
        ok.disabled = true;
        try {
          const { data } = await users.create(body, { key });
          close({ user: data, temp });
        } catch (e) {
          const text = e.status === 409 ? t('users.create.taken') : e.status === 403 ? t('users.errors.forbidden') : describeError(e);
          replace(err, alert('error', text, { role: 'alert', small: true }));
        } finally {
          ok.disabled = false;
        }
      } }, t('users.create.submit'));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  });
}

async function editDialog(u, { canGrant }) {
  const { data: fresh, etag } = await users.get(u.id);
  const display = h('input', { type: 'text', autocomplete: 'off', maxlength: '128', value: fresh.display_name || '', 'data-testid': 'edit-display-name' });
  const email = h('input', { type: 'email', autocomplete: 'off', maxlength: '254', value: fresh.email || '', 'data-testid': 'edit-email' });
  const roles = roleChooser({ roles: fresh.roles, scopes: fresh.scopes, canGrant });
  const err = h('div', { 'aria-live': 'assertive', 'data-testid': 'edit-error' });
  const ids = [nextId('f'), nextId('f')];
  display.id = ids[0];
  email.id = ids[1];
  return modal(t('users.edit.title', { name: fresh.username }), () => h('form', { class: 'form', novalidate: true, onSubmit: (e) => e.preventDefault() },
    h('div', { class: 'cols' },
      h('div', { class: 'field' }, h('label', { for: ids[0] }, t('auth.displayName')), display),
      h('div', { class: 'field' }, h('label', { for: ids[1] }, t('auth.email')), email)),
    roles,
    h('p', { class: 'hint' }, t('users.edit.rolesNote')),
    err), {
    icon: 'edit', wide: true, testid: 'user-edit-dialog',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'user-edit-submit', onClick: async () => {
        replace(err);
        const rv = roles.value();
        if (!rv.roles.length && !rv.scopes.length) { replace(err, alert('error', t('users.needRole'), { role: 'alert', small: true })); return; }
        const body = {};
        const dn = display.value.trim() || null;
        const em = email.value.trim() || null;
        if (dn !== (fresh.display_name || null)) body.display_name = dn;
        if (em !== (fresh.email || null)) body.email = em;
        const same = (a, b) => a.length === b.length && a.every((x) => b.includes(x));
        if (!same(rv.roles, fresh.roles) || !same(rv.scopes, fresh.scopes)) { body.roles = rv.roles; body.scopes = rv.scopes; }
        if (!Object.keys(body).length) { close(false); return; }
        ok.disabled = true;
        try {
          await users.update(fresh.id, body, etag);
          close(true);
        } catch (e) {
          let text;
          if (e.status === 409 && isAdminUser(fresh) && body.roles) text = [h('strong', null, t('users.lastAdmin.title')), h('br'), t('users.lastAdmin.body', { name: userName(fresh) }), ' ', t('users.lastAdmin.next')];
          else if (e.status === 409) text = t('users.create.taken');
          else if (e.status === 403) text = t('users.errors.forbidden');
          else if (e.status === 412) text = t('users.errors.changed');
          else text = describeError(e);
          replace(err, alert(e.status === 409 && body.roles ? 'warn' : 'error', text, { role: 'alert', testid: e.status === 409 ? 'last-admin-inline' : null }));
        } finally {
          ok.disabled = false;
        }
      } }, t('common.save'));
      return [h('button', { type: 'button', onClick: () => close(false) }, t('common.cancel')), ok];
    },
  });
}

function resetPasswordDialog(u) {
  const modeName = nextId('pwmode');
  const inputs = ['temp', 'set'].map((v) => h('input', { type: 'radio', name: modeName, value: v, checked: v === 'temp', 'data-testid': `reset-mode-${v}` }));
  const pw = passwordInput({ autocomplete: 'new-password', testid: 'reset-password' });
  const meter = strengthMeter(pw.input, () => ({ username: u.username, email: u.email }));
  const pwId = nextId('f');
  pw.input.id = pwId;
  const setBox = h('div', { class: 'field', hidden: true }, h('label', { for: pwId }, t('auth.newPassword')), pw.el, pw.caps, meter);
  inputs.forEach((i) => i.addEventListener('change', () => { setBox.hidden = !inputs[1].checked; }));
  const must = h('input', { type: 'checkbox', checked: true, 'data-testid': 'reset-must-change' });
  const err = h('div', { 'aria-live': 'assertive' });
  return modal(t('users.reset.title', { name: userName(u) }), () => h('form', { class: 'form', novalidate: true, onSubmit: (e) => e.preventDefault() },
    h('div', { class: 'consequence' }, h('p', null, t('users.reset.body')), h('ul', null, h('li', null, t('users.reset.c1')), h('li', null, u.mfa.enabled ? t('users.reset.c2mfa') : t('users.reset.c2')))),
    h('fieldset', { class: 'pw-modes' },
      h('legend', { class: 'visually-hidden' }, t('users.reset.how')),
      h('label', { class: 'radio-row' }, inputs[0], h('span', null, h('span', null, t('users.create.pwTemp')), h('small', { class: 'muted' }, t('users.reset.tempHint')))),
      h('label', { class: 'radio-row' }, inputs[1], h('span', null, h('span', null, t('users.create.pwSet')), h('small', { class: 'muted' }, t('users.create.pwSetHint')))),
      setBox,
      h('label', { class: 'check' }, must, t('users.create.mustChange'))),
    err), {
    icon: 'key', testid: 'reset-dialog',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'reset-submit', onClick: async () => {
        replace(err);
        const set = inputs[1].checked;
        if (set && !meter.result().ok) { replace(err, alert('error', t('auth.passwordRule'), { role: 'alert', small: true })); return; }
        ok.disabled = true;
        try {
          const { data } = await users.resetPassword(u.id, { password: set ? pw.input.value : undefined, mustChange: must.checked });
          close({ user: data.user, temp: data.temporary_password });
        } catch (e) {
          replace(err, alert('error', e.status === 403 ? t('users.errors.forbidden') : describeError(e), { role: 'alert', small: true }));
        } finally {
          ok.disabled = false;
        }
      } }, t('users.reset.submit'));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  });
}

function sessionsDialog(u) {
  const box = h('div');
  const refresh = () => load(box, async () => {
    const { data } = await users.sessions(u.id);
    return sessionsTable(data.items, { testid: 'user-sessions-table' });
  }, { kind: 'table' });
  refresh();
  return modal(t('users.sessions.title', { name: userName(u) }), () => [h('p', { class: 'muted' }, t('users.sessions.lead')), box], {
    icon: 'laptop', wide: true, testid: 'user-sessions-dialog',
    actions: (close) => [
      h('button', { type: 'button', onClick: () => close(false) }, t('common.close')),
      h('button', { type: 'button', class: 'danger', 'data-testid': 'user-sessions-revoke', onClick: async () => {
        try {
          await users.revokeSessions(u.id);
          toast(t('users.sessions.revoked', { name: userName(u) }), 'ok');
          refresh();
        } catch (e) {
          explain(e, u, t('users.sessions.revokeAll'));
        }
      } }, icon('logout'), t('users.sessions.revokeAll')),
    ],
  });
}

// ---------------------------------------------------------------- 画面

const FILTERS = {
  role: ['all', 'admin', 'operator', 'viewer', 'custom'],
  state: ['all', 'active', 'disabled', 'mustChange'],
  mfa: ['all', 'on', 'off'],
  method: ['all', 'password', 'sso'],
};

/** 絞り込み（画面に依存しない。試験でも使う）。 */
export function filterUsers(list, { q = '', role = 'all', state = 'all', mfa = 'all', method = 'all' } = {}) {
  const needle = q.trim().toLowerCase();
  return list.filter((u) => {
    if (needle && ![u.username, u.display_name, u.email].some((x) => x && x.toLowerCase().includes(needle))) return false;
    if (role !== 'all' && (role === 'custom' ? !['custom', 'none'].includes(rolePreset(u)) : !u.roles.includes(role))) return false;
    if (state === 'active' && u.disabled) return false;
    if (state === 'disabled' && !u.disabled) return false;
    if (state === 'mustChange' && !u.must_change_password) return false;
    if (mfa === 'on' && !u.mfa.enabled) return false;
    if (mfa === 'off' && (u.mfa.enabled || !u.has_password)) return false;
    if (method === 'password' && !u.has_password) return false;
    if (method === 'sso' && !u.oidc.length) return false;
    return true;
  });
}

export async function render(root) {
  const me = getMe();
  if (hasScope('tenant:admin') === false) {
    root.append(
      pageHeader({ title: t('nav.users'), description: t('users.lead') }),
      h('section', { class: 'panel' }, emptyState({ icon: 'lock', title: t('users.noAccess'), body: t('users.noAccessBody'), testid: 'users-no-access' })));
    return null;
  }
  const canGrant = (sc) => hasScope(sc) !== false;
  let list = [];
  let sso = false;
  const f = { q: '', role: 'all', state: 'all', mfa: 'all', method: 'all' };
  const box = h('div');
  const summary = h('div', { class: 'users-summary small muted', 'aria-live': 'polite', 'data-testid': 'users-summary' });

  const open = async (u) => {
    try {
      if (await editDialog(u, { canGrant })) { toast(t('users.edit.saved', { name: userName(u) }), 'ok'); refresh(); }
    } catch (e) { explain(e, u, t('users.edit.action')); }
  };

  const add = async () => {
    const r = await createDialog({ canGrant, sso });
    if (!r) return;
    refresh();
    if (r.temp) await revealDialog({ user: r.user, password: r.temp, title: t('users.reveal.createdTitle'), lead: t('users.reveal.createdLead', { name: userName(r.user) }) });
    else toast(t('users.create.done', { name: userName(r.user) }), 'ok');
  };

  const setDisabled = async (u, disabled) => {
    if (disabled) {
      const ok = await confirmDialog(t('users.disable.title', { name: userName(u) }), t('users.disable.body'), {
        ok: t('users.disable.ok'), danger: false, icon: 'ban', consequences: [t('users.disable.c1'), t('users.disable.c2')], testid: 'disable-dialog',
      });
      if (!ok) return;
    }
    try {
      const { etag } = await users.get(u.id);
      await users.update(u.id, { disabled }, etag);
      toast(disabled ? t('users.disable.done', { name: userName(u) }) : t('users.enable.done', { name: userName(u) }), 'ok');
      refresh();
    } catch (e) { explain(e, u, disabled ? t('users.disable.ok') : t('users.enable.ok')); }
  };

  const remove = async (u) => {
    const ok = await confirmDialog(t('users.delete.title', { name: userName(u) }), t('users.delete.body'), {
      ok: t('users.delete.ok'), requireText: u.username, consequences: [t('users.delete.c1'), t('users.delete.c2'), t('users.delete.c3')], testid: 'delete-dialog',
    });
    if (!ok) return;
    try {
      await users.remove(u.id);
      toast(t('users.delete.done', { name: userName(u) }), 'ok');
      refresh();
    } catch (e) { explain(e, u, t('users.delete.ok')); }
  };

  const resetPw = async (u) => {
    const r = await resetPasswordDialog(u);
    if (!r) return;
    refresh();
    if (r.temp) await revealDialog({ user: r.user, password: r.temp, title: t('users.reveal.resetTitle'), lead: t('users.reveal.resetLead', { name: userName(r.user) }) });
    else toast(t('users.reset.done', { name: userName(u) }), 'ok');
  };

  const resetMfa = async (u) => {
    const ok = await confirmDialog(t('users.mfaReset.title', { name: userName(u) }), t('users.mfaReset.body'), {
      ok: t('users.mfaReset.ok'), danger: false, icon: 'shield-off', consequences: [t('users.mfaReset.c1'), t('users.mfaReset.c2')], testid: 'mfa-reset-dialog',
    });
    if (!ok) return;
    try {
      await users.resetMfa(u.id);
      toast(t('users.mfaReset.done', { name: userName(u) }), 'ok');
      refresh();
    } catch (e) { explain(e, u, t('users.mfaReset.ok')); }
  };

  const actions = (u) => {
    const self = me && me.user && me.user.id === u.id;
    const b = iconButton('more', t('users.actionsFor', { name: userName(u) }), { size: 'sm', testid: `user-actions-${u.username}` });
    attachMenu(b, () => [
      { label: t('users.edit.action'), icon: 'edit', testid: 'user-edit', onSelect: () => open(u) },
      { label: u.has_password ? t('users.reset.action') : t('users.reset.actionSet'), icon: 'key', testid: 'user-reset-password', disabled: self, onSelect: () => resetPw(u) },
      u.mfa.enabled ? { label: t('users.mfaReset.ok'), icon: 'shield-off', testid: 'user-reset-mfa', disabled: self, onSelect: () => resetMfa(u) } : null,
      { label: t('users.sessions.action'), icon: 'laptop', testid: 'user-sessions', onSelect: () => sessionsDialog(u).then(() => refresh()) },
      { separator: true },
      u.disabled
        ? { label: t('users.enable.ok'), icon: 'user-check', testid: 'user-enable', onSelect: () => setDisabled(u, false) }
        : { label: t('users.disable.ok'), icon: 'ban', testid: 'user-disable', disabled: self, onSelect: () => setDisabled(u, true) },
      { label: t('users.delete.ok'), icon: 'trash', danger: true, testid: 'user-delete', disabled: self, onSelect: () => remove(u) },
      self ? { node: h('p', { class: 'small muted menu-note' }, t('users.selfNote')) } : null,
    ]);
    return b;
  };

  const table = dataTable({
    testid: 'users-table',
    caption: t('nav.users'),
    sort: { key: 'name', dir: 'asc' },
    rowKey: (u) => u.id,
    empty: { icon: 'filter', title: t('users.noMatch'), body: t('users.noMatchBody') },
    columns: [
      { key: 'name', label: t('users.col.user'), primary: true, sort: (u) => userName(u).toLowerCase(), render: (u) => userCell(u, { me, onOpen: open }) },
      { key: 'role', label: t('users.col.role'), wrap: true, sort: (u) => (isAdminUser(u) ? 0 : u.roles.includes('operator') ? 1 : 2), render: roleBadges },
      { label: t('users.col.method'), wrap: true, render: methodBadges },
      { key: 'mfa', label: t('users.col.mfa'), sort: (u) => (u.mfa.enabled ? 0 : 1), render: mfaBadge },
      { key: 'state', label: t('users.col.state'), sort: (u) => (u.disabled ? 2 : u.must_change_password ? 1 : 0), render: stateBadge },
      { key: 'login', label: t('users.col.lastLogin'), sort: (u) => u.last_login_at || '', render: (u) => (u.last_login_at ? h('span', { title: fmtTime(u.last_login_at) }, fmtRelative(u.last_login_at)) : h('span', { class: 'subtle' }, t('users.never'))) },
      { label: t('common.actions'), actions: true, render: actions },
    ],
    rows: [],
  });

  const draw = () => {
    const rows = filterUsers(list, f);
    const withPw = list.filter((u) => u.has_password && !u.disabled);
    replace(summary,
      tn('users.summary.count', list.length), ' · ',
      tn('users.summary.admins', list.filter(isAdminUser).length), ' · ',
      t('users.summary.mfa', { on: withPw.filter((u) => u.mfa.enabled).length, total: withPw.length }),
      rows.length !== list.length ? [' · ', h('strong', null, tn('users.summary.shown', rows.length))] : null);
    table.update(rows);
  };

  // 2 回目からは表だけを描き直す（検索の欄のフォーカスと入力をそのままにする）
  let content = null;
  const empty = () => emptyState({ icon: 'users', title: t('users.emptyTitle'), body: t('users.emptyBody'), actions: [button(t('users.add'), { variant: 'primary', icon: 'user-plus', onClick: add })] });
  const refresh = async () => {
    if (content && content.isConnected) {
      try {
        list = await users.listAll();
        if (list.length) draw();
        else { replace(box, empty()); content = null; }
      } catch (e) {
        reportError(e, t('users.loadFailed'), { retry: refresh });
      }
      return;
    }
    await load(box, async () => {
      list = await users.listAll();
      if (!list.length) return empty();
      draw();
      content = h('div', null, toolbar, table);
      return content;
    }, { kind: 'table' });
  };

  const sel = (key) => {
    const s = h('select', { 'aria-label': t(`users.filter.${key}`), 'data-testid': `users-filter-${key}`, class: 'sm' },
      FILTERS[key].map((v) => h('option', { value: v }, t(`users.filter.${key}.${v}`))));
    s.addEventListener('change', () => { f[key] = s.value; syncClear(); draw(); });
    return s;
  };
  const selects = Object.keys(FILTERS).map((k) => [k, sel(k)]);
  const search = searchBox({ placeholder: t('users.search'), shortcut: true, testid: 'users-search', onInput: debounce((v) => { f.q = v; syncClear(); draw(); }, 120) });
  const clearBtn = button(t('users.filter.clear'), { variant: 'link', size: 'sm', testid: 'users-filter-clear', onClick: () => {
    Object.assign(f, { q: '', role: 'all', state: 'all', mfa: 'all', method: 'all' });
    search.input.value = '';
    search.input.dispatchEvent(new Event('input'));
    selects.forEach(([, s]) => { s.value = 'all'; });
    syncClear();
    draw();
  } });
  const syncClear = () => { clearBtn.hidden = !(f.q || f.role !== 'all' || f.state !== 'all' || f.mfa !== 'all' || f.method !== 'all'); };
  syncClear();
  const toolbar = h('div', { class: 'users-toolbar' },
    search,
    h('div', { class: 'users-filters', role: 'group', 'aria-label': t('users.filter.label') }, selects.map(([k, s]) => h('label', { class: 'filter' }, h('span', { class: 'small muted' }, t(`users.filter.${k}`)), s))),
    clearBtn,
    h('div', { class: 'spacer' }),
    summary);

  root.append(
    pageHeader({
      title: t('nav.users'),
      description: t('users.lead'),
      help: helpHint({ title: t('nav.users'), body: t('users.help'), guide: '12-security.md' }),
      actions: [
        iconButton('refresh', t('common.refresh'), { variant: 'secondary', testid: 'users-refresh', onClick: () => refresh() }),
        button(t('users.add'), { variant: 'primary', icon: 'user-plus', kbd: 'N', testid: 'users-create', onClick: add }),
      ],
    }),
    h('section', { class: 'panel users-panel' }, box));
  auth.providers().then(({ data }) => { sso = data.oidc.length > 0; }).catch(() => {});
  await refresh();
  const q = new URLSearchParams(location.hash.split('?')[1] || '');
  if (q.get('new') === '1' || q.get('user')) {
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/users`);
    if (q.get('new') === '1') add();
    else { const u = list.find((x) => x.id === q.get('user')); if (u) open(u); }
  }
  const offs = [
    onPageKey('n', add),
    refreshOn(['user.'], () => refresh()),
  ];
  return () => offs.forEach((off) => off());
}
