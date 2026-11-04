// 管理 API（/api/v1）の呼び出し。認証は Bearer トークン（sessionStorage に置く。ログには出さない）。
//
// 失敗は ApiError にする（problem+json の title・code・request_id・retryable・Retry-After を持つ）。
// 401: 入り直しの処理（setReauthHandler、main.js がダイアログを出す）があれば、入り直すまで待ってから 1 回だけ送り直す
//      （画面の状態を残すため）。無ければ bus に `unauthorized` を流し、画面はログインへ戻る。
// 403 の details.reason が "pending"（パスワードの変更・MFA の登録が先）なら bus に `pending` を流す。

import { emit } from '../lib/bus.js';
import { t } from '../i18n/index.js';

export const BASE = '/api/v1';
const TOKEN_KEY = 'senca.console.token';

export function getToken() {
  try { return sessionStorage.getItem(TOKEN_KEY); } catch { return null; }
}
export function setToken(t) {
  sessionStorage.setItem(TOKEN_KEY, t);
}
export function clearToken() {
  try { sessionStorage.removeItem(TOKEN_KEY); } catch { /* 何もしない */ }
}
/** ログインのセッション（`sencas_`）か（API トークン・JWT ではない）。 */
export function isSessionToken(tok = getToken()) {
  return !!tok && tok.startsWith('sencas_');
}

let reauthHandler = null;
/** 401 のときに呼ぶ関数（入り直せたら true で解決する Promise を返す）。null で外す。 */
export function setReauthHandler(fn) {
  reauthHandler = fn;
}

export class ApiError extends Error {
  constructor({ status, title, code, requestId, retryable, retryAfter, details, type, method, path }) {
    super(title || `HTTP ${status}`);
    this.name = 'ApiError';
    this.status = status;
    this.title = title || `HTTP ${status}`;
    this.code = code || null;
    this.requestId = requestId || null;
    this.retryable = !!retryable;
    this.retryAfter = retryAfter ?? null;
    this.details = details ?? null;
    this.type = type || null;
    this.method = method;
    this.path = path;
  }
}

function buildUrl(path, query) {
  const u = new URL(BASE + path, location.origin);
  if (query) {
    for (const [k, v] of Object.entries(query)) {
      if (v !== undefined && v !== null && v !== '') u.searchParams.set(k, String(v));
    }
  }
  return u.pathname + u.search;
}

const KNOWN = [0, 400, 401, 403, 404, 405, 409, 412, 413, 415, 422, 428, 429, 500, 502, 503];
/** 状態コードの短い説明（problem に title が無いとき）。 */
export function statusText(status) {
  return KNOWN.includes(status) ? t(`http.${status}`) : null;
}

async function toError(res, method, path) {
  const ct = res.headers.get('content-type') || '';
  let problem = {};
  if (ct.includes('json')) {
    try { problem = await res.json(); } catch { problem = {}; }
  } else {
    try { await res.text(); } catch { /* 本文は使わない */ }
  }
  const ra = res.headers.get('retry-after');
  return new ApiError({
    status: res.status,
    title: problem.title || statusText(res.status) || res.statusText,
    code: problem.code,
    requestId: problem.request_id || res.headers.get('x-request-id'),
    retryable: problem.retryable,
    retryAfter: ra !== null && ra !== '' && !Number.isNaN(Number(ra)) ? Number(ra) : null,
    details: problem.details,
    type: problem.type,
    method,
    path,
  });
}

/**
 * 管理 API を呼ぶ。
 * @param {string} method
 * @param {string} path `/streams` など（/api/v1 は付けない）
 * @param {object} [opt]
 * @param {object} [opt.body] JSON の本文
 * @param {object} [opt.query]
 * @param {string} [opt.ifMatch] ETag
 * @param {string} [opt.idempotencyKey]
 * @param {'json'|'blob'|'response'} [opt.as]
 * @param {AbortSignal} [opt.signal]
 * @param {string} [opt.token] 使うトークン（ログインの確認用）
 * @param {boolean} [opt.quiet401] 401 で画面を戻さない（入り直しもしない）
 * @param {'omit'|'same-origin'} [opt.credentials] クッキーを送るか（OIDC のコードの交換だけ same-origin）
 * @returns {Promise<{status:number, data:any, etag:string|null, headers:Headers}>}
 */
export async function request(method, path, opt = {}) {
  const token = opt.token ?? getToken();
  const headers = { Accept: opt.as === 'blob' ? '*/*' : 'application/json' };
  if (token) headers.Authorization = `Bearer ${token}`;
  if (opt.ifMatch) headers['If-Match'] = opt.ifMatch;
  if (opt.idempotencyKey) headers['Idempotency-Key'] = opt.idempotencyKey;
  let body;
  if (opt.body !== undefined) {
    headers['Content-Type'] = 'application/json';
    body = JSON.stringify(opt.body);
  }
  const url = buildUrl(path, opt.query);
  let res;
  try {
    res = await fetch(url, { method, headers, body, signal: opt.signal, cache: 'no-store', credentials: opt.credentials || 'omit' });
  } catch (e) {
    if (e.name === 'AbortError') throw e;
    throw new ApiError({ status: 0, title: statusText(0), method, path });
  }
  if (!res.ok) {
    const err = await toError(res, method, path);
    if (res.status === 401 && !opt.quiet401 && opt.token === undefined && token) {
      // 期限切れ・失効。入り直せたら同じ要求を 1 回だけ送り直す（401 の要求は処理されていないので、POST でも重ならない）
      // 送った後で別の要求が入り直していた（値が変わった）なら、待たずに送り直す
      if (!opt.retried && getToken() && getToken() !== token) return request(method, path, { ...opt, retried: true });
      if (reauthHandler && !opt.retried && await reauthHandler(err)) return request(method, path, { ...opt, retried: true });
      if (!reauthHandler) emit('unauthorized', err);
    }
    if (res.status === 403 && err.details && err.details.reason === 'pending') emit('pending', err.details.pending || []);
    throw err;
  }
  const etag = res.headers.get('etag');
  if (opt.as === 'response') return { status: res.status, data: res, etag, headers: res.headers };
  if (opt.as === 'blob') return { status: res.status, data: await res.blob(), etag, headers: res.headers };
  if (res.status === 204) return { status: 204, data: null, etag, headers: res.headers };
  const ct = res.headers.get('content-type') || '';
  const data = ct.includes('json') ? await res.json() : await res.text();
  return { status: res.status, data, etag, headers: res.headers };
}

export const get = (p, o) => request('GET', p, o);
export const post = (p, body, o = {}) => request('POST', p, { ...o, body });
export const patch = (p, body, ifMatch, o = {}) => request('PATCH', p, { ...o, body, ifMatch });
export const del = (p, o) => request('DELETE', p, o);
