// 非同期の小道具: 取り消しできる待ち、時間切れ、指数バックオフ、外から決める Promise。

import { abortReason, SencaError } from '../errors.js';

/**
 * @template T
 * @typedef {{promise: Promise<T>, resolve: (v: T) => void, reject: (e: unknown) => void, settled: boolean}} Deferred
 */

/**
 * @template T
 * @returns {Deferred<T>}
 */
export function deferred() {
  /** @type {(v: T) => void} */ let resolve = () => {};
  /** @type {(e: unknown) => void} */ let reject = () => {};
  const d = /** @type {Deferred<T>} */ ({ settled: false });
  d.promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  d.resolve = (v) => { if (!d.settled) { d.settled = true; resolve(v); } };
  d.reject = (e) => { if (!d.settled) { d.settled = true; reject(e); } };
  // 誰も待っていない失敗を「未処理」にしない
  d.promise.catch(() => {});
  return d;
}

/**
 * `ms` 待つ。`signal` で取り消すと AbortError で失敗する。
 * @param {number} ms
 * @param {AbortSignal} [signal]
 * @returns {Promise<void>}
 */
export function sleep(ms, signal) {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(abortReason(signal));
    const t = setTimeout(() => { signal?.removeEventListener('abort', onAbort); resolve(); }, ms);
    const onAbort = () => { clearTimeout(t); reject(abortReason(signal)); };
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

/**
 * `p` を `signal` で取り消せるようにする（`p` 自体は止まらない）。
 * @template T
 * @param {Promise<T>} p
 * @param {AbortSignal} [signal]
 * @returns {Promise<T>}
 */
export function abortable(p, signal) {
  if (!signal) return p;
  if (signal.aborted) return Promise.reject(abortReason(signal));
  return new Promise((resolve, reject) => {
    const onAbort = () => reject(abortReason(signal));
    signal.addEventListener('abort', onAbort, { once: true });
    p.then(
      (v) => { signal.removeEventListener('abort', onAbort); resolve(v); },
      (e) => { signal.removeEventListener('abort', onAbort); reject(e); },
    );
  });
}

/**
 * `ms` で時間切れ（SencaError `timeout`）にする。
 * @template T
 * @param {Promise<T>} p
 * @param {number} ms
 * @param {string} what
 * @returns {Promise<T>}
 */
export function withTimeout(p, ms, what) {
  if (!Number.isFinite(ms) || ms <= 0) return p;
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new SencaError('timeout', `${what}: ${ms} ms で時間切れ`)), ms);
    p.then((v) => { clearTimeout(t); resolve(v); }, (e) => { clearTimeout(t); reject(e); });
  });
}

/**
 * 指数バックオフの方針。
 * @typedef {object} BackoffPolicy
 * @property {number} [initialDelayMs] 最初の待ち（既定 500）
 * @property {number} [maxDelayMs] 待ちの上限（既定 10000）
 * @property {number} [factor] 倍率（既定 2）
 * @property {number} [jitter] ゆらぎの割合 0〜1（既定 0.2 = ±20 %）
 */

/**
 * `attempt` 回目（0 から）の待ち時間。
 * @param {number} attempt
 * @param {BackoffPolicy} [p]
 * @param {() => number} [random]
 */
export function backoffDelay(attempt, p = {}, random = Math.random) {
  const initial = p.initialDelayMs ?? 500;
  const max = p.maxDelayMs ?? 10000;
  const factor = p.factor ?? 2;
  const jitter = p.jitter ?? 0.2;
  const base = Math.min(max, initial * Math.pow(factor, attempt));
  const j = base * jitter * (random() * 2 - 1);
  return Math.max(0, Math.round(Math.min(max, base + j)));
}

/**
 * 親の signal の取り消しを受け継ぐ AbortController（`dispose()` で親から外す）。
 * @param {...(AbortSignal | undefined)} parents
 */
export function linkedController(...parents) {
  const c = new AbortController();
  /** @type {Array<() => void>} */
  const offs = [];
  for (const p of parents) {
    if (!p) continue;
    if (p.aborted) { c.abort(p.reason); break; }
    const f = () => c.abort(p.reason);
    p.addEventListener('abort', f, { once: true });
    offs.push(() => p.removeEventListener('abort', f));
  }
  return { controller: c, signal: c.signal, dispose: () => { for (const o of offs) o(); } };
}
