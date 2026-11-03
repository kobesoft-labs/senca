// 出来事を複数の購読者へ配る。購読者ごとに非同期イテレーター（`for await`）を返す。
//
// - 購読者ごとにキューを持つ。読まない購読者がいても他は止まらない（溜まりすぎたら古いものから捨てる）。
// - `close()` で全員の反復を終える（`error` を渡せば反復はその誤りで失敗する）。
// - `signal` を取り消すと、その購読の反復は静かに終わる（`for await` を抜ける）。
// - `break` などで反復をやめると購読を外す。

/**
 * @template T
 * @typedef {object} SubscribeOptions
 * @property {AbortSignal} [signal] 取り消すと反復を終える
 * @property {(v: T) => boolean} [filter] 真のものだけ受ける
 * @property {() => Iterable<T>} [initial] 購読の始めに先に流すもの（今の状態など）
 * @property {number} [maxBuffer] 読まれずに溜められる数（既定 1024）
 */

/** @template T */
export class Broadcaster {
  constructor() {
    /** @type {Set<Subscriber<T>>} */
    this.subs = new Set();
    this.closed = false;
    /** @type {unknown} */
    this.error = undefined;
  }

  /** @param {T} v */
  emit(v) {
    if (this.closed) return;
    for (const s of this.subs) s.push(v);
  }

  /** @param {unknown} [error] */
  close(error) {
    if (this.closed) return;
    this.closed = true;
    this.error = error;
    for (const s of [...this.subs]) s.end(error);
    this.subs.clear();
  }

  /**
   * @param {SubscribeOptions<T>} [opts]
   * @returns {AsyncIterableIterator<T>}
   */
  subscribe(opts = {}) {
    const s = new Subscriber(this, opts);
    if (opts.initial) for (const v of opts.initial()) s.push(v);
    if (this.closed) s.end(this.error);
    else if (opts.signal?.aborted) s.end();
    else this.subs.add(s);
    return s;
  }
}

/**
 * @template T
 * @implements {AsyncIterableIterator<T>}
 */
class Subscriber {
  /**
   * @param {Broadcaster<T>} hub
   * @param {SubscribeOptions<T>} opts
   */
  constructor(hub, opts) {
    this.hub = hub;
    this.filter = opts.filter;
    this.max = opts.maxBuffer ?? 1024;
    /** @type {T[]} */
    this.buf = [];
    /** @type {Array<{resolve: (r: IteratorResult<T>) => void, reject: (e: unknown) => void}>} */
    this.waiting = [];
    this.done = false;
    /** @type {unknown} */
    this.error = undefined;
    this.signal = opts.signal;
    this.onAbort = () => this.end();
    this.signal?.addEventListener('abort', this.onAbort, { once: true });
  }

  /** @param {T} v */
  push(v) {
    if (this.done) return;
    if (this.filter && !this.filter(v)) return;
    const w = this.waiting.shift();
    if (w) { w.resolve({ value: v, done: false }); return; }
    this.buf.push(v);
    if (this.buf.length > this.max) this.buf.shift();
  }

  /** @param {unknown} [error] */
  end(error) {
    if (this.done) return;
    this.done = true;
    this.error = error;
    this.hub.subs.delete(this);
    this.signal?.removeEventListener('abort', this.onAbort);
    for (const w of this.waiting.splice(0)) {
      if (error !== undefined && this.buf.length === 0) w.reject(error);
      else w.resolve({ value: undefined, done: true });
    }
  }

  /** @returns {Promise<IteratorResult<T>>} */
  next() {
    if (this.buf.length) return Promise.resolve({ value: /** @type {T} */ (this.buf.shift()), done: false });
    if (this.done) {
      if (this.error !== undefined) {
        const e = this.error;
        this.error = undefined; // 1 度だけ投げる
        return Promise.reject(e);
      }
      return Promise.resolve({ value: undefined, done: true });
    }
    return new Promise((resolve, reject) => this.waiting.push({ resolve, reject }));
  }

  /** @returns {Promise<IteratorResult<T>>} */
  return() {
    this.buf.length = 0;
    this.end();
    this.error = undefined;
    return Promise.resolve({ value: undefined, done: true });
  }

  [Symbol.asyncIterator]() {
    return this;
  }
}
