/**
 * 資源の ETag（GET・PATCH の応答の `ETag`、無ければ `"{generation}"`）。
 * @param {object} resource
 * @returns {string | undefined}
 */
export function etagOf(resource: object): string | undefined;
/** @param {IfMatch} v */
export function ifMatchValue(v: IfMatch): string;
/** Idempotency-Key（UUID v4）。 */
export function newIdempotencyKey(): any;
/** @param {string | null} v @returns {number | null} */
export function parseRetryAfter(v: string | null): number | null;
/**
 * 失敗の応答 → ApiError。
 * @param {Response} res
 * @param {string} method
 * @param {string} path
 */
export function problemFrom(res: Response, method: string, path: string): Promise<ApiError>;
/** HTTP の芯（SencaClient が持つ）。 */
export class Http {
    /** @param {ClientOptions} o */
    constructor(o: ClientOptions);
    base: string;
    token: string | (() => string | Promise<string>) | undefined;
    fetch: typeof fetch;
    retry: boolean | RetryOptions;
    timeoutMs: number;
    headers: Record<string, string>;
    idempotency: boolean;
    /** @returns {Promise<Record<string, string>>} */
    authHeaders(): Promise<Record<string, string>>;
    /**
     * @param {string} path `/streams/{id}` の形（値は埋めたもの）
     * @param {RequestOptions['query']} [query]
     */
    url(path: string, query?: RequestOptions["query"]): string;
    /**
     * 要求を送る（再試行つき）。
     * @template T
     * @param {string} method
     * @param {string} path
     * @param {RequestOptions} [o]
     * @returns {Promise<HttpResult<T>>}
     */
    request<T>(method: string, path: string, o?: RequestOptions): Promise<HttpResult<T>>;
    /**
     * @private
     * @param {string} method
     * @param {string} url
     * @param {string} path
     * @param {RequestOptions & {headers: Record<string, string>, bodyText?: string}} o
     * @returns {Promise<HttpResult<any>>}
     */
    private once;
}
/**
 * カーソルでたどる一覧。`for await (const x of list)` で全部、`pages()` で頁ごと。
 * @template T
 */
export class Paginator<T> {
    /**
     * @param {(cursor: string | null, signal?: AbortSignal) => Promise<{items: T[], next_cursor?: string | null}>} fetchPage
     * @param {{signal?: AbortSignal, cursor?: string}} [o]
     */
    constructor(fetchPage: (cursor: string | null, signal?: AbortSignal) => Promise<{
        items: T[];
        next_cursor?: string | null;
    }>, o?: {
        signal?: AbortSignal;
        cursor?: string;
    });
    /** @private */
    private _fetch;
    /** @private */
    private _signal;
    /** @private */
    private _cursor;
    /** 頁ごと（`next_cursor` が null になるまで）。 @returns {AsyncGenerator<{items: T[], nextCursor: string | null}>} */
    pages(): AsyncGenerator<{
        items: T[];
        nextCursor: string | null;
    }>;
    /**
     * 全部（`max` 個まで）を配列で。
     * @param {{max?: number}} [o]
     * @returns {Promise<T[]>}
     */
    toArray(o?: {
        max?: number;
    }): Promise<T[]>;
    /** 最初の 1 つ（無ければ undefined）。 @returns {Promise<T | undefined>} */
    first(): Promise<T | undefined>;
    /** @returns {AsyncGenerator<T>} */
    [Symbol.asyncIterator](): AsyncGenerator<T>;
}
/**
 * 再試行の方針（`false` で再試行しない）。
 */
export type RetryOptions = {
    /**
     * 最初の 1 回を含む回数（既定 3）
     */
    maxAttempts?: number | undefined;
    /**
     * 既定 200
     */
    initialDelayMs?: number | undefined;
    /**
     * 既定 5000
     */
    maxDelayMs?: number | undefined;
    /**
     * 既定 2
     */
    factor?: number | undefined;
    /**
     * 既定 0.2
     */
    jitter?: number | undefined;
};
export type ClientOptions = {
    /**
     * `https://node.example`（`/api/v1` は足す）か `https://node.example/api/v1`
     */
    baseUrl: string;
    /**
     * API トークン（`senca_...`）・ログインのセッション（`sencas_...`）・JWT。関数なら要求のたびに呼ぶ。
     * ログインだけをするなら省いてよい（ログインの操作は認証なしで使えます）
     */
    token?: string | (() => string | Promise<string>) | undefined;
    /**
     * 既定は globalThis.fetch
     */
    fetch?: typeof fetch | undefined;
    /**
     * 既定 true（3 回まで）
     */
    retry?: boolean | RetryOptions | undefined;
    /**
     * 1 回の要求の時間切れ（既定 30000、SSE には使わない）
     */
    timeoutMs?: number | undefined;
    /**
     * すべての要求に付ける見出し
     */
    headers?: Record<string, string> | undefined;
    /**
     * POST に自動で Idempotency-Key を付ける（既定 true。再試行を安全にする）
     */
    idempotency?: boolean | undefined;
};
/**
 * If-Match の値。ETag の文字列（`"3"`）、generation の数、`generation` を持つ資源のどれか。
 */
export type IfMatch = string | number | {
    generation: number;
} | {
    etag: string;
};
/**
 * 1 回の要求の指定。
 */
export type RequestOptions = {
    query?: Record<string, string | number | boolean | null | undefined> | undefined;
    /**
     * JSON にして送る
     */
    body?: unknown;
    headers?: Record<string, string> | undefined;
    ifMatch?: IfMatch | undefined;
    /**
     * POST の Idempotency-Key（既定は自動で作る。`false` で付けない）
     */
    idempotencyKey?: string | false | undefined;
    signal?: AbortSignal | undefined;
    /**
     * 応答の読み方（既定 json）
     */
    as?: "blob" | "json" | "response" | undefined;
    retry?: boolean | RetryOptions | undefined;
    timeoutMs?: number | undefined;
    /**
     * fetch の credentials（既定 `omit`。クッキーを送る `POST /auth/exchange` だけ `same-origin`）
     */
    credentials?: RequestCredentials | undefined;
};
/**
 * 応答（本文と見出し）。
 */
export type HttpResult<T> = {
    status: number;
    data: T;
    etag: string | null;
    headers: Headers;
    requestId: string | null;
    replayed: boolean;
};
import { ApiError } from '../errors.js';
