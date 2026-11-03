// セッションが利用者に渡すもの: 参加者・受けているトラック・公開・届いた message。

import { SencaError } from '../errors.js';
import { Broadcaster } from '../internal/broadcast.js';
import { deferred, withTimeout, abortable } from '../internal/async.js';

/** @typedef {import('./protocol.js').Role} Role */
/** @typedef {import('./protocol.js').Kind} Kind */
/** @typedef {import('./protocol.js').WireParticipant} WireParticipant */
/** @typedef {import('./media.js').InboundStats} InboundStats */

/**
 * 参加者が公開しているトラック（受けているかどうかに依らない）。
 * @typedef {object} PublishedTrackInfo
 * @property {string} id トラック ID
 * @property {Kind} kind
 * @property {readonly string[]} rids サイマルキャストの層（低い順、無ければ空）
 */

/**
 * セッションの中から RemoteTrack・Participant が使う口（内部）。
 * @typedef {object} SessionPort
 * @property {(trackId: string, rid: string | null) => Promise<void>} requestLayer
 * @property {(mid: string, trackId: string) => Promise<InboundStats | undefined>} inboundStats
 * @property {() => readonly string[]} focused
 * @property {(participantId: string) => RemoteTrack[]} subscriptionsOf
 */

/** 部屋の他の参加者（自分は含まない）。オブジェクトは参加している間同じもので、metadata などはその場で変わる。 */
export class Participant {
  /**
   * @internal
   * @param {WireParticipant} w
   * @param {SessionPort} port
   */
  constructor(w, port) {
    /** @type {SessionPort} */
    this._port = port;
    /** connection_id（接続ごとに変わる）。 */
    this.id = w.connection_id;
    /** client_id（トークン・認証 Webhook が決めたもの）。 */
    this.clientId = w.client_id;
    this.bundleId = w.bundle_id;
    /** @type {Role} */
    this.role = w.role;
    /** 参加トークン・認証 Webhook・REST が決めた、他の参加者に見せる metadata。 */
    /** @type {unknown} */
    this.metadata = w.metadata ?? null;
    /** 別のノードの参加者ならそのノード（グリッドのカスケード）。 */
    this.node = w.node;
    /** @type {Map<string, PublishedTrackInfo>} 公開しているトラック（受けているかどうかに依らない）。 */
    this.tracks = new Map();
    for (const t of w.tracks ?? []) this.tracks.set(t.track_id, { id: t.track_id, kind: t.kind, rids: Object.freeze([...(t.rids ?? [])]) });
  }

  /** 別のノードにいる参加者か。 */
  get remote() {
    return this.node !== undefined;
  }

  /** スポットライトで注目されているか。 */
  get focused() {
    return this._port.focused().includes(this.id);
  }

  /** この参加者から受けているトラック。 */
  get subscriptions() {
    return this._port.subscriptionsOf(this.id);
  }

  toJSON() {
    return { id: this.id, clientId: this.clientId, bundleId: this.bundleId, role: this.role, metadata: this.metadata, node: this.node, tracks: [...this.tracks.values()] };
  }
}

/**
 * RemoteTrack の出来事。
 * @typedef {{type: 'layer', rid: string | null} | {type: 'paused'} | {type: 'resumed'} | {type: 'ended', reason: string}} RemoteTrackEvent
 */

/**
 * 受けているトラック。サーバーが受信の枠にこのトラックを割り当てている間だけ生きる（枠が別の送り手へ替わる・送り手が抜ける・
 * 再接続で `ended`）。`mediaStreamTrack` はブラウザーの受信のトラックで、枠が使い回されると次の RemoteTrack も同じものを持つ。
 */
export class RemoteTrack {
  /**
   * @internal
   * @param {SessionPort} port
   * @param {{id: string, kind: Kind, participant: Participant | undefined, participantId: string | undefined, clientId: string | undefined,
   *   mid: string, track: MediaStreamTrack, rids: readonly string[], paused: boolean, MediaStream?: typeof MediaStream}} p
   */
  constructor(port, p) {
    /** @type {SessionPort} */
    this._port = port;
    /** トラック ID（`{connection_id}.{mid}`）。 */
    this.id = p.id;
    /** @type {Kind} */
    this.kind = p.kind;
    /** 送り手（`connected`・`connection.created` で知らない送り手なら undefined）。 */
    this.participant = p.participant;
    /** 送り手の connection_id。 */
    this.participantId = p.participantId;
    this.clientId = p.clientId;
    /** 受信の枠（m 行）の mid。 */
    this.mid = p.mid;
    /** ブラウザーの受信のトラック。 */
    this.mediaStreamTrack = p.track;
    /** `mediaStreamTrack` だけを入れた MediaStream（`<video>.srcObject` にそのまま渡せる）。 */
    this.stream = p.MediaStream ? new p.MediaStream([p.track]) : undefined;
    /** サイマルキャストの層（低い順）。 */
    this.rids = p.rids;
    /** サーバーが今配っている層（`layer.changed` で更新。まだ知らなければ null）。 @type {string | null} */
    this.layer = null;
    /** 選んだ層（`setLayer`、既定 `'auto'`）。 @type {string} */
    this.requestedLayer = 'auto';
    /** `active`（届いている）・`paused`（スポットライトの外などで止めている）・`ended`。 @type {'active' | 'paused' | 'ended'} */
    this.state = p.paused ? 'paused' : 'active';
    /** @type {Broadcaster<RemoteTrackEvent>} */
    this._hub = new Broadcaster();
    /** @type {import('../internal/async.js').Deferred<{reason: string}>} */
    this._ended = deferred();
    /** 終わったとき（理由つき）に解決する。 */
    this.ended = this._ended.promise;
    /** @type {Array<{rid: string, done: () => void}>} */
    this._layerWaiters = [];
  }

  /**
   * 受ける層を選ぶ。`'auto'` はサーバーが帯域で選ぶ。層を指定したときは、サーバーがその層へ切り替えた（`layer.changed`）ときに
   * 解決する（キーフレームを待つので少しかかる）。送り手がその層を送っていなければ `timeoutMs`（既定 5000）で `timeout`。
   * @param {string} layer `'r0'` などの rid か `'auto'`
   * @param {{signal?: AbortSignal, timeoutMs?: number}} [opts]
   * @returns {Promise<void>}
   */
  async setLayer(layer, opts = {}) {
    if (this.state === 'ended') throw new SencaError('invalid_argument', 'トラックはもう終わっている');
    if (this.kind !== 'video') throw new SencaError('invalid_argument', '層を選べるのは映像だけ');
    const rid = layer === 'auto' ? null : layer;
    if (rid !== null && !this.rids.includes(rid)) {
      throw new SencaError('invalid_argument', `層 ${rid} はこのトラックに無い（${this.rids.join(', ') || 'サイマルキャストでない'}）`);
    }
    this.requestedLayer = layer;
    /** @type {Promise<void> | null} */
    let reached = null;
    if (rid !== null && this.layer !== rid) {
      const d = deferred();
      const w = { rid, done: () => d.resolve(undefined) };
      this._layerWaiters.push(w);
      reached = d.promise.finally(() => { this._layerWaiters = this._layerWaiters.filter((x) => x !== w); });
      reached.catch(() => {});
      this.ended.then(() => d.reject(new SencaError('invalid_argument', 'トラックが終わった')));
    }
    await abortable(this._port.requestLayer(this.id, rid), opts.signal);
    if (reached) await abortable(withTimeout(reached, opts.timeoutMs ?? 5000, `層 ${rid} への切り替え`), opts.signal);
  }

  /**
   * このトラックの出来事（層の変化・一時停止・終わり）。終わると反復も終わる。
   * @param {{signal?: AbortSignal}} [opts]
   * @returns {AsyncIterableIterator<RemoteTrackEvent>}
   */
  events(opts = {}) {
    return this._hub.subscribe({ signal: opts.signal });
  }

  /** 受信の統計（終わっていれば undefined）。 */
  async stats() {
    if (this.state === 'ended') return undefined;
    return this._port.inboundStats(this.mid, this.id);
  }

  /**
   * `<video>`・`<audio>` に付ける（`srcObject` を設定して返す）。
   * @template {HTMLMediaElement} E
   * @param {E} el
   * @returns {E}
   */
  attach(el) {
    if (this.stream) el.srcObject = this.stream;
    return el;
  }

  /** @param {string | null} rid @internal */
  _onLayer(rid) {
    this.layer = rid;
    for (const w of [...this._layerWaiters]) if (w.rid === rid) w.done();
    this._hub.emit({ type: 'layer', rid });
  }

  /** @param {boolean} paused @internal */
  _onPaused(paused) {
    if (this.state === 'ended') return;
    const next = paused ? 'paused' : 'active';
    if (next === this.state) return;
    this.state = next;
    this._hub.emit(paused ? { type: 'paused' } : { type: 'resumed' });
  }

  /** @param {string} reason @internal */
  _end(reason) {
    if (this.state === 'ended') return;
    this.state = 'ended';
    this._hub.emit({ type: 'ended', reason });
    this._hub.close();
    this._ended.resolve({ reason });
  }

  toJSON() {
    return { id: this.id, kind: this.kind, participantId: this.participantId, clientId: this.clientId, mid: this.mid, rids: this.rids, layer: this.layer, state: this.state };
  }
}

/**
 * 公開の指定。
 * @typedef {object} PublishOptions
 * @property {boolean | import('./media.js').SimulcastLayer[] | {layers?: import('./media.js').SimulcastLayer[]}} [simulcast]
 *   サイマルキャスト（`true` で既定の 3 層 r0・r1・r2）
 * @property {number} [maxBitrateKbps] 映像の上限（サイマルキャストなら一番上の層）。部屋の上限があればそれ以下
 * @property {number} [audioBitrateKbps] 音声の上限
 * @property {import('./protocol.js').VideoCodec} [codec] 部屋のコーデックと違えば `unsupported_codec`（部屋が決める。確かめるために書ける）
 * @property {AbortSignal} [signal]
 */

/** 送っているトラック 1 本。 */
export class LocalTrack {
  /**
   * @internal
   * @param {MediaStreamTrack} track
   * @param {Publication} pub
   */
  constructor(track, pub) {
    /** @type {Kind} */
    this.kind = /** @type {Kind} */ (track.kind);
    /** 送っている MediaStreamTrack（`replace()` で替わる）。 */
    this.mediaStreamTrack = track;
    /** サーバーが付けたトラック ID（answer の `published`。再接続で変わる）。 @type {string | undefined} */
    this.id = undefined;
    /** 送る m 行の mid。 @type {string | undefined} */
    this.mid = undefined;
    /** サイマルキャストの層（低い順）。 @type {readonly string[]} */
    this.rids = [];
    /** @type {Publication} */
    this.publication = pub;
    /** @type {RTCRtpTransceiver | undefined} @internal */
    this._transceiver = undefined;
  }

  /**
   * 別の MediaStreamTrack に差し替える（再交渉しない。カメラの切り替えなど）。
   * @param {MediaStreamTrack | null} track
   */
  async replace(track) {
    if (track && track.kind !== this.kind) throw new SencaError('invalid_argument', `${this.kind} のトラックに ${track.kind} は入れられない`);
    if (this._transceiver) await this._transceiver.sender.replaceTrack(track);
    if (track) this.mediaStreamTrack = track;
  }

  /** 送るのを止める・再開する（MediaStreamTrack.enabled。止めている間は黒・無音を送る）。 */
  get enabled() {
    return this.mediaStreamTrack.enabled;
  }

  set enabled(v) {
    this.mediaStreamTrack.enabled = v;
  }
}

/** `session.publish()` の結果。 */
export class Publication {
  /**
   * @internal
   * @param {MediaStreamTrack[]} tracks
   * @param {PublishOptions} opts
   * @param {MediaStream | undefined} stream
   * @param {(p: Publication) => Promise<void>} unpublish
   */
  constructor(tracks, opts, stream, unpublish) {
    /** @type {LocalTrack[]} */
    this.tracks = tracks.map((t) => new LocalTrack(t, this));
    this.options = opts;
    this.stream = stream;
    /** @internal */
    this._unpublish = unpublish;
    this.active = true;
  }

  /** @param {Kind} kind */
  track(kind) {
    return this.tracks.find((t) => t.kind === kind);
  }

  get audio() {
    return this.track('audio');
  }

  get video() {
    return this.track('video');
  }

  /** 公開をやめる（送る m 行を止めて再交渉する。他の参加者には track.unpublished が届く）。 */
  async unpublish() {
    if (!this.active) return;
    this.active = false;
    await this._unpublish(this);
  }

  /**
   * 映像の上限を変える（再交渉しない。サイマルキャストなら一番上の層を、他の層は比で縮める）。
   * @param {number} kbps
   */
  async setMaxBitrate(kbps) {
    const v = this.video?._transceiver;
    if (!v) return;
    const params = v.sender.getParameters();
    const encs = params.encodings ?? [];
    const top = encs.length ? encs[encs.length - 1].maxBitrate : undefined;
    for (const e of encs) {
      if (top && e.maxBitrate && encs.length > 1) e.maxBitrate = Math.round((e.maxBitrate / top) * kbps * 1000);
      else e.maxBitrate = kbps * 1000;
    }
    await v.sender.setParameters(params);
    this.options = { ...this.options, maxBitrateKbps: kbps };
  }
}

/** 届いたアプリの message（DataChannel の `#` ラベル、またはシグナリングの `message`）。 */
export class InboundMessage {
  /**
   * @internal
   * @param {string} label
   * @param {string | Uint8Array} data
   * @param {string | null} from
   * @param {'datachannel' | 'signaling'} via
   * @param {(id: string) => Participant | undefined} lookup
   */
  constructor(label, data, from, via, lookup) {
    this.label = label;
    /** 本文（text は string、binary は Uint8Array）。 */
    this.data = data;
    /**
     * 送り手の connection_id。DataChannel で部屋全体に届いたもの（送り手を運ばない）と API からのものは null。
     * 送り手を知りたいなら `send(label, data, {to})`（シグナリング経由、`from` が付く）を使うか本文に入れる。
     */
    this.from = from;
    /** 届いた経路。 */
    this.via = via;
    /** @internal */
    this._lookup = lookup;
    this.receivedAt = Date.now();
  }

  /** 送り手（分からなければ undefined）。 */
  get sender() {
    return this.from ? this._lookup(this.from) : undefined;
  }

  get binary() {
    return typeof this.data !== 'string';
  }

  /** 本文を文字列で。 */
  text() {
    return typeof this.data === 'string' ? this.data : new TextDecoder().decode(this.data);
  }

  /** 本文を JSON として読む（`send()` にオブジェクトを渡すと JSON で送る）。 */
  json() {
    return JSON.parse(this.text());
  }
}
