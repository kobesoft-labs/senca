/**
 * @template T
 * @typedef {{promise: Promise<T>, resolve: (v: T) => void, reject: (e: unknown) => void, settled: boolean}} Deferred
 */
/**
 * @template T
 * @returns {Deferred<T>}
 */
export function deferred<T>(): Deferred<T>;
/**
 * `ms` 待つ。`signal` で取り消すと AbortError で失敗する。
 * @param {number} ms
 * @param {AbortSignal} [signal]
 * @returns {Promise<void>}
 */
export function sleep(ms: number, signal?: AbortSignal): Promise<void>;
/**
 * `p` を `signal` で取り消せるようにする（`p` 自体は止まらない）。
 * @template T
 * @param {Promise<T>} p
 * @param {AbortSignal} [signal]
 * @returns {Promise<T>}
 */
export function abortable<T>(p: Promise<T>, signal?: AbortSignal): Promise<T>;
/**
 * `ms` で時間切れ（SencaError `timeout`）にする。
 * @template T
 * @param {Promise<T>} p
 * @param {number} ms
 * @param {string} what
 * @returns {Promise<T>}
 */
export function withTimeout<T>(p: Promise<T>, ms: number, what: string): Promise<T>;
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
export function backoffDelay(attempt: number, p?: BackoffPolicy, random?: () => number): number;
/**
 * 親の signal の取り消しを受け継ぐ AbortController（`dispose()` で親から外す）。
 * @param {...(AbortSignal | undefined)} parents
 */
export function linkedController(...parents: (AbortSignal | undefined)[]): {
    controller: AbortController;
    signal: AbortSignal;
    dispose: () => void;
};
export type Deferred<T> = {
    promise: Promise<T>;
    resolve: (v: T) => void;
    reject: (e: unknown) => void;
    settled: boolean;
};
/**
 * 指数バックオフの方針。
 */
export type BackoffPolicy = {
    /**
     * 最初の待ち（既定 500）
     */
    initialDelayMs?: number | undefined;
    /**
     * 待ちの上限（既定 10000）
     */
    maxDelayMs?: number | undefined;
    /**
     * 倍率（既定 2）
     */
    factor?: number | undefined;
    /**
     * ゆらぎの割合 0〜1（既定 0.2 = ±20 %）
     */
    jitter?: number | undefined;
};
