/**
 * @param {import('./http.js').Http} http
 * @param {EventsOptions} [o]
 * @returns {AsyncGenerator<ServerEvent>}
 */
export function streamEvents(http: import("./http.js").Http, o?: EventsOptions): AsyncGenerator<ServerEvent>;
/** @typedef {import('./openapi.gen.js').SseEventData} SseEventData */
/**
 * SSE のイベント。`type` が `resync` なら取りこぼしの合図（`data` は空）。
 * @typedef {{id: string | null, type: SseEventData['type'] | (string & {}), data: SseEventData} | {id: string | null, type: 'resync', data: Record<string, never>}} ServerEvent
 */
/**
 * @typedef {object} EventsOptions
 * @property {string[]} [types] この種類だけ（`stream.created` など。`stream.*` の形で前方一致）
 * @property {string} [lastEventId] ここから（`Last-Event-ID`）
 * @property {boolean | import('../internal/async.js').BackoffPolicy} [reconnect] 切れたら付け直す（既定 true）
 * @property {AbortSignal} [signal] 取り消すと反復を終える
 */
/** text/event-stream の解析器（行を足すと、まとまったイベントを返す）。 */
export class EventStreamParser {
    buf: string;
    event: string;
    /** @type {string[]} */
    data: string[];
    /** @type {string | undefined} */
    id: string | undefined;
    /** @type {string | null} */
    lastId: string | null;
    /** @type {number | null} */
    retry: number | null;
    /**
     * @param {string} text
     * @returns {Array<{type: string, id: string | null, data: string}>}
     */
    feed(text: string): Array<{
        type: string;
        id: string | null;
        data: string;
    }>;
}
export type SseEventData = import("./openapi.gen.js").SseEventData;
/**
 * SSE のイベント。`type` が `resync` なら取りこぼしの合図（`data` は空）。
 */
export type ServerEvent = {
    id: string | null;
    type: SseEventData["type"] | (string & {});
    data: SseEventData;
} | {
    id: string | null;
    type: "resync";
    data: Record<string, never>;
};
export type EventsOptions = {
    /**
     * この種類だけ（`stream.created` など。`stream.*` の形で前方一致）
     */
    types?: string[] | undefined;
    /**
     * ここから（`Last-Event-ID`）
     */
    lastEventId?: string | undefined;
    /**
     * 切れたら付け直す（既定 true）
     */
    reconnect?: boolean | import("../internal/async.js").BackoffPolicy | undefined;
    /**
     * 取り消すと反復を終える
     */
    signal?: AbortSignal | undefined;
};
