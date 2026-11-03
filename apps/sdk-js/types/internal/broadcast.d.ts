/**
 * @template T
 * @typedef {object} SubscribeOptions
 * @property {AbortSignal} [signal] 取り消すと反復を終える
 * @property {(v: T) => boolean} [filter] 真のものだけ受ける
 * @property {() => Iterable<T>} [initial] 購読の始めに先に流すもの（今の状態など）
 * @property {number} [maxBuffer] 読まれずに溜められる数（既定 1024）
 */
/** @template T */
export class Broadcaster<T> {
    /** @type {Set<Subscriber<T>>} */
    subs: Set<Subscriber<T>>;
    closed: boolean;
    /** @type {unknown} */
    error: unknown;
    /** @param {T} v */
    emit(v: T): void;
    /** @param {unknown} [error] */
    close(error?: unknown): void;
    /**
     * @param {SubscribeOptions<T>} [opts]
     * @returns {AsyncIterableIterator<T>}
     */
    subscribe(opts?: SubscribeOptions<T>): AsyncIterableIterator<T>;
}
export type SubscribeOptions<T> = {
    /**
     * 取り消すと反復を終える
     */
    signal?: AbortSignal | undefined;
    /**
     * 真のものだけ受ける
     */
    filter?: ((v: T) => boolean) | undefined;
    /**
     * 購読の始めに先に流すもの（今の状態など）
     */
    initial?: (() => Iterable<T>) | undefined;
    /**
     * 読まれずに溜められる数（既定 1024）
     */
    maxBuffer?: number | undefined;
};
/**
 * @template T
 * @implements {AsyncIterableIterator<T>}
 */
declare class Subscriber<T> implements AsyncIterableIterator<T> {
    /**
     * @param {Broadcaster<T>} hub
     * @param {SubscribeOptions<T>} opts
     */
    constructor(hub: Broadcaster<T>, opts: SubscribeOptions<T>);
    hub: Broadcaster<T>;
    filter: ((v: T) => boolean) | undefined;
    max: number;
    /** @type {T[]} */
    buf: T[];
    /** @type {Array<{resolve: (r: IteratorResult<T>) => void, reject: (e: unknown) => void}>} */
    waiting: Array<{
        resolve: (r: IteratorResult<T>) => void;
        reject: (e: unknown) => void;
    }>;
    done: boolean;
    /** @type {unknown} */
    error: unknown;
    signal: AbortSignal | undefined;
    onAbort: () => void;
    /** @param {T} v */
    push(v: T): void;
    /** @param {unknown} [error] */
    end(error?: unknown): void;
    /** @returns {Promise<IteratorResult<T>>} */
    next(): Promise<IteratorResult<T>>;
    /** @returns {Promise<IteratorResult<T>>} */
    return(): Promise<IteratorResult<T>>;
    [Symbol.asyncIterator](): this;
}
export {};
