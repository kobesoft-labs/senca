// ログイン（/auth）と利用者の管理（/users）の操作（ADR-0017、docs/api/openapi.yaml）。
// ログインの前に使うもの（providers・login・exchange）は quiet401（401 で画面を戻さない・入り直さない）。

import { get, post, patch, del, request } from './client.js';
import { idemKey } from '../lib/format.js';

const enc = encodeURIComponent;
const quiet = { quiet401: true };

export const auth = {
  /** ログインの方法（認証なし）: {password, mfa_required, setup_required, oidc:[{id, display_name, start_url}]} */
  providers: () => get('/auth/providers', quiet),
  /** {status:'ok', token, …} か {status:'mfa_required', mfa_token, expires_at, methods} */
  login: ({ username, password, tenant }) => post('/auth/login', tenant ? { username, password, tenant } : { username, password }, quiet),
  loginMfa: ({ mfaToken, code, recoveryCode }) => post('/auth/login/mfa', code ? { mfa_token: mfaToken, code } : { mfa_token: mfaToken, recovery_code: recoveryCode }, quiet),
  /** OIDC の一度きりのコード（クッキー senca_oidc と組）をセッションに替える。 */
  exchange: (code) => post('/auth/exchange', { code }, { ...quiet, credentials: 'same-origin' }),
  /** 自分の情報（どの認証方法でも。pending のセッションでも使える）。token を渡すとその値で確かめる。 */
  me: (o = {}) => get('/auth/me', o),
  logout: () => post('/auth/logout', undefined, { ...quiet, idempotencyKey: idemKey() }),
  changePassword: (current, next) => post('/auth/password', { current_password: current, new_password: next }, { idempotencyKey: idemKey() }),
  totpSetup: (password) => post('/auth/mfa/totp/setup', { password }, { idempotencyKey: idemKey() }),
  totpConfirm: (enrollmentToken, code) => post('/auth/mfa/totp/confirm', { enrollment_token: enrollmentToken, code }, { idempotencyKey: idemKey() }),
  /** 本人確認は {password} か {code}（TOTP・回復コード）。 */
  mfaDisable: (proof) => post('/auth/mfa/disable', proof, { idempotencyKey: idemKey() }),
  recoveryCodes: (proof) => post('/auth/mfa/recovery-codes', proof, { idempotencyKey: idemKey() }),
  sessions: ({ limit, cursor } = {}) => get('/auth/sessions', { query: { limit, cursor } }),
  revokeSession: (id) => del(`/auth/sessions/${enc(id)}`),
};

export const users = {
  list: ({ limit, cursor } = {}) => get('/users', { query: { limit, cursor } }),
  /** 全部（カーソルで最後まで）。 */
  listAll: async () => {
    const out = [];
    let cursor;
    for (let i = 0; i < 20; i++) {
      const { data } = await get('/users', { query: { limit: 200, cursor } });
      out.push(...data.items);
      if (!data.next_cursor) break;
      cursor = data.next_cursor;
    }
    return out;
  },
  get: (id) => get(`/users/${enc(id)}`),
  /** token を渡すと、その値で作る（最初の管理者を最初の管理者トークンで作るとき）。 */
  create: (body, { token, key = idemKey() } = {}) => post('/users', body, { idempotencyKey: key, token, quiet401: token !== undefined }),
  update: (id, body, etag) => patch(`/users/${enc(id)}`, body, etag),
  remove: (id, etag) => del(`/users/${enc(id)}`, { ifMatch: etag }),
  /** password を省くと仮のパスワードをノードが作る（応答の temporary_password）。 */
  resetPassword: (id, { password, mustChange = true } = {}) => post(`/users/${enc(id)}/password`, password ? { password, must_change_password: mustChange } : { must_change_password: mustChange }, { idempotencyKey: idemKey() }),
  resetMfa: (id) => del(`/users/${enc(id)}/mfa`),
  sessions: (id) => get(`/users/${enc(id)}/sessions`, { query: { limit: 100 } }),
  revokeSessions: (id) => del(`/users/${enc(id)}/sessions`),
};

/** 役割と権限（openapi の Role・Scope）。 */
export const ROLES = ['admin', 'operator', 'viewer'];
export const ROLE_SCOPES = {
  admin: ['tenant:admin'],
  operator: ['stream:read', 'stream:write', 'publish', 'subscribe', 'recording:read'],
  viewer: ['stream:read', 'recording:read', 'subscribe'],
};
export const USER_SCOPES = ['stream:read', 'stream:write', 'publish', 'subscribe', 'recording:read', 'recording:delete', 'tenant:admin', 'node:admin'];

/** 利用者の役割の見え方: 役割が 1 つで個別の権限が無ければその役割、ほかは 'custom'。 */
export function rolePreset(u) {
  if (u.roles.length === 1 && !u.scopes.length) return u.roles[0];
  if (!u.roles.length && !u.scopes.length) return 'none';
  return 'custom';
}

/** 管理者か（有効な tenant:admin）。 */
export function isAdminUser(u) {
  return !!u && !u.disabled && (u.effective_scopes || []).includes('tenant:admin');
}

export { request };
