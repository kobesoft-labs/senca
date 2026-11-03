/**
 * 取り消しの誤り（`signal.reason` があればそれ、無ければ AbortError の DOMException）。
 * @param {AbortSignal | undefined} signal
 * @returns {unknown}
 */
export function abortReason(signal: AbortSignal | undefined): unknown;
/** @param {unknown} e */
export function isAbortError(e: unknown): boolean;
/**
 * シグナリング v1 の `error.code`（docs/realtime/signaling.md §7）。
 * @typedef {'bad_message' | 'unsupported_version' | 'auth_failed' | 'forbidden' | 'room_not_found' | 'room_full'
 *   | 'role_violation' | 'e2ee_mismatch' | 'unsupported_codec' | 'invalid_sdp' | 'negotiation_failed'
 *   | 'invalid_state' | 'rate_limited' | 'unknown_track' | 'unknown_label' | 'timeout' | 'internal'} SignalingErrorCode
 */
/**
 * SDK が自分で出す誤り。
 * - `connection_failed`: WebSocket・ピア接続がつながらない、途中で切れて再接続をあきらめた
 * - `closed`: 閉じたセッションを使った
 * - `not_connected`: まだつながっていない（再接続中など）で送れない
 * - `kicked`・`room_deleted`: サーバーが切断した（再接続しない）
 * - `message_too_large`: アプリの message が 64 KiB を超えた
 * - `invalid_argument`: 引数の誤り
 * - `unsupported`: この環境・この部屋ではできない
 * - `too_many_redirects`: `redirect` が続いた
 * @typedef {'connection_failed' | 'closed' | 'not_connected' | 'kicked' | 'room_deleted' | 'message_too_large'
 *   | 'invalid_argument' | 'unsupported' | 'too_many_redirects'} ClientErrorCode
 */
/** @typedef {SignalingErrorCode | ClientErrorCode | string} SencaErrorCode */
/** SDK のすべての誤りの基底。 */
export class SencaError extends Error {
    /**
     * @param {SencaErrorCode} code
     * @param {string} message
     * @param {{fatal?: boolean, closeCode?: number, request?: string, cause?: unknown}} [opts]
     */
    constructor(code: SencaErrorCode, message: string, opts?: {
        fatal?: boolean;
        closeCode?: number;
        request?: string;
        cause?: unknown;
    });
    /** 誤りの種類（シグナリングの `code`、REST の problem の `code`、SDK の code）。 */
    code: string;
    /** 真ならセッション（接続）は続かない。偽ならその要求だけが失敗した。 */
    fatal: boolean;
    /** WebSocket の close code（あれば）。 */
    closeCode: number | undefined;
    /** 誤りの元になった message の `type`（シグナリング）。 */
    request: string | undefined;
}
/**
 * REST（problem+json、RFC 9457）の誤り。`code` は problem の `code`（`NOT_FOUND`・`PRECONDITION_FAILED` など）。
 * 輸送の失敗（応答が無い）は `status` 0・`code` `NETWORK_ERROR`。
 */
export class ApiError extends SencaError {
    /**
     * @param {{status: number, code: string, title: string, type?: string, detail?: string, requestId?: string | null,
     *   retryable?: boolean, retryAfterMs?: number | null, details?: unknown, method?: string, path?: string,
     *   problem?: Record<string, unknown> | null, cause?: unknown}} p
     */
    constructor(p: {
        status: number;
        code: string;
        title: string;
        type?: string;
        detail?: string;
        requestId?: string | null;
        retryable?: boolean;
        retryAfterMs?: number | null;
        details?: unknown;
        method?: string;
        path?: string;
        problem?: Record<string, unknown> | null;
        cause?: unknown;
    });
    /** HTTP の状態コード（輸送の失敗は 0）。 */
    status: number;
    title: string;
    /** problem の `type`（`urn:senca:error:...`）。 */
    type: string | null;
    detail: string | null;
    /** `X-Request-Id`（問い合わせのときに伝える）。 */
    requestId: string | null;
    /** 同じ要求を送り直してよいか（problem の `retryable`、408・429・502・503・504、輸送の失敗）。 */
    retryable: boolean;
    /** `Retry-After`（ミリ秒）。 */
    retryAfterMs: number | null;
    /** problem の `details`（code ごとの補足。パイプラインの検証の問題など）。 */
    details: {} | null;
    method: string | undefined;
    path: string | undefined;
    /** problem+json の本文そのもの。 */
    problem: Record<string, unknown> | null;
}
/**
 * シグナリング v1 の `error.code`（docs/realtime/signaling.md §7）。
 */
export type SignalingErrorCode = "bad_message" | "unsupported_version" | "auth_failed" | "forbidden" | "room_not_found" | "room_full" | "role_violation" | "e2ee_mismatch" | "unsupported_codec" | "invalid_sdp" | "negotiation_failed" | "invalid_state" | "rate_limited" | "unknown_track" | "unknown_label" | "timeout" | "internal";
/**
 * SDK が自分で出す誤り。
 * - `connection_failed`: WebSocket・ピア接続がつながらない、途中で切れて再接続をあきらめた
 * - `closed`: 閉じたセッションを使った
 * - `not_connected`: まだつながっていない（再接続中など）で送れない
 * - `kicked`・`room_deleted`: サーバーが切断した（再接続しない）
 * - `message_too_large`: アプリの message が 64 KiB を超えた
 * - `invalid_argument`: 引数の誤り
 * - `unsupported`: この環境・この部屋ではできない
 * - `too_many_redirects`: `redirect` が続いた
 */
export type ClientErrorCode = "connection_failed" | "closed" | "not_connected" | "kicked" | "room_deleted" | "message_too_large" | "invalid_argument" | "unsupported" | "too_many_redirects";
export type SencaErrorCode = SignalingErrorCode | ClientErrorCode | string;
