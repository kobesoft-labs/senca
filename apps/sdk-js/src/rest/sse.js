// SSE（GET /api/v1/events）を非同期イテレーターで読む。EventSource は Authorization を送れないので fetch で読む。
//
// - 切れたら Last-Event-ID を付けて付け直す（指数バックオフ）。ノードは再送を保持しないので、付け直すと
//   `resync`（取りこぼしたかもしれない。一覧を読み直す合図）が来ることがある。`resync` は types で絞っても必ず流す。
// - 401・403 は付け直さずに ApiError で反復を終える。

import { isAbortError } from '../errors.js';
import { backoffDelay, sleep } from '../internal/async.js';
import { problemFrom } from './http.js';

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
  constructor() {
    this.buf = '';
    this.event = '';
    /** @type {string[]} */
    this.data = [];
    /** @type {string | undefined} */
    this.id = undefined;
    /** @type {string | null} */
    this.lastId = null;
    /** @type {number | null} */
    this.retry = null;
  }

  /**
   * @param {string} text
   * @returns {Array<{type: string, id: string | null, data: string}>}
   */
  feed(text) {
    this.buf += text;
    /** @type {Array<{type: string, id: string | null, data: string}>} */
    const out = [];
    let idx;
    while ((idx = this.buf.search(/\r\n|\n|\r(?!$)/)) !== -1) {
      const line = this.buf.slice(0, idx);
      this.buf = this.buf.slice(idx + (this.buf[idx] === '\r' && this.buf[idx + 1] === '\n' ? 2 : 1));
      if (line === '') {
        if (this.id !== undefined) this.lastId = this.id;
        if (this.data.length || this.event) out.push({ type: this.event || 'message', id: this.lastId, data: this.data.join('\n') });
        this.event = '';
        this.data = [];
        this.id = undefined;
        continue;
      }
      if (line.startsWith(':')) continue;
      const c = line.indexOf(':');
      const field = c === -1 ? line : line.slice(0, c);
      let value = c === -1 ? '' : line.slice(c + 1);
      if (value.startsWith(' ')) value = value.slice(1);
      if (field === 'event') this.event = value;
      else if (field === 'data') this.data.push(value);
      else if (field === 'id' && !value.includes('\0')) this.id = value;
      else if (field === 'retry' && /^\d+$/.test(value)) this.retry = Number(value);
    }
    return out;
  }
}

/**
 * @param {import('./http.js').Http} http
 * @param {EventsOptions} [o]
 * @returns {AsyncGenerator<ServerEvent>}
 */
export async function* streamEvents(http, o = {}) {
  const pats = o.types?.map((t) => (t.endsWith('*') ? { prefix: t.slice(0, -1) } : { exact: t }));
  /** @param {string} t */
  const wanted = (t) => t === 'resync' || !pats || pats.some((p) => (p.exact ? p.exact === t : t.startsWith(/** @type {string} */ (p.prefix))));
  let lastId = o.lastEventId ?? null;
  const policy = o.reconnect ?? true;
  let attempt = 0;
  for (;;) {
    if (o.signal?.aborted) return;
    /** @type {Response | null} */
    let res = null;
    let received = false;
    try {
      /** @type {Record<string, string>} */
      const headers = { Accept: 'text/event-stream', ...http.headers, ...(await http.authHeaders()) };
      if (lastId !== null) headers['Last-Event-ID'] = lastId;
      res = await http.fetch(http.url('/events'), { method: 'GET', headers, signal: o.signal, cache: 'no-store', credentials: 'omit' });
      if (!res.ok) {
        const err = await problemFrom(res, 'GET', '/events');
        if (res.status === 401 || res.status === 403 || res.status === 404 || policy === false) throw err;
      } else if (res.body) {
        const parser = new EventStreamParser();
        const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
        try {
          for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            received = true;
            for (const ev of parser.feed(value)) {
              if (ev.id !== null) lastId = ev.id;
              attempt = 0;
              if (!wanted(ev.type)) continue;
              /** @type {any} */
              let data = {};
              try { data = ev.data ? JSON.parse(ev.data) : {}; } catch { data = ev.data; }
              yield /** @type {ServerEvent} */ ({ id: ev.id, type: ev.type, data });
            }
          }
        } finally {
          reader.cancel().catch(() => {});
        }
      }
    } catch (e) {
      if (isAbortError(e) || o.signal?.aborted) return;
      if (e && typeof e === 'object' && 'status' in e && [401, 403, 404].includes(/** @type {any} */ (e).status)) throw e;
      if (policy === false) throw e;
    }
    if (policy === false) return;
    if (received) attempt = 0;
    try {
      await sleep(backoffDelay(attempt++, { initialDelayMs: 1000, maxDelayMs: 30000, ...(typeof policy === 'object' ? policy : {}) }), o.signal);
    } catch {
      return;
    }
  }
}
