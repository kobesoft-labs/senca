// SDK の誤り。すべて SencaError（`code`・`fatal` を持つ）から派生する。
// 取り消し（AbortSignal）は標準どおり DOMException の AbortError（`signal.reason`）で知らせ、SencaError にしない。

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
  constructor(code, message, opts = {}) {
    super(message, opts.cause !== undefined ? { cause: opts.cause } : undefined);
    this.name = 'SencaError';
    /** 誤りの種類（シグナリングの `code`、REST の problem の `code`、SDK の code）。 */
    this.code = code;
    /** 真ならセッション（接続）は続かない。偽ならその要求だけが失敗した。 */
    this.fatal = !!opts.fatal;
    /** WebSocket の close code（あれば）。 */
    this.closeCode = opts.closeCode;
    /** 誤りの元になった message の `type`（シグナリング）。 */
    this.request = opts.request;
  }
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
  constructor(p) {
    super(p.code, p.title || `HTTP ${p.status}`, { fatal: !p.retryable, cause: p.cause });
    this.name = 'ApiError';
    /** HTTP の状態コード（輸送の失敗は 0）。 */
    this.status = p.status;
    this.title = p.title || `HTTP ${p.status}`;
    /** problem の `type`（`urn:senca:error:...`）。 */
    this.type = p.type ?? null;
    this.detail = p.detail ?? null;
    /** `X-Request-Id`（問い合わせのときに伝える）。 */
    this.requestId = p.requestId ?? null;
    /** 同じ要求を送り直してよいか（problem の `retryable`、408・429・502・503・504、輸送の失敗）。 */
    this.retryable = !!p.retryable;
    /** `Retry-After`（ミリ秒）。 */
    this.retryAfterMs = p.retryAfterMs ?? null;
    /** problem の `details`（code ごとの補足。パイプラインの検証の問題など）。 */
    this.details = p.details ?? null;
    this.method = p.method;
    this.path = p.path;
    /** problem+json の本文そのもの。 */
    this.problem = p.problem ?? null;
  }
}

/**
 * 取り消しの誤り（`signal.reason` があればそれ、無ければ AbortError の DOMException）。
 * @param {AbortSignal | undefined} signal
 * @returns {unknown}
 */
export function abortReason(signal) {
  if (signal && signal.reason !== undefined) return signal.reason;
  return new DOMException('The operation was aborted.', 'AbortError');
}

/** @param {unknown} e */
export function isAbortError(e) {
  return !!e && typeof e === 'object' && /** @type {{name?: string}} */ (e).name === 'AbortError';
}
