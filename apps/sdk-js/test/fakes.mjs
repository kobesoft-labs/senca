// 試験用の偽物: WebSocket・RTCPeerConnection と、シグナリング v1 を話す小さなサーバー（台本で動かす）。
// SDP は本物ではなく、m 行を 1 行ずつ書いた文字列（`m=video 0 sendonly rids=r0;r1` など）。

export const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms));

/** 条件が真になるまで待つ。 */
export async function until(f, ms = 2000, what = 'condition') {
  const end = Date.now() + ms;
  for (;;) {
    const v = await f();
    if (v) return v;
    if (Date.now() > end) throw new Error(`timeout waiting for ${what}`);
    await tick(2);
  }
}

class Emitter {
  constructor() { this._l = {}; }
  addEventListener(t, f) { (this._l[t] ??= new Set()).add(f); }
  removeEventListener(t, f) { this._l[t]?.delete(f); }
  _fire(t, ev = {}) {
    const on = this['on' + t];
    if (typeof on === 'function') on.call(this, ev);
    for (const f of [...(this._l[t] ?? [])]) f(ev);
  }
}

export class FakeTrack extends Emitter {
  constructor(kind, id = Math.random().toString(36).slice(2)) {
    super();
    this.kind = kind;
    this.id = id;
    this.enabled = true;
    this.readyState = 'live';
  }
}

export class FakeMediaStream {
  constructor(tracks = []) { this._t = [...tracks]; this.id = Math.random().toString(36).slice(2); }
  getTracks() { return [...this._t]; }
}

class FakeSender {
  constructor(track, encodings) {
    this.track = track;
    this.params = { encodings: encodings ?? [{}] };
  }
  async replaceTrack(t) { this.track = t; }
  getParameters() { return structuredClone(this.params); }
  async setParameters(p) { this.params = p; }
}

class FakeTransceiver {
  constructor(kind, direction, track, init) {
    this.mid = null;
    this.direction = direction;
    this.currentDirection = null;
    this.kind = kind;
    this.sender = new FakeSender(track ?? null, init?.sendEncodings);
    this.receiver = { track: new FakeTrack(kind) };
    this.encodings = init?.sendEncodings;
    this.codecPrefs = null;
  }
  setCodecPreferences(c) { this.codecPrefs = c; }
}

export class FakeDataChannel extends Emitter {
  constructor(label, init, pc) {
    super();
    this.label = label;
    this.init = init;
    this.readyState = 'connecting';
    this.binaryType = 'blob';
    this.bufferedAmount = 0;
    this.bufferedAmountLowThreshold = 0;
    this.sent = [];
    this.pc = pc;
  }
  send(d) {
    if (this.readyState !== 'open') throw new Error('InvalidStateError: not open');
    this.sent.push(d);
    this.pc.onDcSend?.(this, d);
  }
  close() {
    if (this.readyState === 'closed') return;
    this.readyState = 'closed';
    this._fire('close');
  }
  /** サーバーから届いた */
  _receive(data) { this._fire('message', { data }); }
  _open() { if (this.readyState === 'connecting') { this.readyState = 'open'; this._fire('open'); } }
}

export const CAPS = {
  video: { codecs: [{ mimeType: 'video/VP8', clockRate: 90000 }, { mimeType: 'video/H264', clockRate: 90000, sdpFmtpLine: 'packetization-mode=0;profile-level-id=42e01f' }, { mimeType: 'video/H264', clockRate: 90000, sdpFmtpLine: 'packetization-mode=1;profile-level-id=42e01f' }, { mimeType: 'video/rtx', clockRate: 90000 }, { mimeType: 'video/VP9', clockRate: 90000 }] },
  audio: { codecs: [{ mimeType: 'audio/opus', clockRate: 48000, channels: 2 }, { mimeType: 'audio/red', clockRate: 48000 }] },
};

export function makeEnv(server) {
  const pcs = [];
  class PC extends Emitter {
    constructor(config) {
      super();
      this.config = config;
      this.transceivers = [];
      this.dcs = [];
      this.connectionState = 'new';
      this.signalingState = 'stable';
      this.localDescription = null;
      this.nextMid = 0;
      this.offers = [];
      this.closed = false;
      pcs.push(this);
      server.pcs.push(this);
    }
    addTransceiver(trackOrKind, init = {}) {
      const kind = typeof trackOrKind === 'string' ? trackOrKind : trackOrKind.kind;
      const t = new FakeTransceiver(kind, init.direction ?? 'sendrecv', typeof trackOrKind === 'string' ? null : trackOrKind, init);
      this.transceivers.push(t);
      return t;
    }
    getTransceivers() { return [...this.transceivers]; }
    createDataChannel(label, init) {
      const dc = new FakeDataChannel(label, init, this);
      this.dcs.push(dc);
      if (this.connectionState === 'connected') queueMicrotask(() => dc._open());
      return dc;
    }
    async createOffer(opts = {}) {
      const lines = ['v=0'];
      if (opts.iceRestart) lines.push('a=ice-restart');
      for (const t of this.transceivers) {
        const rids = t.encodings?.filter((e) => e.rid).map((e) => e.rid) ?? [];
        lines.push(`m=${t.kind} ${t.mid ?? '?'} ${t.direction}${rids.length ? ' rids=' + rids.join(';') : ''}${t.encodings?.[0]?.maxBitrate ? ' max=' + t.encodings.map((e) => e.maxBitrate).join(';') : ''}`);
      }
      if (this.dcs.length) lines.push('m=application');
      return { type: 'offer', sdp: lines.join('\n'), iceRestart: !!opts.iceRestart };
    }
    async setLocalDescription(d) {
      if (d.type === 'rollback') { this.signalingState = 'stable'; return; }
      for (const t of this.transceivers) if (t.mid === null) t.mid = String(this.nextMid++);
      // mid を入れて書き直す
      let i = 0;
      const fixed = d.sdp.split('\n').map((l) => {
        if (!(l.startsWith('m=audio') || l.startsWith('m=video'))) return l;
        const t = this.transceivers[i++];
        const parts = l.split(' ');
        parts[1] = t.mid;
        return parts.join(' ');
      });
      this.localDescription = { type: 'offer', sdp: fixed.join('\n') };
      this.offers.push(this.localDescription.sdp);
      this.signalingState = 'have-local-offer';
    }
    async setRemoteDescription(d) {
      if (this.signalingState !== 'have-local-offer') throw new Error('InvalidStateError: no local offer');
      this.remoteDescription = d;
      this.signalingState = 'stable';
      if (this.connectionState !== 'connected' && !server.holdIce) setTimeout(() => this._setState('connected'), 1);
    }
    _setState(s) {
      if (this.closed) return;
      this.connectionState = s;
      this._fire('connectionstatechange');
      if (s === 'connected') for (const dc of this.dcs) dc._open();
    }
    async getStats() {
      const m = new Map();
      for (const t of this.transceivers) {
        if (!t.mid) continue;
        if (t.direction === 'recvonly') m.set('in' + t.mid, { type: 'inbound-rtp', kind: t.kind, mid: t.mid, bytesReceived: 1000, packetsReceived: 10, packetsLost: 0, framesDecoded: t.kind === 'video' ? 42 : undefined, frameWidth: t.kind === 'video' ? 640 : undefined });
        if (t.direction === 'sendonly') for (const e of t.encodings ?? [{}]) m.set('out' + t.mid + (e.rid ?? ''), { type: 'outbound-rtp', kind: t.kind, mid: t.mid, rid: e.rid, bytesSent: 5000, packetsSent: 50 });
      }
      m.set('cp', { type: 'candidate-pair', nominated: true, currentRoundTripTime: 0.012, availableOutgoingBitrate: 2_000_000, bytesSent: 9, bytesReceived: 8 });
      for (const dc of this.dcs) m.set('dc' + dc.label, { type: 'data-channel', label: dc.label, state: dc.readyState, messagesSent: dc.sent.length, messagesReceived: 0, bytesSent: 0, bytesReceived: 0 });
      return m;
    }
    close() {
      this.closed = true;
      this.connectionState = 'closed';
      for (const dc of this.dcs) dc.close();
    }
  }

  class WS extends Emitter {
    constructor(url, protocol) {
      super();
      this.url = url;
      this.requested = protocol;
      this.protocol = '';
      this.readyState = 0;
      this.sentMessages = [];
      server._accept(this);
    }
    send(text) {
      if (this.readyState !== 1) throw new Error('InvalidStateError');
      const m = JSON.parse(text);
      this.sentMessages.push(m);
      server._onClient(this, m);
    }
    close(code = 1000, reason = '') {
      if (this.readyState >= 2) return;
      this.readyState = 3;
      queueMicrotask(() => this._fire('close', { code, reason }));
      server._onClose(this, code);
    }
    /** サーバー側から */
    _push(m) { if (this.readyState === 1) queueMicrotask(() => this._fire('message', { data: JSON.stringify(m) })); }
    _serverClose(code, reason = '') {
      if (this.readyState >= 2) return;
      this.readyState = 3;
      queueMicrotask(() => this._fire('close', { code, reason }));
    }
  }

  return {
    pcs,
    environment: {
      WebSocket: WS,
      RTCPeerConnection: PC,
      MediaStream: FakeMediaStream,
      RTCRtpSender: { getCapabilities: (k) => CAPS[k] },
      RTCRtpReceiver: { getCapabilities: (k) => CAPS[k] },
    },
  };
}

/** SDP（偽物）の m 行。 */
export function parseFakeSdp(sdp) {
  return sdp.split('\n').filter((l) => l.startsWith('m=audio') || l.startsWith('m=video')).map((l) => {
    const [m, mid, dir, ...rest] = l.split(' ');
    const rids = rest.find((x) => x.startsWith('rids='))?.slice(5).split(';') ?? [];
    return { kind: m.slice(2), mid, direction: dir, rids };
  });
}

/**
 * シグナリング v1 の台本のサーバー。既定の振る舞い:
 * connect → connected（`room` の参加者・recv_slots）、offer → answer（受ける枠に部屋のトラックを順に割り当て）、
 * 他の message は記録するだけ。`script.onConnect(conn, msg)` などで差し替える。
 */
export class FakeServer {
  constructor(opts = {}) {
    this.conns = [];
    this.pcs = [];
    this.seq = 0;
    this.participants = opts.participants ?? [];
    this.recvSlots = opts.recvSlots ?? null;
    this.room = { video_codec: 'vp8', audio_codec: 'opus', ...(opts.room ?? {}) };
    this.script = opts.script ?? {};
    this.holdIce = false;
    this.refuse = false;
  }

  _accept(ws) {
    const conn = { ws, id: `c${++this.seq}`, received: [], offers: [], url: ws.url, slots: new Map() };
    this.conns.push(conn);
    ws.conn = conn;
    queueMicrotask(() => {
      if (this.refuse || this.script.refuse?.(ws.url)) { ws.readyState = 3; ws._fire('error'); ws._fire('close', { code: 1006 }); return; }
      ws.readyState = 1;
      ws.protocol = ws.requested;
      ws._fire('open');
    });
  }

  get last() { return this.conns[this.conns.length - 1]; }

  _onClient(ws, m) {
    const c = ws.conn;
    c.received.push(m);
    const s = this.script;
    if (s.onMessage && s.onMessage(c, m) === false) return;
    if (m.type === 'connect') {
      c.connect = m;
      if (s.onConnect) return s.onConnect(c, m);
      return this.sendConnected(c);
    }
    if (m.type === 'offer') {
      c.offers.push(m.sdp);
      if (s.onOffer) return s.onOffer(c, m);
      return this.answer(c, m.sdp);
    }
  }

  _onClose(ws, code) { ws.conn.closedByClient = code; }

  sendConnected(c, extra = {}) {
    const tracks = this.participants.flatMap((p) => p.tracks);
    const want = this.recvSlots ?? {
      audio: tracks.filter((t) => t.kind === 'audio').length,
      video: tracks.filter((t) => t.kind === 'video').length,
    };
    c.ws._push({
      type: 'connected', version: 1, connection_id: c.id, client_id: c.connect.client_id, bundle_id: c.id, room_id: c.connect.room_id,
      role: c.connect.role, multistream: true, e2ee: false, video_codec: this.room.video_codec, audio_codec: 'opus',
      max_video_bitrate_kbps: this.room.max_video_bitrate_kbps,
      participants: this.participants, recv_slots: want, data_channels: (c.connect.data_channels ?? []).filter((d) => d.label !== 'signaling'),
      signaling_data_channel: !!c.connect.signaling?.data_channel, ping_interval_ms: 10000, metadata: { name: c.connect.client_id ?? null },
      ...extra,
    });
  }

  /** 受ける枠に部屋のトラックを割り当てて answer する。 */
  answer(c, sdp, extra = {}) {
    const ms = parseFakeSdp(sdp);
    const tracks = this.participants.flatMap((p) => p.tracks.map((t) => ({ ...t, client_id: p.client_id })));
    const used = new Set();
    const slots = [];
    for (const m of ms.filter((x) => x.direction === 'recvonly')) {
      const prev = c.slots.get(m.mid);
      let t = prev && tracks.find((x) => x.track_id === prev.track_id);
      if (!t) t = tracks.find((x) => x.kind === m.kind && !used.has(x.track_id) && ![...c.slots.values()].some((s) => s.track_id === x.track_id && s.mid !== m.mid));
      if (t) used.add(t.track_id);
      const slot = t ? { mid: m.mid, kind: m.kind, state: 'active', track_id: t.track_id, connection_id: t.connection_id, client_id: t.client_id } : { mid: m.mid, kind: m.kind, state: 'empty' };
      c.slots.set(m.mid, slot);
      slots.push(slot);
    }
    const published = ms.filter((x) => x.direction === 'sendonly').map((x) => ({ mid: x.mid, track_id: `${c.id}.${x.mid}`, kind: x.kind, ...(x.rids.length ? { rids: x.rids } : {}) }));
    c.ws._push({ type: 'answer', sdp: 'v=0 answer', slots, published, ...extra });
  }

  push(c, m) { c.ws._push(m); }
}

export function participant(id, clientId, kinds = ['audio', 'video'], rids = []) {
  return {
    connection_id: id, client_id: clientId, bundle_id: id, role: 'sendrecv', metadata: { name: clientId },
    tracks: kinds.map((k, i) => ({ track_id: `${id}.${i}`, connection_id: id, kind: k, ...(k === 'video' && rids.length ? { rids } : {}) })),
  };
}
