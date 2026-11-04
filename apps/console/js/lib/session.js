// 今ログインしている主体（GET /auth/me の答え）と、その見せ方。外枠・アカウント・ユーザーの画面が使う。
//
// /auth/me はセッション・API トークン・JWT のどれでも使える。答えが得られない古いノードでは、
// JWT の claim（無ければ API トークンの末尾 4 文字）から分かるだけを入れる（scopes は null = 分からない）。

import { emit } from './bus.js';
import { getToken } from '../api/client.js';

let me = null;
let providerNames = {};

/** IdP の表示名（GET /auth/providers の oidc）を覚える。 */
export function setProviderNames(list) {
  providerNames = Object.fromEntries((list || []).map((p) => [p.id, p.display_name || p.id]));
}

/** IdP の ID から表示名（分からなければ ID）。 */
export function providerName(id) {
  return providerNames[id] || id;
}

/** 今の主体（{tenant, subject, auth_method, scopes, user, session, pending}）か null。 */
export function getMe() {
  return me;
}

export function setMe(next) {
  me = next || null;
  emit('me', me);
}

/** /auth/me が無いときの代わり（JWT の claim か API トークンの末尾）。 */
export function guessMe(tok = getToken()) {
  if (!tok) return null;
  const parts = tok.split('.');
  if (parts.length === 3) {
    try {
      const json = JSON.parse(atob(parts[1].replace(/-/g, '+').replace(/_/g, '/')));
      const scopes = Array.isArray(json.scopes) ? json.scopes : typeof json.scope === 'string' ? json.scope.split(/\s+/).filter(Boolean) : null;
      return { tenant: json.tenant || json.tenant_id || json.org || null, subject: json.sub || 'jwt', auth_method: 'jwt', scopes, user: null, session: null, pending: [], name: json.name || json.preferred_username || json.email || null, guessed: true };
    } catch { /* API トークンとして扱う */ }
  }
  return { tenant: null, subject: `senca_…${tok.slice(-4)}`, auth_method: tok.startsWith('sencas_') ? 'session' : 'api_token', scopes: null, user: null, session: null, pending: [], guessed: true };
}

/** 権限を持つか。tenant:admin は node:admin 以外をすべて含む。scopes が分からなければ null。 */
export function hasScope(scope, who = me) {
  if (!who || !Array.isArray(who.scopes)) return null;
  if (who.scopes.includes(scope)) return true;
  return scope !== 'node:admin' && who.scopes.includes('tenant:admin');
}

/** 見せる名前（表示名 → 利用者名 → 主体）。 */
export function displayName(who = me) {
  if (!who) return '—';
  if (who.user) return who.user.display_name || who.user.username;
  return who.name || who.subject || '—';
}

/** 頭文字（アバター用）。日本語などは最初の 1 文字、英字は単語の頭を 2 つまで。 */
export function initials(name) {
  const s = String(name || '').trim();
  if (!s) return '?';
  const words = s.split(/[\s._@+-]+/).filter(Boolean);
  if (/^[A-Za-z]/.test(s) && words.length > 1) return (words[0][0] + words[1][0]).toUpperCase();
  return [...s][0].toUpperCase();
}

/**
 * ログインの方法の鍵（辞書の `auth.method.*`）と、IdP の ID。
 * @returns {{key: 'password'|'passwordMfa'|'sso'|'apiToken'|'jwt'|'session', provider: string|null}}
 */
export function signInMethod(who = me) {
  if (!who) return { key: 'session', provider: null };
  if (who.auth_method === 'api_token') return { key: 'apiToken', provider: null };
  if (who.auth_method === 'jwt') return { key: 'jwt', provider: null };
  const s = who.session;
  if (s && s.method === 'oidc') return { key: 'sso', provider: s.provider ? providerName(s.provider) : null };
  if (s && s.method === 'password') return { key: s.mfa ? 'passwordMfa' : 'password', provider: null };
  return { key: 'session', provider: null };
}

// ---------------------------------------------------------------- ログインの後に開くところ
const NEXT_KEY = 'senca.console.next';

/** 管理画面の中のハッシュ（#/…）だけを受け付ける。ログイン・完了の画面は戻り先にしない。 */
export function safeHash(h) {
  if (typeof h !== 'string' || !h.startsWith('#/')) return null;
  if (/^#\/auth\//.test(h) || /[\s\\]/.test(h) || h.length > 400) return null;
  return h;
}

/** OIDC の return_to（/console/#/…）からハッシュを取り出す。 */
export function hashFromReturnTo(r) {
  if (typeof r !== 'string') return null;
  const m = r.match(/^\/console\/?(#\/.*)?$/);
  if (!m) return null;
  return safeHash(m[1] || '#/') || '#/';
}

/** OIDC の start に渡す return_to（今のハッシュ）。 */
export function returnToFor(hash = location.hash) {
  return `/console/${safeHash(hash) || '#/'}`;
}

/** IdP へ移る前に、どの IdP で始めたかを覚える（失敗の画面の「もう一度」に使う。値は秘密ではない）。 */
export function rememberProvider(id) {
  try { sessionStorage.setItem(NEXT_KEY, JSON.stringify({ provider: id, at: Date.now() })); } catch { /* 無くてもよい */ }
}

/** 覚えた IdP を取り出して消す（10 分より古いものは使わない）。 */
export function takeProvider() {
  try {
    const v = JSON.parse(sessionStorage.getItem(NEXT_KEY) || 'null');
    sessionStorage.removeItem(NEXT_KEY);
    return v && Date.now() - v.at < 600000 ? v.provider : null;
  } catch { return null; }
}
