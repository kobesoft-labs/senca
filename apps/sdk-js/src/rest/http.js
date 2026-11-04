// 管理 API の HTTP の芯: 認証・problem+json → ApiError・再試行・Idempotency-Key・If-Match・ETag。

import { ApiError, abortReason, isAbortError } from '../errors.js';
import { backoffDelay, linkedController, sleep } from '../internal/async.js';

/**
 * 再試行の方針（`false` で再試行しない）。
 * @typedef {object} RetryOptions
 * @property {number} [maxAttempts] 最初の 1 回を含む回数（既定 3）
 * @property {number} [initialDelayMs] 既定 200
 * @property {number} [maxDelayMs] 既定 5000
 * @property {number} [factor] 既定 2
 * @property {number} [jitter] 既定 0.2
 */

/**
 * @typedef {object} ClientOptions
 * @property {string} baseUrl `https://node.example`（`/api/v1` は足す）か `https://node.example/api/v1`
 * @property {string | (() => string | Promise<string>)} [token] API トークン（`senca_...`）・ログインのセッション（`sencas_...`）・JWT。関数なら要求のたびに呼ぶ。
 *   ログインだけをするなら省いてよい（ログインの操作は認証なしで使えます）
 * @property {typeof fetch} [fetch] 既定は globalThis.fetch
 * @property {boolean | RetryOptions} [retry] 既定 true（3 回まで）
 * @property {number} [timeoutMs] 1 回の要求の時間切れ（既定 30000、SSE には使わない）
 * @property {Record<string, string>} [headers] すべての要求に付ける見出し
 * @property {boolean} [idempotency] POST に自動で Idempotency-Key を付ける（既定 true。再試行を安全にする）
 */

/**
 * If-Match の値。ETag の文字列（`"3"`）、generation の数、`generation` を持つ資源のどれか。
 * @typedef {string | number | {generation: number} | {etag: string}} IfMatch
 */

/**
 * 1 回の要求の指定。
 * @typedef {object} RequestOptions
 * @property {Record<string, string | number | boolean | undefined | null>} [query]
 * @property {unknown} [body] JSON にして送る
 * @property {Record<string, string>} [headers]
 * @property {IfMatch} [ifMatch]
 * @property {string | false} [idempotencyKey] POST の Idempotency-Key（既定は自動で作る。`false` で付けない）
 * @property {AbortSignal} [signal]
 * @property {'json' | 'response' | 'blob'} [as] 応答の読み方（既定 json）
 * @property {boolean | RetryOptions} [retry]
 * @property {number} [timeoutMs]
 * @property {RequestCredentials} [credentials] fetch の credentials（既定 `omit`。クッキーを送る `POST /auth/exchange` だけ `same-origin`）
 */

/**
 * 応答（本文と見出し）。
 * @template T
 * @typedef {{status: number, data: T, etag: string | null, headers: Headers, requestId: string | null, replayed: boolean}} HttpResult
 */

const RETRYABLE_STATUS = new Set([408, 429, 502, 503, 504]);
/** 応答の ETag（資源のオブジェクト → ETag）。 */
const ETAGS = new WeakMap();

/**
 * 資源の ETag（GET・PATCH の応答の `ETag`、無ければ `"{generation}"`）。
 * @param {object} resource
 * @returns {string | undefined}
 */
export function etagOf(resource) {
  if (!resource || typeof resource !== 'object') return undefined;
  const e = ETAGS.get(resource);
  if (e) return e;
  const r = /** @type {{etag?: unknown, generation?: unknown}} */ (resource);
  if (typeof r.etag === 'string') return r.etag;
  if (typeof r.generation === 'number') return `"${r.generation}"`;
  return undefined;
}

/** @param {IfMatch} v */
export function ifMatchValue(v) {
  if (typeof v === 'string') return /^(W\/)?"/.test(v) ? v : `"${v}"`;
  if (typeof v === 'number') return `"${v}"`;
  const e = etagOf(v);
  if (!e) throw new TypeError('ifMatch: ETag・generation が分からない');
  return e;
}

/** Idempotency-Key（UUID v4）。 */
export function newIdempotencyKey() {
  const c = /** @type {any} */ (globalThis).crypto;
  if (c?.randomUUID) return c.randomUUID();
  const b = new Uint8Array(16);
  c.getRandomValues(b);
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = [...b].map((x) => x.toString(16).padStart(2, '0')).join('');
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

/** @param {string | null} v @returns {number | null} */
export function parseRetryAfter(v) {
  if (!v) return null;
  if (/^\d+$/.test(v.trim())) return Number(v.trim()) * 1000;
  const t = Date.parse(v);
  return Number.isFinite(t) ? Math.max(0, t - Date.now()) : null;
}

/** HTTP の芯（SencaClient が持つ）。 */
export class Http {
  /** @param {ClientOptions} o */
  constructor(o) {
    if (!o || !o.baseUrl) throw new TypeError('baseUrl が要る');
    let base = o.baseUrl.replace(/\/+$/, '');
    if (!/\/api\/v\d+$/.test(base)) base += '/api/v1';
    this.base = base;
    this.token = o.token;
    this.fetch = o.fetch ?? globalThis.fetch.bind(globalThis);
    this.retry = o.retry ?? true;
    this.timeoutMs = o.timeoutMs ?? 30000;
    this.headers = o.headers ?? {};
    this.idempotency = o.idempotency ?? true;
  }

  /** @returns {Promise<Record<string, string>>} */
  async authHeaders() {
    const t = typeof this.token === 'function' ? await this.token() : this.token;
    return t ? { Authorization: `Bearer ${t}` } : {};
  }

  /**
   * @param {string} path `/streams/{id}` の形（値は埋めたもの）
   * @param {RequestOptions['query']} [query]
   */
  url(path, query) {
    const u = new URL(this.base + path);
    for (const [k, v] of Object.entries(query ?? {})) if (v !== undefined && v !== null) u.searchParams.set(k, String(v));
    return u.href;
  }

  /**
   * 要求を送る（再試行つき）。
   * @template T
   * @param {string} method
   * @param {string} path
   * @param {RequestOptions} [o]
   * @returns {Promise<HttpResult<T>>}
   */
  async request(method, path, o = {}) {
    method = method.toUpperCase();
    /** @type {Record<string, string>} */
    const headers = { Accept: 'application/json, application/problem+json', ...this.headers, ...(o.headers ?? {}) };
    if (o.body !== undefined) headers['Content-Type'] = 'application/json';
    if (o.ifMatch !== undefined) headers['If-Match'] = ifMatchValue(o.ifMatch);
    if (method === 'POST' && o.idempotencyKey !== false && (o.idempotencyKey || this.idempotency)) {
      headers['Idempotency-Key'] = o.idempotencyKey || newIdempotencyKey();
    }
    const policy = o.retry ?? this.retry;
    const max = policy === false ? 1 : (typeof policy === 'object' ? policy.maxAttempts : undefined) ?? 3;
    const backoff = typeof policy === 'object' ? policy : {};
    const body = o.body !== undefined ? JSON.stringify(o.body) : undefined;
    const url = this.url(path, o.query);
    // 本文を作ったかもしれない失敗（応答が無い）でも送り直してよいか
    const safe = method === 'GET' || method === 'DELETE' || method === 'PUT' || !!headers['Idempotency-Key'];
    for (let attempt = 0; ; attempt++) {
      /** @type {ApiError} */
      let err;
      try {
        return await this.once(method, url, path, { ...o, headers: { ...headers, ...(await this.authHeaders()) }, bodyText: body });
      } catch (e) {
        if (isAbortError(e) || !(e instanceof ApiError)) throw e;
        err = e;
      }
      const retryable = err.status === 0 ? safe : err.retryable && (safe || err.status === 429 || err.status === 503);
      if (!retryable || attempt + 1 >= max) throw err;
      const wait = Math.max(err.retryAfterMs ?? 0, backoffDelay(attempt, { initialDelayMs: 200, maxDelayMs: 5000, ...backoff }));
      await sleep(wait, o.signal);
    }
  }

  /**
   * @private
   * @param {string} method
   * @param {string} url
   * @param {string} path
   * @param {RequestOptions & {headers: Record<string, string>, bodyText?: string}} o
   * @returns {Promise<HttpResult<any>>}
   */
  async once(method, url, path, o) {
    if (o.signal?.aborted) throw abortReason(o.signal);
    const timeoutMs = o.timeoutMs ?? this.timeoutMs;
    const ctl = linkedController(o.signal);
    const timer = timeoutMs > 0 ? setTimeout(() => ctl.controller.abort(new DOMException('timeout', 'TimeoutError')), timeoutMs) : null;
    /** @type {Response} */
    let res;
    try {
      res = await this.fetch(url, { method, headers: o.headers, body: o.bodyText, signal: ctl.signal, cache: 'no-store', credentials: o.credentials ?? 'omit' });
    } catch (e) {
      if (o.signal?.aborted) throw abortReason(o.signal);
      const timedOut = ctl.signal.aborted;
      throw new ApiError({ status: 0, code: timedOut ? 'TIMEOUT' : 'NETWORK_ERROR', title: timedOut ? `${timeoutMs} ms で応答が無い` : `つながらない: ${e instanceof Error ? e.message : String(e)}`, retryable: true, method, path, cause: e });
    } finally {
      if (timer) clearTimeout(timer);
      ctl.dispose();
    }
    const requestId = res.headers.get('X-Request-Id');
    if (!res.ok) throw await problemFrom(res, method, path);
    const etag = res.headers.get('ETag');
    const replayed = res.headers.get('Idempotency-Replayed') === 'true';
    /** @type {unknown} */
    let data;
    if (o.as === 'response') data = res;
    else if (o.as === 'blob') data = await res.blob();
    else if (res.status === 204) data = undefined;
    else {
      const text = await res.text();
      data = text ? JSON.parse(text) : undefined;
    }
    if (data && typeof data === 'object' && etag && o.as !== 'response' && o.as !== 'blob') ETAGS.set(data, etag);
    return { status: res.status, data, etag, headers: res.headers, requestId, replayed };
  }
}

/**
 * 失敗の応答 → ApiError。
 * @param {Response} res
 * @param {string} method
 * @param {string} path
 */
export async function problemFrom(res, method, path) {
  /** @type {Record<string, any> | null} */
  let p = null;
  const ct = res.headers.get('Content-Type') ?? '';
  let text = '';
  try { text = await res.text(); } catch { /* 本文なし */ }
  if (/json/.test(ct) && text) {
    try { p = JSON.parse(text); } catch { p = null; }
  }
  const status = res.status;
  const code = typeof p?.code === 'string' ? p.code : codeFromStatus(status);
  const retryable = typeof p?.retryable === 'boolean' ? p.retryable || status === 429 || status === 503 : RETRYABLE_STATUS.has(status);
  return new ApiError({
    status,
    code,
    title: typeof p?.title === 'string' ? p.title : (text.slice(0, 200) || `HTTP ${status}`),
    type: p?.type,
    detail: p?.detail,
    requestId: p?.request_id ?? res.headers.get('X-Request-Id'),
    retryable,
    retryAfterMs: parseRetryAfter(res.headers.get('Retry-After')),
    details: p?.details ?? p?.errors ?? null,
    method,
    path,
    problem: p,
  });
}

/** @param {number} s */
function codeFromStatus(s) {
  return ({ 400: 'INVALID_REQUEST', 401: 'AUTH_FAILED', 403: 'FORBIDDEN', 404: 'NOT_FOUND', 405: 'METHOD_NOT_ALLOWED', 409: 'CONFLICT', 412: 'PRECONDITION_FAILED', 413: 'PAYLOAD_TOO_LARGE', 415: 'UNSUPPORTED_MEDIA_TYPE', 428: 'PRECONDITION_REQUIRED', 429: 'RATE_LIMITED', 503: 'CAPACITY_UNAVAILABLE' })[s] ?? (s >= 500 ? 'INTERNAL_ERROR' : 'HTTP_ERROR');
}

/**
 * カーソルでたどる一覧。`for await (const x of list)` で全部、`pages()` で頁ごと。
 * @template T
 */
export class Paginator {
  /**
   * @param {(cursor: string | null, signal?: AbortSignal) => Promise<{items: T[], next_cursor?: string | null}>} fetchPage
   * @param {{signal?: AbortSignal, cursor?: string}} [o]
   */
  constructor(fetchPage, o = {}) {
    /** @private */
    this._fetch = fetchPage;
    /** @private */
    this._signal = o.signal;
    /** @private */
    this._cursor = o.cursor ?? null;
  }

  /** 頁ごと（`next_cursor` が null になるまで）。 @returns {AsyncGenerator<{items: T[], nextCursor: string | null}>} */
  async *pages() {
    let cursor = this._cursor;
    for (;;) {
      if (this._signal?.aborted) return;
      const p = await this._fetch(cursor, this._signal);
      const next = p.next_cursor ?? null;
      yield { items: p.items, nextCursor: next };
      if (!next || next === cursor) return;
      cursor = next;
    }
  }

  /** @returns {AsyncGenerator<T>} */
  async *[Symbol.asyncIterator]() {
    for await (const p of this.pages()) yield* p.items;
  }

  /**
   * 全部（`max` 個まで）を配列で。
   * @param {{max?: number}} [o]
   * @returns {Promise<T[]>}
   */
  async toArray(o = {}) {
    const max = o.max ?? Infinity;
    /** @type {T[]} */
    const out = [];
    for await (const x of this) {
      out.push(x);
      if (out.length >= max) break;
    }
    return out;
  }

  /** 最初の 1 つ（無ければ undefined）。 @returns {Promise<T | undefined>} */
  async first() {
    for await (const x of this) return x;
    return undefined;
  }
}
