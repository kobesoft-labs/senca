// リアルタイムの部屋のセッション（シグナリング v1、docs/realtime/signaling.md）。
//
// 利用者は SDP・受信の枠（slot）・再交渉を見ない。Session が次を受け持つ:
// - 参加（connect → connected → 最初の offer/answer → ピア接続がつながる）
// - 受信の枠: `connected.recv_slots`・`renegotiate` の数まで recvonly の transceiver を足して offer し直す。
//   offer は 1 つずつ（answer の前に次を出さない）。answer を待つ間の `renegotiate` は answer の後でまとめて出す。
// - 枠の割り当て（answer・`slots`）→ RemoteTrack の始まり・一時停止・終わり
// - 公開・公開の取り消し（送る m 行の追加・inactive → 再交渉）
// - DataChannel（`#` ラベル）とシグナリングの message、シグナリングの DataChannel への切り替え（§6）
// - ping/pong と、ping が来ないときの見張り
// - 切断からの回復: ICE が切れたら ICE restart（offer の iceRestart）、だめなら・シグナリングが切れたら参加し直す
//   （プロトコルに再開（resume）は無いので、新しい接続として入り直し、公開も張り直す）。指数バックオフ。
// - redirect（別のノードへ入り直す）、誤り（fatal なら閉じる、そうでなければその要求だけ失敗）

import { SencaError, abortReason, isAbortError } from '../errors.js';
import { Broadcaster } from '../internal/broadcast.js';
import { abortable, backoffDelay, deferred, linkedController, sleep, withTimeout } from '../internal/async.js';
import { CODE_BY_CLOSE, MAX_APP_MESSAGE_BYTES, PROTOCOL_VERSION, SIGNALING_LABEL, SUBPROTOCOL } from './protocol.js';
import { compactStats, encodingsFor, preferCodec, simulcastLayers, summarizeStats } from './media.js';
import { InboundMessage, Participant, Publication, RemoteTrack } from './objects.js';

export const SDK_NAME = '@senca/sdk';
export const SDK_VERSION = '0.0.0';

/** @typedef {import('./protocol.js').Role} Role */
/** @typedef {import('./protocol.js').Kind} Kind */
/** @typedef {import('./protocol.js').ServerMessage} ServerMessage */
/** @typedef {import('./protocol.js').ClientMessage} ClientMessage */
/** @typedef {import('./protocol.js').ConnectMessage} ConnectMessage */
/** @typedef {import('./protocol.js').ConnectedMessage} ConnectedMessage */
/** @typedef {import('./protocol.js').WireSlot} WireSlot */
/** @typedef {import('./protocol.js').WireDataChannel} WireDataChannel */
/** @typedef {import('./media.js').SessionStats} SessionStats */
/** @typedef {import('./objects.js').PublishOptions} PublishOptions */

/**
 * DataChannel の宣言（`'#chat'` だけでもよい）。
 * @typedef {string | {label: string, direction?: 'sendrecv' | 'sendonly' | 'recvonly', ordered?: boolean,
 *   maxRetransmits?: number, maxPacketLifeTime?: number}} DataChannelOption
 */

/**
 * 再接続の方針（`false` で再接続しない）。
 * @typedef {object} ReconnectOptions
 * @property {number} [maxAttempts] あきらめるまでの回数（既定 10）
 * @property {number} [initialDelayMs] 最初の待ち（既定 500）
 * @property {number} [maxDelayMs] 待ちの上限（既定 10000）
 * @property {number} [factor] 倍率（既定 2）
 * @property {number} [jitter] ゆらぎ（既定 0.2）
 */

/**
 * WebRTC・WebSocket の実装（既定はブラウザーの globalThis。試験や Node.js で差し替える）。
 * @typedef {object} Environment
 * @property {typeof WebSocket} [WebSocket]
 * @property {typeof RTCPeerConnection} [RTCPeerConnection]
 * @property {typeof MediaStream} [MediaStream]
 * @property {{getCapabilities(kind: string): RTCRtpCapabilities | null}} [RTCRtpSender]
 * @property {{getCapabilities(kind: string): RTCRtpCapabilities | null}} [RTCRtpReceiver]
 */

/**
 * `Senca.connect()` の指定。
 * @typedef {object} ConnectOptions
 * @property {string | string[]} url シグナリングの URL（`wss://node.example/signaling`。`https://node.example` なら `/signaling` を足す）。
 *   複数なら再接続のたびに順に替える
 * @property {string} room 部屋の ID
 * @property {string | ((ctx: {room: string, attempt: number}) => string | Promise<string>)} [token]
 *   参加トークン。関数なら参加・再接続のたびに呼ぶ（期限の短いトークンを取り直す）
 * @property {Role} [role] 既定 `sendrecv`
 * @property {string} [clientId]
 * @property {string} [bundleId] 同じ値の接続どうしは互いのトラックを受けない
 * @property {unknown} [metadata] 認証 Webhook へだけ渡す（他の参加者に見せる metadata はトークン・Webhook が決める）
 * @property {boolean} [multistream] 既定 true（false で種類ごとに 1 本だけ受ける）
 * @property {{number: number, focusLayer?: string, unfocused?: 'lowest' | 'off'}} [spotlight] 受け手としてのスポットライト
 * @property {string} [receiveLayer] 受ける映像の最初の層（`'r0'` など。サイマルキャストのトラックを受けたら自動で選ぶ）
 * @property {MediaStream | MediaStreamTrack[] | (PublishOptions & {stream: MediaStream | MediaStreamTrack[]})} [publish]
 *   参加と同時に公開する（最初の offer に入れるので再交渉が 1 回少ない）
 * @property {{codec?: import('./protocol.js').VideoCodec, bitrateKbps?: number} | false} [video] 映像の希望（`false` で送らない）
 * @property {{codec?: 'opus', bitrateKbps?: number} | false} [audio]
 * @property {DataChannelOption[]} [dataChannels] 使う DataChannel（`#` で始まるラベル）
 * @property {{dataChannel?: boolean, closeWebSocket?: boolean}} [signaling] シグナリングを DataChannel へ切り替える（§6）
 * @property {boolean} [e2ee] E2EE の部屋に入る（部屋の設定と同じでなければ `e2ee_mismatch`。暗号化（SFrame）は利用者が Encoded Transform で行う）
 * @property {import('./protocol.js').ForwardingFilter} [forwardingFilter] この接続の購読を絞る
 * @property {RTCIceServer[]} [iceServers]
 * @property {RTCConfiguration} [rtcConfiguration] RTCPeerConnection の設定（bundlePolicy は max-bundle に固定）
 * @property {boolean | ReconnectOptions} [reconnect] 既定 true
 * @property {number} [connectTimeoutMs] 1 回の参加の時間切れ（既定 20000。ピア接続がつながるまで）
 * @property {boolean | {intervalMs?: number}} [reportStats] getStats の要約を pong でサーバーへ送る（REST の統計に出る）
 * @property {AbortSignal} [signal] 参加中に取り消すと AbortError。参加の後に取り消すとセッションを閉じる
 * @property {Environment} [environment]
 * @property {{debug?: (...a: unknown[]) => void, warn?: (...a: unknown[]) => void}} [logger]
 */

/**
 * 部屋の情報（`connected`）。
 * @typedef {object} RoomInfo
 * @property {string} id
 * @property {import('./protocol.js').VideoCodec} videoCodec
 * @property {'opus'} audioCodec
 * @property {boolean} e2ee
 * @property {boolean} multistream
 * @property {number | undefined} maxVideoBitrateKbps
 * @property {{number: number, focusLayer?: string, unfocused?: 'lowest' | 'off'} | undefined} spotlight
 */

/**
 * 回線の状態（`network.status`、0 よい〜3 悪い）。
 * @typedef {{level: number, bandwidthBps: number, rttMs: number}} NetworkStatus
 */

/**
 * 閉じた理由（`session.closed`）。
 * @typedef {object} CloseInfo
 * @property {string} reason `normal`・`kicked`・`room_deleted`・`error`・`connection_failed`・`aborted` など
 * @property {SencaError | undefined} error 誤りで閉じたとき
 */

/**
 * セッションの出来事（`session.events()`）。`type` で見分ける。
 * @typedef {{type: 'participant.joined', participant: Participant}
 *   | {type: 'participant.left', participant: Participant, reason: string}
 *   | {type: 'participant.updated', participant: Participant}
 *   | {type: 'track.published', participant: Participant | undefined, track: import('./objects.js').PublishedTrackInfo}
 *   | {type: 'track.unpublished', participant: Participant | undefined, trackId: string}
 *   | {type: 'track.subscribed', track: RemoteTrack}
 *   | {type: 'track.unsubscribed', track: RemoteTrack, reason: string}
 *   | {type: 'track.paused', track: RemoteTrack}
 *   | {type: 'track.resumed', track: RemoteTrack}
 *   | {type: 'layer.changed', track: RemoteTrack, rid: string | null}
 *   | {type: 'spotlight.changed', focused: readonly string[]}
 *   | {type: 'network', status: NetworkStatus}
 *   | {type: 'push', data: unknown}
 *   | {type: 'signaling.switched'}
 *   | {type: 'ice.restarting'}
 *   | {type: 'ice.restarted'}
 *   | {type: 'reconnecting', attempt: number, delayMs: number, error: SencaError | undefined}
 *   | {type: 'reconnected', connectionId: string}
 *   | {type: 'redirected', location: string}
 *   | {type: 'error', error: SencaError}
 *   | {type: 'closed', reason: string, error: SencaError | undefined}} SessionEvent
 */

/** @typedef {SessionEvent['type']} SessionEventType */

/** 再接続しても直らない誤り。 */
const PERMANENT = new Set([
  'bad_message', 'unsupported_version', 'auth_failed', 'forbidden', 'room_not_found', 'role_violation', 'e2ee_mismatch',
  'unsupported_codec', 'invalid_sdp', 'negotiation_failed', 'unknown_label', 'kicked', 'room_deleted', 'too_many_redirects',
  'invalid_argument', 'unsupported', 'closed',
]);
/** サーバーの切断の理由のうち、再接続しないもの。 */
const FINAL_REASONS = new Set(['normal', 'kicked', 'room_deleted']);
const MAX_REDIRECTS = 3;
const ICE_DISCONNECT_GRACE_MS = 3000;
const ICE_RESTART_TIMEOUT_MS = 10000;
const ANSWER_TIMEOUT_MS = 15000;
const DC_HIGH_WATER = 1 << 20;

/** 1 回の参加（WebSocket・ピア接続・その上の状態）。再接続のたびに作り直す。 */
class Link {
  /** @internal @param {number} epoch @param {string} url */
  constructor(epoch, url) {
    this.epoch = epoch;
    this.url = url;
    /** @type {WebSocket | null} */
    this.ws = null;
    /** @type {RTCPeerConnection | null} */
    this.pc = null;
    /** @type {RTCDataChannel | null} */
    this.sigDc = null;
    this.switched = false;
    this.wsClosedOnPurpose = false;
    /** @type {ConnectedMessage | null} */
    this.connected = null;
    /** @type {import('../internal/async.js').Deferred<ConnectedMessage>} */
    this.connectedD = deferred();
    /** @type {import('../internal/async.js').Deferred<never>} */
    this.lost = deferred();
    /** @type {{audio: RTCRtpTransceiver[], video: RTCRtpTransceiver[]}} */
    this.recv = { audio: [], video: [] };
    this.target = { audio: 0, video: 0 };
    /** @type {Promise<void> | null} */
    this.negotiating = null;
    this.again = false;
    this.iceRestart = false;
    this.restarting = false;
    /** @type {import('../internal/async.js').Deferred<Extract<ServerMessage, {type: 'answer'}>> | null} */
    this.pendingAnswer = null;
    /** @type {Map<string, RemoteTrack>} mid → 今その枠で受けているトラック */
    this.slotTracks = new Map();
    /** @type {Map<string, string | null>} mid → サーバーが配っている層（トラックの前に届いた layer.changed を覚える） */
    this.slotLayer = new Map();
    /** @type {Map<string, string>} mid → 自分の送るトラックの ID */
    this.published = new Map();
    /** @type {Map<string, RTCDataChannel>} */
    this.dcs = new Map();
    /** @type {Array<{reject: (e: unknown) => void}>} */
    this.layerRequests = [];
    /** @type {SencaError | null} */
    this.fatal = null;
    /** @type {string | null} */
    this.disconnectReason = null;
    /** @type {string | null} */
    this.redirect = null;
    this.closed = false;
    /** 参加が済んだ（これより前に失われたら _attempt が誤りを投げる）。 */
    this.joined = false;
    /** @type {Promise<void>} */
    this.chain = Promise.resolve();
    this.lastSeen = Date.now();
    /** @type {ReturnType<typeof setInterval> | null} */
    this.watchdog = null;
    /** @type {ReturnType<typeof setTimeout> | null} */
    this.iceTimer = null;
    /** @type {import('../internal/async.js').Deferred<void>} */
    this.peerUp = deferred();
  }
}

/**
 * 部屋に入る。参加し、最初の交渉が済み、ピア接続がつながったら解決する。
 * @param {ConnectOptions} options
 * @returns {Promise<Session>}
 */
export async function connect(options) {
  const s = new Session(options);
  await s._start();
  return s;
}

/** 部屋のセッション。`Senca.connect()` が返す。 */
export class Session {
  /** @internal @param {ConnectOptions} options */
  constructor(options) {
    if (!options || typeof options !== 'object') throw new SencaError('invalid_argument', 'options が要る');
    if (!options.room) throw new SencaError('invalid_argument', 'room が要る');
    if (!options.url || (Array.isArray(options.url) && !options.url.length)) throw new SencaError('invalid_argument', 'url が要る');
    /** @private */
    this._o = options;
    const g = /** @type {any} */ (globalThis);
    const env = options.environment ?? {};
    /** @private */
    this._env = {
      WebSocket: env.WebSocket ?? g.WebSocket,
      RTCPeerConnection: env.RTCPeerConnection ?? g.RTCPeerConnection,
      MediaStream: env.MediaStream ?? g.MediaStream,
      RTCRtpSender: env.RTCRtpSender ?? g.RTCRtpSender,
      RTCRtpReceiver: env.RTCRtpReceiver ?? g.RTCRtpReceiver,
    };
    if (!this._env.WebSocket || !this._env.RTCPeerConnection) throw new SencaError('unsupported', 'WebSocket と RTCPeerConnection が要る');
    /** @private */
    this._urls = (Array.isArray(options.url) ? options.url : [options.url]).map(normalizeUrl);
    /** @private */
    this._urlIndex = 0;
    /** @private @type {string | null} 次に使う URL（redirect の先） */
    this._nextUrl = null;
    /** @private */
    this._dataChannels = normalizeDataChannels(options.dataChannels);
    /** @private @type {Link | null} */
    this._link = null;
    /** @private */
    this._epoch = 0;
    /** @private @type {Broadcaster<SessionEvent>} */
    this._events = new Broadcaster();
    /** @private @type {Broadcaster<InboundMessage>} */
    this._messages = new Broadcaster();
    /** @private @type {Broadcaster<RemoteTrack>} */
    this._trackHub = new Broadcaster();
    /** @private @type {Map<string, Participant>} */
    this._participants = new Map();
    /** @private @type {Map<string, RemoteTrack>} */
    this._tracks = new Map();
    /** @private @type {Set<Publication>} */
    this._publications = new Set();
    /** @private @type {import('../internal/async.js').Deferred<CloseInfo>} */
    this._closedD = deferred();
    /** @private */
    this._close = new AbortController();
    /** @private @type {Array<() => void>} */
    this._stateWaiters = [];
    /** @private @type {SessionStats | null} */
    this._lastStats = null;
    /** @private @type {unknown} */
    this._pongStats = undefined;
    /** @private @type {ReturnType<typeof setInterval> | null} */
    this._statsTimer = null;
    /** @private @type {readonly string[]} */
    this._focused = Object.freeze([]);

    /** 今の状態。 @type {'connecting' | 'connected' | 'reconnecting' | 'closed'} */
    this.state = 'connecting';
    /** 自分の connection_id（再接続で変わる）。 @type {string} */
    this.connectionId = '';
    /** @type {string | undefined} */
    this.clientId = options.clientId;
    /** @type {string} */
    this.bundleId = '';
    /** @type {Role} */
    this.role = options.role ?? 'sendrecv';
    /** 他の参加者に見せる自分の metadata（トークン・認証 Webhook が決めたもの）。 @type {unknown} */
    this.metadata = null;
    /** @type {RoomInfo | null} */
    this.room = null;
    /** 回線の状態（`network.status` を受けるまで null）。 @type {NetworkStatus | null} */
    this.network = null;
    /** セッションが閉じたら解決する（拒否はしない）。 */
    this.closed = this._closedD.promise;

    /** @private @type {import('./objects.js').SessionPort} */
    this._port = {
      requestLayer: (id, rid) => this._requestLayer(id, rid),
      inboundStats: async (mid, id) => (await this.stats()).inbound.find((x) => x.mid === mid && x.trackId === id),
      focused: () => this._focused,
      subscriptionsOf: (pid) => [...this._tracks.values()].filter((t) => t.participantId === pid),
    };

    const pub = options.publish;
    if (pub) {
      const isOpts = !isMediaSource(pub);
      const src = isOpts ? /** @type {PublishOptions & {stream: MediaStream | MediaStreamTrack[]}} */ (pub).stream : /** @type {MediaStream | MediaStreamTrack[]} */ (pub);
      this._newPublication(src, isOpts ? /** @type {PublishOptions} */ (pub) : {});
    }
  }

  // ---------------------------------------------------------------- 公開の読み取り

  /** 他の参加者（connection_id → Participant）。その場で変わる。 @returns {ReadonlyMap<string, Participant>} */
  get participants() {
    return this._participants;
  }

  /** 今受けているトラック（トラック ID → RemoteTrack）。その場で変わる。 @returns {ReadonlyMap<string, RemoteTrack>} */
  get tracks() {
    return this._tracks;
  }

  /** 自分の公開。 @returns {readonly Publication[]} */
  get publications() {
    return [...this._publications];
  }

  /** スポットライトで注目されている参加者の connection_id（新しく注目した順）。 */
  get spotlight() {
    return { enabled: !!this.room?.spotlight, focused: this._focused };
  }

  // ---------------------------------------------------------------- 非同期イテレーター

  /**
   * 出来事の列。セッションが閉じると（`closed` を流してから）終わる。
   * @param {{types?: SessionEventType[], signal?: AbortSignal}} [opts]
   * @returns {AsyncIterableIterator<SessionEvent>}
   */
  events(opts = {}) {
    const types = opts.types ? new Set(opts.types) : null;
    return this._events.subscribe({ signal: opts.signal, filter: types ? (e) => types.has(e.type) : undefined });
  }

  /**
   * 受けるトラックの列。今受けているものを先に流し、その後に新しく受けたものを流す。
   * @param {{kind?: Kind, participant?: string | Participant, signal?: AbortSignal}} [opts]
   * @returns {AsyncIterableIterator<RemoteTrack>}
   */
  remoteTracks(opts = {}) {
    const pid = typeof opts.participant === 'string' ? opts.participant : opts.participant?.id;
    /** @param {RemoteTrack} t */
    const match = (t) => (!opts.kind || t.kind === opts.kind) && (!pid || t.participantId === pid);
    return this._trackHub.subscribe({ signal: opts.signal, filter: match, initial: () => [...this._tracks.values()].filter(match) });
  }

  /**
   * 届いた message の列（ラベルを省けば全部）。
   * @param {string | {label?: string, signal?: AbortSignal}} [label]
   * @param {{signal?: AbortSignal}} [opts]
   * @returns {AsyncIterableIterator<InboundMessage>}
   */
  messages(label, opts = {}) {
    const o = typeof label === 'object' ? label : { ...opts, label };
    return this._messages.subscribe({ signal: o.signal, filter: o.label ? (m) => m.label === o.label : undefined });
  }

  /**
   * 最初に来た出来事（`type` が一致し `filter` が真のもの）を待つ。
   * @template {SessionEventType} T
   * @param {T} type
   * @param {{filter?: (e: Extract<SessionEvent, {type: T}>) => boolean, signal?: AbortSignal, timeoutMs?: number}} [opts]
   * @returns {Promise<Extract<SessionEvent, {type: T}>>}
   */
  async waitFor(type, opts = {}) {
    const ctl = linkedController(opts.signal, this._close.signal);
    try {
      const it = this.events({ types: [type], signal: ctl.signal });
      const p = (async () => {
        for await (const e of it) {
          const ev = /** @type {Extract<SessionEvent, {type: T}>} */ (e);
          if (!opts.filter || opts.filter(ev)) return ev;
        }
        if (opts.signal?.aborted) throw abortReason(opts.signal);
        throw new SencaError('closed', 'セッションが閉じた');
      })();
      return await withTimeout(p, opts.timeoutMs ?? 0, `${type} を待つ`);
    } finally {
      ctl.controller.abort();
      ctl.dispose();
    }
  }

  // ---------------------------------------------------------------- 操作

  /**
   * カメラ・マイク・画面などを公開する。交渉が済み、サーバーがトラック ID を付けたら解決する。
   * @param {MediaStream | MediaStreamTrack | MediaStreamTrack[]} source
   * @param {PublishOptions} [opts]
   * @returns {Promise<Publication>}
   */
  async publish(source, opts = {}) {
    this._assertOpen();
    if (this.role === 'recvonly') throw new SencaError('role_violation', 'recvonly では公開できない');
    if (opts.codec && this.room && opts.codec !== this.room.videoCodec) {
      throw new SencaError('unsupported_codec', `部屋の映像のコーデックは ${this.room.videoCodec}（${opts.codec} は送れない）`);
    }
    const pub = this._newPublication(source, opts);
    try {
      const link = await this._whenConnected(opts.signal);
      this._attachPublication(link, pub);
      await abortable(this._negotiate(link), opts.signal);
      return pub;
    } catch (e) {
      // 公開できなかった: 取り消す（次の交渉で m 行を止める）
      await this._removePublication(pub).catch(() => {});
      throw e;
    }
  }

  /**
   * アプリの message を送る。`to` を省けば、そのラベルを開いている他の全員へ（DataChannel、開いていなければシグナリング）。
   * `to` を書けばその参加者だけへ（シグナリング経由。受け手には `from` が付く）。
   * 文字列は text、Uint8Array・ArrayBuffer は binary、それ以外は JSON にして text で送る。64 KiB まで。
   * @param {string} label `#` で始まり、`dataChannels` で送る向きに宣言したもの
   * @param {string | ArrayBuffer | ArrayBufferView | object} data
   * @param {{to?: string | Participant | Array<string | Participant>, signal?: AbortSignal}} [opts]
   * @returns {Promise<void>}
   */
  async send(label, data, opts = {}) {
    this._assertOpen();
    const decl = this._dataChannels.find((d) => d.label === label);
    if (!decl) throw new SencaError('unknown_label', `${label} は dataChannels で宣言していない`);
    if (decl.direction === 'recvonly') throw new SencaError('unknown_label', `${label} は recvonly で宣言した`);
    const payload = encodePayload(data);
    const size = typeof payload === 'string' ? new TextEncoder().encode(payload).length : payload.byteLength;
    if (size > MAX_APP_MESSAGE_BYTES) throw new SencaError('message_too_large', `message は 64 KiB まで（${size} byte）`);
    const to = opts.to === undefined ? [] : (Array.isArray(opts.to) ? opts.to : [opts.to]).map((x) => (typeof x === 'string' ? x : x.id));
    const link = await this._whenConnected(opts.signal);
    const dc = link.dcs.get(label);
    if (to.length === 0 && dc && dc.readyState === 'open') {
      if (dc.bufferedAmount > DC_HIGH_WATER) await abortable(drain(dc), opts.signal);
      if (typeof payload === 'string') dc.send(payload);
      else dc.send(/** @type {any} */ (payload));
      return;
    }
    /** @type {ClientMessage} */
    const m = typeof payload === 'string' ? { type: 'message', label, data: payload } : { type: 'message', label, data_base64: toBase64(payload) };
    if (to.length) m.to = to;
    this._send(link, m);
  }

  /**
   * 統計（getStats の要約と、そのままの報告）。
   * @returns {Promise<SessionStats>}
   */
  async stats() {
    const link = this._link;
    if (!link?.pc) throw new SencaError('not_connected', 'つながっていない');
    const report = await link.pc.getStats();
    const s = summarizeStats(report, {
      state: link.pc.connectionState,
      previous: this._lastStats,
      publishedOf: (mid) => link.published.get(mid),
      slotOf: (mid) => {
        const t = link.slotTracks.get(mid);
        return t ? { trackId: t.id, participantId: t.participantId, clientId: t.clientId, layer: t.layer } : undefined;
      },
    });
    this._lastStats = s;
    return s;
  }

  /**
   * 閉じる（サーバーへ `disconnect` を送る）。何度呼んでもよい。
   * @param {string} [reason]
   * @returns {Promise<CloseInfo>}
   */
  async close(reason = 'normal') {
    if (this.state === 'closed') return this.closed;
    const link = this._link;
    if (link && !link.closed) {
      try { this._send(link, { type: 'disconnect', reason }); } catch { /* 送れなくてもよい */ }
      // 送りきってから閉じる
      await sleep(0);
    }
    this._finalize({ reason, error: undefined });
    return this.closed;
  }

  // ---------------------------------------------------------------- 参加

  /** @internal */
  async _start() {
    const sig = this._o.signal;
    try {
      await this._join(sig, 0);
    } catch (e) {
      const err = isAbortError(e) ? e : toSencaError(e);
      if (this._link) this._teardown(this._link);
      this._finalize({ reason: isAbortError(e) ? 'aborted' : 'error', error: err instanceof SencaError ? err : undefined });
      throw err;
    }
    sig?.addEventListener('abort', () => { this.close('aborted'); }, { once: true });
    this._setState('connected');
  }

  /** @private 次に使う URL */
  _pickUrl() {
    if (this._nextUrl) {
      const u = this._nextUrl;
      this._nextUrl = null;
      return u;
    }
    return this._urls[this._urlIndex % this._urls.length];
  }

  /**
   * 1 回の参加（redirect は追う）。
   * @private
   * @param {AbortSignal | undefined} signal
   * @param {number} attempt
   */
  async _join(signal, attempt) {
    let url = this._pickUrl();
    for (let hops = 0; ; hops++) {
      const r = await this._attempt(url, signal, attempt);
      if (!r) return;
      if (hops >= MAX_REDIRECTS) throw new SencaError('too_many_redirects', `redirect が ${MAX_REDIRECTS} 回続いた`, { fatal: true });
      this._emit({ type: 'redirected', location: r });
      url = normalizeUrl(r);
    }
  }

  /**
   * @private
   * @param {string} url
   * @param {AbortSignal | undefined} signal
   * @param {number} attempt
   * @returns {Promise<string | null>} redirect の先（無ければ null）
   */
  async _attempt(url, signal, attempt) {
    const o = this._o;
    const link = new Link(++this._epoch, url);
    this._link = link;
    const ctl = linkedController(signal, this._close.signal);
    // 参加の途中で接続が失われたら、待っているものを全部失敗させる
    /** @template T @param {Promise<T>} p @returns {Promise<T>} */
    const guard = (p) => abortable(Promise.race([p, link.lost.promise]), ctl.signal);
    const timeoutMs = o.connectTimeoutMs ?? 20000;
    const deadline = setTimeout(() => this._lose(link, { error: new SencaError('timeout', `参加が ${timeoutMs} ms で済まない`) }), timeoutMs);
    try {
      const token = typeof o.token === 'function' ? await guard(Promise.resolve(o.token({ room: o.room, attempt }))) : o.token;
      await guard(this._openWebSocket(link));
      const pc = new this._env.RTCPeerConnection({ ...(o.rtcConfiguration ?? {}), iceServers: o.iceServers ?? o.rtcConfiguration?.iceServers ?? [], bundlePolicy: 'max-bundle', rtcpMuxPolicy: 'require' });
      link.pc = pc;
      this._wirePeer(link);
      this._send(link, this._connectMessage(token));
      let connected;
      try {
        connected = await guard(link.connectedD.promise);
      } catch (e) {
        if (link.redirect) return link.redirect;
        throw e;
      }
      this._applyConnected(link, connected);
      // 宣言した DataChannel を開く（最初の offer に m=application を入れる）
      for (const d of connected.data_channels) {
        if (d.label === SIGNALING_LABEL) continue;
        this._openDataChannel(link, d);
      }
      if (connected.signaling_data_channel) this._openSignalingChannel(link);
      for (const p of this._publications) this._attachPublication(link, p);
      const sub = this.role !== 'sendonly';
      link.target = {
        audio: Math.max(connected.recv_slots.audio, sub ? 1 : 0),
        video: Math.max(connected.recv_slots.video, sub ? 1 : 0),
      };
      await guard(this._negotiate(link));
      await guard(link.peerUp.promise);
      this._startWatchdog(link);
      this._startStatsReporting();
      link.joined = true;
      return null;
    } catch (e) {
      if (link.redirect) return link.redirect;
      this._teardown(link);
      throw link.fatal ?? (isAbortError(e) ? e : toSencaError(e));
    } finally {
      clearTimeout(deadline);
      ctl.dispose();
    }
  }

  /** @private @param {string | undefined} token @returns {ConnectMessage} */
  _connectMessage(token) {
    const o = this._o;
    /** @type {ConnectMessage} */
    const m = { type: 'connect', version: PROTOCOL_VERSION, room_id: o.room, role: this.role, sdk: { name: SDK_NAME, version: SDK_VERSION } };
    if (token) m.token = token;
    if (o.clientId) m.client_id = o.clientId;
    if (o.bundleId) m.bundle_id = o.bundleId;
    if (o.metadata !== undefined) m.metadata = o.metadata;
    if (o.multistream === false) m.multistream = false;
    if (o.e2ee) m.e2ee = true;
    if (o.forwardingFilter) m.forwarding_filter = o.forwardingFilter;
    if (o.spotlight) {
      m.spotlight = { number: o.spotlight.number };
      if (o.spotlight.focusLayer) m.spotlight.focus_rid = o.spotlight.focusLayer;
      if (o.spotlight.unfocused) m.spotlight.unfocused = o.spotlight.unfocused;
    }
    // 公開する映像のサイマルキャストの層（受け手の既定の層 receiveLayer も、層を宣言するときだけ送れる）
    const sc = [...this._publications].map((p) => p.options.simulcast).find(Boolean) ?? null;
    const layers = sc ? simulcastLayers(sc) : null;
    if (layers) {
      m.simulcast = { rids: layers.map((l) => l.rid) };
      if (o.receiveLayer && m.simulcast.rids.includes(o.receiveLayer)) m.simulcast.default_rid = o.receiveLayer;
    }
    const kinds = new Set([...this._publications].flatMap((p) => p.tracks.map((t) => t.kind)));
    if (o.video === false) m.video = { enabled: false };
    else if (o.video || kinds.has('video')) {
      m.video = { enabled: true };
      const codec = o.video ? o.video.codec : undefined;
      if (codec) m.video.codec = codec;
      const br = (o.video && o.video.bitrateKbps) || [...this._publications].map((p) => p.options.maxBitrateKbps).find(Boolean);
      if (br) m.video.bitrate_kbps = br;
    }
    if (o.audio === false) m.audio = { enabled: false };
    else if (o.audio && (o.audio.codec || o.audio.bitrateKbps)) {
      m.audio = { enabled: true };
      if (o.audio.codec) m.audio.codec = o.audio.codec;
      if (o.audio.bitrateKbps) m.audio.bitrate_kbps = o.audio.bitrateKbps;
    }
    const dcs = [...this._dataChannels];
    if (o.signaling?.dataChannel) {
      dcs.push({ label: SIGNALING_LABEL });
      m.signaling = { data_channel: true, close_websocket: !!o.signaling.closeWebSocket };
    }
    if (dcs.length) m.data_channels = dcs;
    return m;
  }

  /** @private @param {Link} link */
  _openWebSocket(link) {
    return new Promise((resolve, reject) => {
      /** @type {WebSocket} */
      let ws;
      try {
        ws = new this._env.WebSocket(link.url, SUBPROTOCOL);
      } catch (e) {
        reject(new SencaError('connection_failed', `WebSocket を開けない: ${String(e)}`, { cause: e }));
        return;
      }
      link.ws = ws;
      let opened = false;
      ws.onopen = () => {
        opened = true;
        if (ws.protocol && ws.protocol !== SUBPROTOCOL) {
          ws.close(1002);
          reject(new SencaError('connection_failed', `サーバーが subprotocol ${SUBPROTOCOL} を選ばなかった`));
          return;
        }
        resolve(undefined);
      };
      ws.onerror = () => {
        if (!opened) reject(new SencaError('connection_failed', `${link.url} へつながらない`));
      };
      ws.onmessage = (ev) => this._onText(link, ev.data);
      ws.onclose = (ev) => {
        if (!opened) {
          reject(new SencaError('connection_failed', `${link.url} へつながらない（close ${ev.code}）`, { closeCode: ev.code }));
          return;
        }
        if (link.wsClosedOnPurpose) return;
        // DataChannel へ切り替えて close_websocket なら閉じてよい（サーバーも切断しない）
        if (link.switched && this._o.signaling?.closeWebSocket && link.sigDc?.readyState === 'open') return;
        this._lose(link, { closeCode: ev.code, closeReason: ev.reason });
      };
    });
  }

  /** @private @param {Link} link */
  _wirePeer(link) {
    const pc = /** @type {RTCPeerConnection} */ (link.pc);
    pc.onconnectionstatechange = () => {
      if (link.closed) return;
      const st = pc.connectionState;
      if (st === 'connected') {
        if (link.iceTimer) { clearTimeout(link.iceTimer); link.iceTimer = null; }
        link.peerUp.resolve(undefined);
        if (link.restarting) {
          link.restarting = false;
          this._emit({ type: 'ice.restarted' });
        }
      } else if (st === 'disconnected') {
        if (!link.iceTimer) link.iceTimer = setTimeout(() => { link.iceTimer = null; if (pc.connectionState !== 'connected') this._restartIce(link); }, ICE_DISCONNECT_GRACE_MS);
      } else if (st === 'failed') {
        if (!link.peerUp.settled) this._lose(link, { error: new SencaError('connection_failed', 'ピア接続がつながらない（ICE・DTLS の失敗）') });
        else this._restartIce(link);
      }
    };
    pc.ondatachannel = (ev) => {
      // サーバーは DataChannel を開かない（クライアントが開く）が、来たら受ける
      if (ev.channel.label.startsWith('#')) this._wireDataChannel(link, ev.channel);
    };
  }

  /** @private @param {Link} link */
  async _restartIce(link) {
    if (link.closed || link.restarting || !link.peerUp.settled) return;
    if (!this._signalingOpen(link)) { this._lose(link, { error: new SencaError('connection_failed', 'ICE が切れ、シグナリングも閉じている') }); return; }
    link.restarting = true;
    link.iceRestart = true;
    this._emit({ type: 'ice.restarting' });
    try {
      await this._negotiate(link);
      const pc = /** @type {RTCPeerConnection} */ (link.pc);
      if (pc.connectionState !== 'connected') {
        await withTimeout(new Promise((resolve, reject) => {
          const f = () => {
            if (pc.connectionState === 'connected') { pc.removeEventListener('connectionstatechange', f); resolve(undefined); }
            if (pc.connectionState === 'closed') { pc.removeEventListener('connectionstatechange', f); reject(new SencaError('connection_failed', 'closed')); }
          };
          pc.addEventListener('connectionstatechange', f);
        }), ICE_RESTART_TIMEOUT_MS, 'ICE restart');
      }
    } catch (e) {
      link.restarting = false;
      this._lose(link, { error: new SencaError('connection_failed', `ICE restart で戻らなかった: ${errorText(e)}`, { cause: e }) });
    }
  }

  /** @private @param {Link} link @param {WireDataChannel} d */
  _openDataChannel(link, d) {
    const pc = /** @type {RTCPeerConnection} */ (link.pc);
    /** @type {RTCDataChannelInit} */
    const init = { ordered: d.ordered ?? true };
    if (d.max_retransmits !== undefined) init.maxRetransmits = d.max_retransmits;
    if (d.max_packet_life_time !== undefined) init.maxPacketLifeTime = d.max_packet_life_time;
    const dc = pc.createDataChannel(d.label, init);
    this._wireDataChannel(link, dc);
  }

  /** @private @param {Link} link @param {RTCDataChannel} dc */
  _wireDataChannel(link, dc) {
    dc.binaryType = 'arraybuffer';
    link.dcs.set(dc.label, dc);
    const label = dc.label;
    dc.onmessage = (ev) => {
      if (link.closed) return;
      const data = typeof ev.data === 'string' ? ev.data : new Uint8Array(/** @type {ArrayBuffer} */ (ev.data));
      this._messages.emit(new InboundMessage(label, data, null, 'datachannel', (id) => this._participants.get(id)));
    };
  }

  /** @private @param {Link} link */
  _openSignalingChannel(link) {
    const pc = /** @type {RTCPeerConnection} */ (link.pc);
    const dc = pc.createDataChannel(SIGNALING_LABEL, { ordered: true });
    link.sigDc = dc;
    dc.onmessage = (ev) => {
      if (typeof ev.data === 'string') this._onText(link, ev.data);
    };
    dc.onclose = () => {
      if (link.closed || !link.switched) return;
      // DataChannel が閉じたら WebSocket に戻る（それも閉じていれば失われた）
      link.switched = false;
      if (!link.ws || link.ws.readyState !== 1) this._lose(link, { error: new SencaError('connection_failed', 'シグナリングの DataChannel が閉じた') });
    };
  }

  /** @private @param {Link} link */
  _signalingOpen(link) {
    return (link.switched && link.sigDc?.readyState === 'open') || link.ws?.readyState === 1;
  }

  /** @private @param {Link} link @param {ClientMessage} m */
  _send(link, m) {
    if (link.closed) throw new SencaError('not_connected', 'つながっていない');
    const text = JSON.stringify(m);
    if (link.switched && link.sigDc?.readyState === 'open') {
      link.sigDc.send(text);
      return;
    }
    if (link.ws && link.ws.readyState === 1) {
      link.ws.send(text);
      return;
    }
    throw new SencaError('not_connected', 'シグナリングが閉じている');
  }

  // ---------------------------------------------------------------- 受けた message

  /** @private @param {Link} link @param {unknown} data */
  _onText(link, data) {
    if (link.closed || link !== this._link) return;
    link.lastSeen = Date.now();
    if (typeof data !== 'string') return;
    /** @type {ServerMessage} */
    let m;
    try {
      m = JSON.parse(data);
    } catch {
      this._o.logger?.warn?.('senca: JSON でない message', data.slice(0, 200));
      return;
    }
    // 受けた順に処理する（answer の処理と slots の処理が入れ替わらないように）
    link.chain = link.chain.then(() => this._handle(link, m)).catch((e) => {
      this._o.logger?.warn?.('senca: message の処理に失敗', m.type, e);
      this._emit({ type: 'error', error: toSencaError(e) });
    });
  }

  /** @private @param {Link} link @param {ServerMessage} m */
  _handle(link, m) {
    if (link.closed) return;
    switch (m.type) {
      case 'connected':
        link.connected = m;
        link.connectedD.resolve(m);
        break;
      case 'answer': {
        this._applySlots(link, m.slots);
        for (const p of m.published) link.published.set(p.mid, p.track_id);
        this._applyPublished(link, m.published);
        const pa = link.pendingAnswer;
        link.pendingAnswer = null;
        pa?.resolve(m);
        break;
      }
      case 'slots':
        this._applySlots(link, m.slots);
        break;
      case 'renegotiate':
        link.target.audio = Math.max(link.target.audio, m.audio);
        link.target.video = Math.max(link.target.video, m.video);
        this._negotiate(link).catch((e) => this._emit({ type: 'error', error: toSencaError(e) }));
        break;
      case 'notify':
        this._onNotify(link, m);
        break;
      case 'push':
        this._emit({ type: 'push', data: m.data });
        break;
      case 'message': {
        const data = m.data !== undefined ? m.data : fromBase64(m.data_base64 ?? '');
        this._messages.emit(new InboundMessage(m.label, data, m.from ?? null, 'signaling', (id) => this._participants.get(id)));
        break;
      }
      case 'ping':
        try {
          /** @type {ClientMessage} */
          const pong = { type: 'pong' };
          if (this._pongStats !== undefined) { pong.stats = this._pongStats; this._pongStats = undefined; }
          this._send(link, pong);
        } catch { /* 閉じかけ */ }
        break;
      case 'switched':
        link.switched = true;
        this._emit({ type: 'signaling.switched' });
        if (this._o.signaling?.closeWebSocket && link.ws) {
          link.wsClosedOnPurpose = true;
          link.ws.close(1000, 'switched');
        }
        break;
      case 'redirect':
        link.redirect = m.location;
        break;
      case 'error': {
        const err = new SencaError(m.code, m.message, { fatal: m.fatal, request: m.request });
        if (m.fatal) {
          link.fatal = err;
          // 参加の前の fatal は待っているものをすぐ失敗させる
          if (!link.connectedD.settled) link.connectedD.reject(err);
          break;
        }
        if (m.request === 'offer' && link.pendingAnswer) {
          const pa = link.pendingAnswer;
          link.pendingAnswer = null;
          pa.reject(err);
        } else if (m.request === 'layer' && link.layerRequests.length) {
          /** @type {{reject: (e: unknown) => void}} */ (link.layerRequests.shift()).reject(err);
        }
        this._emit({ type: 'error', error: err });
        break;
      }
      case 'disconnect':
        link.disconnectReason = m.reason;
        break;
      default:
        this._o.logger?.debug?.('senca: 知らない message', /** @type {{type: string}} */ (m).type);
    }
  }

  /** @private @param {Link} link @param {Extract<ServerMessage, {type: 'notify'}>} m */
  _onNotify(link, m) {
    switch (m.event) {
      case 'connection.created': {
        const p = new Participant(m.participant, this._port);
        this._participants.set(p.id, p);
        this._emit({ type: 'participant.joined', participant: p });
        break;
      }
      case 'connection.updated': {
        const p = this._participants.get(m.connection_id);
        if (p) {
          p.metadata = m.metadata;
          this._emit({ type: 'participant.updated', participant: p });
        } else if (m.connection_id === this.connectionId) {
          this.metadata = m.metadata;
        }
        break;
      }
      case 'connection.destroyed': {
        const p = this._participants.get(m.connection_id);
        if (p) {
          this._participants.delete(m.connection_id);
          this._emit({ type: 'participant.left', participant: p, reason: m.reason });
        }
        break;
      }
      case 'track.added': {
        const p = this._participants.get(m.track.connection_id);
        const info = { id: m.track.track_id, kind: m.track.kind, rids: Object.freeze([...(m.track.rids ?? [])]) };
        p?.tracks.set(info.id, info);
        this._emit({ type: 'track.published', participant: p, track: info });
        break;
      }
      case 'track.removed': {
        const p = this._participants.get(m.connection_id);
        p?.tracks.delete(m.track_id);
        this._emit({ type: 'track.unpublished', participant: p, trackId: m.track_id });
        break;
      }
      case 'spotlight.changed':
        this._focused = Object.freeze([...m.focused]);
        this._emit({ type: 'spotlight.changed', focused: this._focused });
        break;
      case 'layer.changed': {
        link.slotLayer.set(m.mid, m.rid);
        const t = link.slotTracks.get(m.mid);
        if (t && t.id === m.track_id) {
          t._onLayer(m.rid);
          this._emit({ type: 'layer.changed', track: t, rid: m.rid });
        }
        break;
      }
      case 'network.status':
        this.network = { level: m.level, bandwidthBps: m.bandwidth_bps, rttMs: m.rtt_ms };
        this._emit({ type: 'network', status: this.network });
        break;
      default:
        break;
    }
  }

  /** @private @param {Link} link @param {ConnectedMessage} c */
  _applyConnected(link, c) {
    this.connectionId = c.connection_id;
    this.clientId = c.client_id;
    this.bundleId = c.bundle_id;
    this.role = c.role;
    this.metadata = c.metadata ?? null;
    this.room = {
      id: c.room_id,
      videoCodec: c.video_codec,
      audioCodec: c.audio_codec,
      e2ee: c.e2ee,
      multistream: c.multistream,
      maxVideoBitrateKbps: c.max_video_bitrate_kbps,
      spotlight: c.spotlight ? { number: c.spotlight.number, focusLayer: c.spotlight.focus_rid, unfocused: c.spotlight.unfocused } : undefined,
    };
    // 参加者は入り直すたびに作り直す（connection_id が変わる）
    const before = new Map(this._participants);
    this._participants.clear();
    for (const w of c.participants) this._participants.set(w.connection_id, new Participant(w, this._port));
    if (link.epoch > 1) {
      for (const [id, p] of before) if (!this._participants.has(id)) this._emit({ type: 'participant.left', participant: p, reason: 'reconnected' });
      for (const [id, p] of this._participants) if (!before.has(id)) this._emit({ type: 'participant.joined', participant: p });
    }
  }

  // ---------------------------------------------------------------- 受信の枠

  /** @private @param {Link} link @param {WireSlot[]} slots */
  _applySlots(link, slots) {
    for (const s of slots) {
      const prev = link.slotTracks.get(s.mid);
      if (prev && (s.state === 'empty' || s.track_id !== prev.id)) {
        link.slotTracks.delete(s.mid);
        link.slotLayer.delete(s.mid);
        this._endTrack(prev, 'unsubscribed');
      }
      if (s.state === 'empty' || !s.track_id) continue;
      const cur = link.slotTracks.get(s.mid);
      if (cur) {
        const was = cur.state;
        cur._onPaused(s.state === 'paused');
        if (cur.state !== was) this._emit(cur.state === 'paused' ? { type: 'track.paused', track: cur } : { type: 'track.resumed', track: cur });
        continue;
      }
      const tr = link.pc?.getTransceivers().find((t) => t.mid === s.mid);
      if (!tr) continue;
      const pid = s.connection_id;
      const p = pid ? this._participants.get(pid) : undefined;
      const info = p?.tracks.get(s.track_id);
      const rt = new RemoteTrack(this._port, {
        id: s.track_id, kind: s.kind, participant: p, participantId: pid, clientId: s.client_id ?? p?.clientId, mid: s.mid,
        track: tr.receiver.track, rids: info?.rids ?? Object.freeze([]), paused: s.state === 'paused', MediaStream: this._env.MediaStream,
      });
      link.slotTracks.set(s.mid, rt);
      this._tracks.set(rt.id, rt);
      this._emit({ type: 'track.subscribed', track: rt });
      this._trackHub.emit(rt);
      const want = this._o.receiveLayer;
      if (want && rt.kind === 'video' && rt.rids.includes(want)) rt.setLayer(want).catch(() => {});
    }
  }

  /** @private @param {RemoteTrack} t @param {string} reason */
  _endTrack(t, reason) {
    if (this._tracks.get(t.id) === t) this._tracks.delete(t.id);
    t._end(reason);
    this._emit({ type: 'track.unsubscribed', track: t, reason });
  }

  /** @private @param {string} trackId @param {string | null} rid */
  async _requestLayer(trackId, rid) {
    const link = this._link;
    if (!link || link.closed || this.state !== 'connected') throw new SencaError('not_connected', 'つながっていない');
    /** @type {import('../internal/async.js').Deferred<void>} */
    const d = deferred();
    const entry = { reject: (/** @type {unknown} */ e) => d.reject(e) };
    link.layerRequests.push(entry);
    this._send(link, { type: 'layer', track_id: trackId, rid });
    // 誤りは非同期に届く（届かなければ受け付けた）。しばらく待って失敗が無ければ外す
    const t = setTimeout(() => {
      link.layerRequests = link.layerRequests.filter((x) => x !== entry);
      d.resolve(undefined);
    }, 300);
    try {
      await d.promise;
    } finally {
      clearTimeout(t);
    }
  }

  // ---------------------------------------------------------------- 交渉

  /**
   * offer を出して answer を待つ（1 つずつ）。交渉中に呼ばれたら、今の交渉の後にもう一度出す。
   * @private
   * @param {Link} link
   * @returns {Promise<void>}
   */
  _negotiate(link) {
    if (link.negotiating) {
      link.again = true;
      return link.negotiating;
    }
    const run = async () => {
      try {
        do {
          link.again = false;
          if (link.closed) throw new SencaError('not_connected', '接続が閉じた');
          const pc = /** @type {RTCPeerConnection} */ (link.pc);
          this._ensureRecv(link);
          this._applyCodecPreferences(link);
          const offer = await pc.createOffer(link.iceRestart ? { iceRestart: true } : {});
          link.iceRestart = false;
          await pc.setLocalDescription(offer);
          const sdp = pc.localDescription?.sdp ?? offer.sdp ?? '';
          link.pendingAnswer = deferred();
          const wait = link.pendingAnswer.promise;
          this._send(link, { type: 'offer', sdp });
          let answer;
          try {
            answer = await withTimeout(Promise.race([wait, link.lost.promise]), ANSWER_TIMEOUT_MS, 'answer を待つ');
          } catch (e) {
            if (e instanceof SencaError && e.code === 'timeout') this._lose(link, { error: e });
            else if (!link.closed && pc.signalingState === 'have-local-offer') await pc.setLocalDescription({ type: 'rollback' }).catch(() => {});
            throw e;
          }
          await pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
        } while (link.again && !link.closed);
      } finally {
        link.negotiating = null;
      }
    };
    link.negotiating = run();
    return link.negotiating;
  }

  /** @private @param {Link} link */
  _ensureRecv(link) {
    const pc = /** @type {RTCPeerConnection} */ (link.pc);
    for (const kind of /** @type {Kind[]} */ (['audio', 'video'])) {
      while (link.recv[kind].length < link.target[kind]) link.recv[kind].push(pc.addTransceiver(kind, { direction: 'recvonly' }));
    }
  }

  /** @private @param {Link} link */
  _applyCodecPreferences(link) {
    const room = this.room;
    const pc = link.pc;
    if (!room || !pc) return;
    for (const t of pc.getTransceivers()) {
      const kind = t.receiver.track?.kind;
      if (kind !== 'audio' && kind !== 'video') continue;
      if (typeof t.setCodecPreferences !== 'function') continue;
      const name = kind === 'video' ? room.videoCodec : room.audioCodec;
      const caps = this._env.RTCRtpReceiver?.getCapabilities(kind);
      const prefs = caps ? preferCodec(caps.codecs, name) : null;
      if (!prefs) {
        if (kind === 'video') this._o.logger?.warn?.(`senca: このブラウザーは ${name} を扱えない`);
        continue;
      }
      try { t.setCodecPreferences(prefs); } catch { /* 古いブラウザー */ }
    }
  }

  /** @private @param {MediaStream | MediaStreamTrack | MediaStreamTrack[]} source @param {PublishOptions} opts */
  _newPublication(source, opts) {
    /** @type {MediaStreamTrack[]} */
    let tracks;
    /** @type {MediaStream | undefined} */
    let stream;
    if (Array.isArray(source)) tracks = source;
    else if (source && typeof (/** @type {MediaStream} */ (source)).getTracks === 'function') {
      stream = /** @type {MediaStream} */ (source);
      tracks = stream.getTracks();
    } else if (source && typeof (/** @type {MediaStreamTrack} */ (source)).kind === 'string') tracks = [/** @type {MediaStreamTrack} */ (source)];
    else throw new SencaError('invalid_argument', 'MediaStream・MediaStreamTrack を渡す');
    if (!tracks.length) throw new SencaError('invalid_argument', 'トラックが無い');
    const kinds = tracks.map((t) => t.kind);
    if (kinds.filter((k) => k === 'video').length > 1 || kinds.filter((k) => k === 'audio').length > 1) {
      throw new SencaError('invalid_argument', '1 つの公開に映像・音声は 1 本ずつ（画面共有は別の publish か bundleId の別セッションで）');
    }
    if (opts.simulcast) simulcastLayers(opts.simulcast); // 形を先に確かめる
    const pub = new Publication(tracks, opts, stream, (p) => this._removePublication(p));
    this._publications.add(pub);
    return pub;
  }

  /** @private @param {Link} link @param {Publication} pub */
  _attachPublication(link, pub) {
    const pc = /** @type {RTCPeerConnection} */ (link.pc);
    const roomMax = this.room?.maxVideoBitrateKbps;
    for (const lt of pub.tracks) {
      // 再接続で既に付けたもの
      if (lt._transceiver && pc.getTransceivers().includes(lt._transceiver)) continue;
      /** @type {RTCRtpTransceiverInit} */
      const init = { direction: 'sendonly', streams: pub.stream ? [pub.stream] : [] };
      if (lt.kind === 'video') {
        const top = minDefined(pub.options.maxBitrateKbps, roomMax);
        const layers = simulcastLayers(pub.options.simulcast, top);
        if (layers) init.sendEncodings = encodingsFor(layers);
        else if (top) init.sendEncodings = [{ maxBitrate: top * 1000 }];
        lt.rids = layers ? Object.freeze(layers.map((l) => l.rid)) : Object.freeze([]);
      } else if (pub.options.audioBitrateKbps) {
        init.sendEncodings = [{ maxBitrate: pub.options.audioBitrateKbps * 1000 }];
      }
      lt._transceiver = pc.addTransceiver(lt.mediaStreamTrack, init);
      lt.id = undefined;
      lt.mid = undefined;
    }
  }

  /** @private @param {Link} link @param {Array<{mid: string, track_id: string, kind: Kind, rids?: string[]}>} published */
  _applyPublished(link, published) {
    for (const p of this._publications) {
      for (const lt of p.tracks) {
        const mid = lt._transceiver?.mid;
        if (!mid) continue;
        const w = published.find((x) => x.mid === mid);
        if (w) {
          lt.id = w.track_id;
          lt.mid = mid;
          if (w.rids?.length) lt.rids = Object.freeze([...w.rids]);
        }
      }
    }
  }

  /** @private @param {Publication} pub */
  async _removePublication(pub) {
    if (!this._publications.delete(pub)) return;
    pub.active = false;
    const link = this._link;
    for (const lt of pub.tracks) {
      const t = lt._transceiver;
      if (!t) continue;
      lt._transceiver = undefined;
      if (link?.pc?.getTransceivers().includes(t)) {
        try {
          t.direction = 'inactive';
          await t.sender.replaceTrack(null);
        } catch { /* 閉じかけ */ }
      }
    }
    if (link && !link.closed && this.state === 'connected') await this._negotiate(link);
  }

  // ---------------------------------------------------------------- 失われた接続と再接続

  /** @private @param {Link} link */
  _startWatchdog(link) {
    const interval = link.connected?.ping_interval_ms ?? 10000;
    link.watchdog = setInterval(() => {
      if (link.closed) return;
      if (Date.now() - link.lastSeen > interval * 2.5) this._lose(link, { error: new SencaError('timeout', 'サーバーから ping が来ない') });
    }, Math.max(1000, Math.round(interval / 2)));
    unref(link.watchdog);
  }

  /** @private */
  _startStatsReporting() {
    const r = this._o.reportStats;
    if (!r || this._statsTimer) return;
    const ms = typeof r === 'object' && r.intervalMs ? r.intervalMs : 10000;
    this._statsTimer = setInterval(async () => {
      if (this.state !== 'connected') return;
      try { this._pongStats = compactStats(await this.stats()); } catch { /* 次へ */ }
    }, ms);
    unref(this._statsTimer);
  }

  /**
   * 接続が失われた（理由を決めて、閉じるか入り直す）。
   * @private
   * @param {Link} link
   * @param {{closeCode?: number, closeReason?: string, error?: SencaError}} info
   */
  _lose(link, info) {
    if (link.closed || link !== this._link) return;
    const reason = link.disconnectReason;
    /** @type {SencaError} */
    let error;
    if (link.fatal) error = link.fatal;
    else if (reason === 'kicked') error = new SencaError('kicked', 'サーバーが切断した（kicked）', { fatal: true });
    else if (reason === 'room_deleted') error = new SencaError('room_deleted', '部屋が消された', { fatal: true });
    else if (info.error) error = info.error;
    else if (info.closeCode && info.closeCode >= 4000 && /** @type {Record<number, string>} */ (CODE_BY_CLOSE)[info.closeCode]) {
      error = new SencaError(/** @type {Record<number, string>} */ (CODE_BY_CLOSE)[info.closeCode], info.closeReason || `close ${info.closeCode}`, { fatal: true, closeCode: info.closeCode });
    } else error = new SencaError('connection_failed', `シグナリングが切れた（${reason ?? `close ${info.closeCode ?? '?'}`}）`, { closeCode: info.closeCode });
    if (!link.fatal && PERMANENT.has(error.code)) link.fatal = error;
    link.lost.reject(error);
    link.connectedD.reject(error);
    this._teardown(link);
    // 参加の途中なら _attempt が受けて投げる
    if (!link.joined || this.state === 'closed') return;
    if (link.redirect) {
      this._nextUrl = normalizeUrl(link.redirect);
      this._emit({ type: 'redirected', location: link.redirect });
      this._reconnect(undefined, true);
      return;
    }
    if (reason === 'normal' && !link.fatal) { this._finalize({ reason: 'normal', error: undefined }); return; }
    if ((reason && FINAL_REASONS.has(reason)) || PERMANENT.has(error.code)) {
      this._finalize({ reason: reason && reason !== 'error' ? reason : 'error', error });
      return;
    }
    this._reconnect(error, false);
  }

  /** @private @param {SencaError | undefined} error @param {boolean} immediate */
  async _reconnect(error, immediate) {
    const r = this._o.reconnect ?? true;
    if (r === false) { this._finalize({ reason: 'error', error }); return; }
    const policy = typeof r === 'object' ? r : {};
    const max = policy.maxAttempts ?? 10;
    this._setState('reconnecting');
    let last = error;
    for (let attempt = 0; attempt < max; attempt++) {
      if (this.state === 'closed') return;
      const delayMs = immediate && attempt === 0 ? 0 : backoffDelay(attempt, policy);
      this._emit({ type: 'reconnecting', attempt: attempt + 1, delayMs, error: last });
      try {
        await sleep(delayMs, this._close.signal);
        if (attempt > 0 || !immediate) this._urlIndex++;
        await this._join(this._close.signal, attempt + 1);
        this._setState('connected');
        this._emit({ type: 'reconnected', connectionId: this.connectionId });
        return;
      } catch (e) {
        if (isAbortError(e) || this._isClosed()) return;
        last = toSencaError(e);
        if (PERMANENT.has(last.code)) { this._finalize({ reason: 'error', error: last }); return; }
      }
    }
    this._finalize({ reason: 'connection_failed', error: new SencaError('connection_failed', `${max} 回入り直せなかった: ${last?.message ?? ''}`, { fatal: true, cause: last }) });
  }

  /** @private @param {Link} link */
  _teardown(link) {
    if (link.closed) return;
    link.closed = true;
    if (link.watchdog) clearInterval(link.watchdog);
    if (link.iceTimer) clearTimeout(link.iceTimer);
    link.pendingAnswer?.reject(new SencaError('not_connected', '接続が閉じた'));
    for (const r of link.layerRequests.splice(0)) r.reject(new SencaError('not_connected', '接続が閉じた'));
    link.connectedD.reject(link.fatal ?? new SencaError('connection_failed', '接続が閉じた'));
    link.lost.reject(link.fatal ?? new SencaError('connection_failed', '接続が閉じた'));
    for (const t of link.slotTracks.values()) this._endTrack(t, this.state === 'closed' ? 'closed' : 'reconnecting');
    link.slotTracks.clear();
    for (const p of this._publications) for (const lt of p.tracks) { lt._transceiver = undefined; lt.id = undefined; lt.mid = undefined; }
    try { link.sigDc?.close(); } catch { /* 無視 */ }
    for (const dc of link.dcs.values()) { try { dc.close(); } catch { /* 無視 */ } }
    try { link.pc?.close(); } catch { /* 無視 */ }
    if (link.ws && link.ws.readyState <= 1) {
      link.wsClosedOnPurpose = true;
      try { link.ws.close(1000); } catch { /* 無視 */ }
    }
  }

  /** @private @param {CloseInfo} info */
  _finalize(info) {
    if (this.state === 'closed') return;
    this._setState('closed');
    if (this._statsTimer) clearInterval(this._statsTimer);
    if (this._link) this._teardown(this._link);
    for (const t of [...this._tracks.values()]) this._endTrack(t, 'closed');
    this._emit({ type: 'closed', reason: info.reason, error: info.error });
    this._close.abort(new SencaError('closed', 'セッションが閉じた'));
    this._events.close();
    this._messages.close();
    this._trackHub.close();
    this._closedD.resolve(info);
  }

  /** @private @param {'connecting' | 'connected' | 'reconnecting' | 'closed'} s */
  _setState(s) {
    this.state = s;
    for (const f of this._stateWaiters.splice(0)) f();
  }

  /** @private @param {AbortSignal | undefined} signal @returns {Promise<Link>} */
  async _whenConnected(signal) {
    for (;;) {
      if (signal?.aborted) throw abortReason(signal);
      if (this.state === 'closed') throw new SencaError('closed', 'セッションは閉じている');
      if (this.state === 'connected' && this._link && !this._link.closed) return this._link;
      await abortable(new Promise((resolve) => this._stateWaiters.push(() => resolve(undefined))), signal);
    }
  }

  /** @private 状態を読み直す（await の後で TS が絞り込みを持ち越さないように） */
  _isClosed() {
    return this.state === 'closed';
  }

  /** @private */
  _assertOpen() {
    if (this.state === 'closed') throw new SencaError('closed', 'セッションは閉じている');
  }

  /** @private @param {SessionEvent} e */
  _emit(e) {
    this._events.emit(e);
  }
}

// `await using session = await Senca.connect(...)` で抜けたときに閉じる（対応する環境だけ）。
const asyncDispose = /** @type {any} */ (Symbol).asyncDispose;
if (asyncDispose) {
  Object.defineProperty(Session.prototype, asyncDispose, {
    value: function () { return /** @type {Session} */ (this).close(); },
    configurable: true,
    writable: true,
  });
}

// ------------------------------------------------------------------ 小道具

/** @param {string} u */
export function normalizeUrl(u) {
  let s = String(u).trim();
  if (s.startsWith('/') && typeof location !== 'undefined') s = new URL(s, location.href).href;
  if (s.startsWith('http://')) s = 'ws://' + s.slice(7);
  else if (s.startsWith('https://')) s = 'wss://' + s.slice(8);
  if (!/^wss?:\/\//.test(s)) throw new SencaError('invalid_argument', `シグナリングの URL は ws(s):// か http(s)://: ${u}`);
  const url = new URL(s);
  if (url.pathname === '/' || url.pathname === '') url.pathname = '/signaling';
  return url.href;
}

/** @param {DataChannelOption[] | undefined} list @returns {WireDataChannel[]} */
export function normalizeDataChannels(list) {
  /** @type {WireDataChannel[]} */
  const out = [];
  for (const d of list ?? []) {
    const o = typeof d === 'string' ? { label: d } : d;
    if (!o.label.startsWith('#') || o.label.length < 2 || o.label.length > 64) throw new SencaError('invalid_argument', `DataChannel のラベルは # で始まる 2〜64 文字: ${o.label}`);
    if (out.some((x) => x.label === o.label)) throw new SencaError('invalid_argument', `DataChannel のラベルが重複: ${o.label}`);
    if (o.maxRetransmits !== undefined && o.maxPacketLifeTime !== undefined) throw new SencaError('invalid_argument', 'maxRetransmits と maxPacketLifeTime は片方だけ');
    /** @type {WireDataChannel} */
    const w = { label: o.label, direction: o.direction ?? 'sendrecv', ordered: o.ordered ?? true };
    if (o.maxRetransmits !== undefined) w.max_retransmits = o.maxRetransmits;
    if (o.maxPacketLifeTime !== undefined) w.max_packet_life_time = o.maxPacketLifeTime;
    out.push(w);
  }
  return out;
}

/** Node.js では見張りのタイマーでプロセスを止めない。 @param {unknown} t */
function unref(t) {
  const u = /** @type {{unref?: () => void} | null} */ (t);
  if (u && typeof u.unref === 'function') u.unref();
}

/** @param {unknown} x */
function isMediaSource(x) {
  return Array.isArray(x) || (!!x && typeof x === 'object' && typeof (/** @type {any} */ (x).getTracks) === 'function');
}

/** @param {...(number | undefined)} xs */
function minDefined(...xs) {
  const v = xs.filter((x) => typeof x === 'number' && x > 0);
  return v.length ? Math.min(.../** @type {number[]} */ (v)) : undefined;
}

/** @param {unknown} data @returns {string | Uint8Array} */
function encodePayload(data) {
  if (typeof data === 'string') return data;
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  if (data === undefined) throw new SencaError('invalid_argument', 'data が要る');
  return JSON.stringify(data);
}

/** @param {RTCDataChannel} dc */
function drain(dc) {
  return new Promise((resolve) => {
    dc.bufferedAmountLowThreshold = DC_HIGH_WATER / 2;
    const f = () => { dc.removeEventListener('bufferedamountlow', f); resolve(undefined); };
    dc.addEventListener('bufferedamountlow', f);
  });
}

/** @param {Uint8Array} b */
export function toBase64(b) {
  let s = '';
  for (let i = 0; i < b.length; i += 0x8000) s += String.fromCharCode(...b.subarray(i, i + 0x8000));
  return btoa(s);
}

/** @param {string} s */
export function fromBase64(s) {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

/** @param {unknown} e @returns {SencaError} */
function toSencaError(e) {
  if (e instanceof SencaError) return e;
  return new SencaError('connection_failed', errorText(e), { cause: e });
}

/** @param {unknown} e */
function errorText(e) {
  return e instanceof Error ? e.message : String(e);
}
