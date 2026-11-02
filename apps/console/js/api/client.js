// 管理 API（/api/v1）の呼び出し。認証は Bearer トークン（sessionStorage に置く。ログには出さない）。
//
// 失敗は ApiError にする（problem+json の title・code・request_id・retryable・Retry-After を持つ）。
// 401 は bus に `unauthorized` を流し、画面はログインへ戻る。

import { emit } from '../lib/bus.js';

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

const STATUS_TEXT = {
  0: 'ノードに接続できません',
  400: '要求の形が正しくありません',
  401: '認証に失敗しました',
  403: '権限がありません',
  404: '見つかりません',
  405: 'この操作はできません',
  409: '状態が衝突しています',
  412: '他の操作で更新されています',
  413: '大きすぎます',
  415: '本文の形式が違います',
  422: '処理できない内容です',
  428: 'If-Match が必要です',
  429: '要求が多すぎます',
  500: 'ノードの内部エラー',
  502: '中継のエラー',
  503: '今は処理できません',
};

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
    title: problem.title || STATUS_TEXT[res.status] || res.statusText,
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
 * @param {boolean} [opt.quiet401] 401 で画面を戻さない
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
    res = await fetch(url, { method, headers, body, signal: opt.signal, cache: 'no-store', credentials: 'omit' });
  } catch (e) {
    if (e.name === 'AbortError') throw e;
    throw new ApiError({ status: 0, title: STATUS_TEXT[0], method, path });
  }
  if (!res.ok) {
    const err = await toError(res, method, path);
    if (res.status === 401 && !opt.quiet401) emit('unauthorized', err);
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
