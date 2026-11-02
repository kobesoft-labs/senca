// SSE（GET /api/v1/events）を fetch() で読む。EventSource は Authorization ヘッダーを送れないので使わない。
//
// - text/event-stream を自分で解析する（event・data・id・retry、コメント行は keepalive として無視）。
// - 切れたら Last-Event-ID を付けて付け直す（待ちは 1 秒から倍々で最大 30 秒）。
// - `resync` を受けたら bus に `resync` を流す（画面は一覧を読み直す）。ノードは再送の保持を持たないので、
//   付け直しのたびに resync になることがある。
// - 401 はログインへ戻す。403 は購読をやめる（権限が無い）。

import { BASE, getToken } from './client.js';
import { emit } from '../lib/bus.js';

/** text/event-stream の解析器。feed() に文字列を渡すと、まとまったイベントを onEvent で返す。 */
export class EventStreamParser {
  constructor(onEvent) {
    this.onEvent = onEvent;
    this.buf = '';
    this.reset();
    this.lastId = null;
    this.retry = null;
  }

  reset() {
    this.event = '';
    this.data = [];
    this.id = undefined;
  }

  feed(text) {
    this.buf += text;
    let idx;
    // 行の終わりは CRLF・LF・CR（末尾の CR は次の LF を待つ）
    while ((idx = this.buf.search(/\r\n|\n|\r(?!$)/)) !== -1) {
      const line = this.buf.slice(0, idx);
      const nl = this.buf[idx] === '\r' && this.buf[idx + 1] === '\n' ? 2 : 1;
      this.buf = this.buf.slice(idx + nl);
      this.line(line);
    }
  }

  line(line) {
    if (line === '') {
      this.dispatch();
      return;
    }
    if (line.startsWith(':')) return; // コメント（keepalive）
    const c = line.indexOf(':');
    const field = c === -1 ? line : line.slice(0, c);
    let value = c === -1 ? '' : line.slice(c + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    switch (field) {
      case 'event': this.event = value; break;
      case 'data': this.data.push(value); break;
      case 'id': if (!value.includes('\0')) this.id = value; break;
      case 'retry': if (/^\d+$/.test(value)) this.retry = Number(value); break;
      default: break;
    }
  }

  dispatch() {
    if (this.id !== undefined) this.lastId = this.id;
    if (this.data.length || this.event) {
      const raw = this.data.join('\n');
      let data = raw;
      try { data = raw ? JSON.parse(raw) : {}; } catch { /* 文字列のまま */ }
      this.onEvent({ type: this.event || 'message', id: this.lastId, data });
    }
    this.reset();
  }
}

export class EventStream {
  constructor() {
    this.lastEventId = null;
    this.ctrl = null;
    this.stopped = true;
    this.backoff = 1000;
    this.timer = null;
  }

  start() {
    if (!this.stopped) return;
    this.stopped = false;
    this.connect();
  }

  stop() {
    this.stopped = true;
    clearTimeout(this.timer);
    if (this.ctrl) this.ctrl.abort();
    this.ctrl = null;
    emit('sse-status', { state: 'closed' });
  }

  async connect() {
    if (this.stopped) return;
    const token = getToken();
    if (!token) return;
    this.ctrl = new AbortController();
    emit('sse-status', { state: 'connecting' });
    const headers = { Authorization: `Bearer ${token}`, Accept: 'text/event-stream' };
    if (this.lastEventId !== null) headers['Last-Event-ID'] = this.lastEventId;
    let res;
    try {
      res = await fetch(`${BASE}/events`, { headers, signal: this.ctrl.signal, cache: 'no-store', credentials: 'omit' });
    } catch (e) {
      if (e.name === 'AbortError' || this.stopped) return;
      this.schedule();
      return;
    }
    if (res.status === 401) {
      emit('unauthorized', null);
      this.stopped = true;
      return;
    }
    if (res.status === 403) {
      emit('sse-status', { state: 'forbidden' });
      this.stopped = true;
      return;
    }
    if (!res.ok || !res.body) {
      this.schedule();
      return;
    }
    emit('sse-status', { state: 'open' });
    const parser = new EventStreamParser((ev) => this.handle(ev));
    const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
    let received = false;
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        received = true;
        parser.feed(value);
      }
    } catch (e) {
      if (e.name === 'AbortError' || this.stopped) return;
    }
    if (parser.retry) this.backoff = parser.retry;
    else if (received) this.backoff = 1000;
    this.schedule();
  }

  handle(ev) {
    if (ev.id !== null && ev.id !== undefined) this.lastEventId = ev.id;
    this.backoff = 1000;
    if (ev.type === 'resync') {
      emit('resync', { id: ev.id });
      return;
    }
    emit('sse', ev);
  }

  schedule() {
    if (this.stopped) return;
    const wait = this.backoff;
    this.backoff = Math.min(this.backoff * 2, 30000);
    emit('sse-status', { state: 'retry', retryIn: wait });
    clearTimeout(this.timer);
    this.timer = setTimeout(() => this.connect(), wait);
  }
}
