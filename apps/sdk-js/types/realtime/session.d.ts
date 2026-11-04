/**
 * 部屋に入る。参加し、最初の交渉が済み、ピア接続がつながったら解決する。
 * @param {ConnectOptions} options
 * @returns {Promise<Session>}
 */
export function connect(options: ConnectOptions): Promise<Session>;
/** @param {string} u */
export function normalizeUrl(u: string): string;
/** @param {DataChannelOption[] | undefined} list @returns {WireDataChannel[]} */
export function normalizeDataChannels(list: DataChannelOption[] | undefined): WireDataChannel[];
/** @param {Uint8Array} b */
export function toBase64(b: Uint8Array): string;
/** @param {string} s */
export function fromBase64(s: string): Uint8Array<ArrayBuffer>;
export const SDK_NAME: "@senca/sdk";
export const SDK_VERSION: "0.0.2";
/** 部屋のセッション。`Senca.connect()` が返す。 */
export class Session {
    /** @internal @param {ConnectOptions} options */
    constructor(options: ConnectOptions);
    /** @private */
    private _o;
    /** @private */
    private _env;
    /** @private */
    private _urls;
    /** @private */
    private _urlIndex;
    /** @private @type {string | null} 次に使う URL（redirect の先） */
    private _nextUrl;
    /** @private */
    private _dataChannels;
    /** @private @type {Link | null} */
    private _link;
    /** @private */
    private _epoch;
    /** @private @type {Broadcaster<SessionEvent>} */
    private _events;
    /** @private @type {Broadcaster<InboundMessage>} */
    private _messages;
    /** @private @type {Broadcaster<RemoteTrack>} */
    private _trackHub;
    /** @private @type {Map<string, Participant>} */
    private _participants;
    /** @private @type {Map<string, RemoteTrack>} */
    private _tracks;
    /** @private @type {Set<Publication>} */
    private _publications;
    /** @private @type {import('../internal/async.js').Deferred<CloseInfo>} */
    private _closedD;
    /** @private */
    private _close;
    /** @private @type {Array<() => void>} */
    private _stateWaiters;
    /** @private @type {SessionStats | null} */
    private _lastStats;
    /** @private @type {unknown} */
    private _pongStats;
    /** @private @type {ReturnType<typeof setInterval> | null} */
    private _statsTimer;
    /** @private @type {readonly string[]} */
    private _focused;
    /** 今の状態。 @type {'connecting' | 'connected' | 'reconnecting' | 'closed'} */
    state: "connecting" | "connected" | "reconnecting" | "closed";
    /** 自分の connection_id（再接続で変わる）。 @type {string} */
    connectionId: string;
    /** @type {string | undefined} */
    clientId: string | undefined;
    /** @type {string} */
    bundleId: string;
    /** @type {Role} */
    role: Role;
    /** 他の参加者に見せる自分の metadata（トークン・認証 Webhook が決めたもの）。 @type {unknown} */
    metadata: unknown;
    /** @type {RoomInfo | null} */
    room: RoomInfo | null;
    /** 回線の状態（`network.status` を受けるまで null）。 @type {NetworkStatus | null} */
    network: NetworkStatus | null;
    /** セッションが閉じたら解決する（拒否はしない）。 */
    closed: Promise<CloseInfo>;
    /** @private @type {import('./objects.js').SessionPort} */
    private _port;
    /** 他の参加者（connection_id → Participant）。その場で変わる。 @returns {ReadonlyMap<string, Participant>} */
    get participants(): ReadonlyMap<string, Participant>;
    /** 今受けているトラック（トラック ID → RemoteTrack）。その場で変わる。 @returns {ReadonlyMap<string, RemoteTrack>} */
    get tracks(): ReadonlyMap<string, RemoteTrack>;
    /** 自分の公開。 @returns {readonly Publication[]} */
    get publications(): readonly Publication[];
    /** スポットライトで注目されている参加者の connection_id（新しく注目した順）。 */
    get spotlight(): {
        enabled: boolean;
        focused: readonly string[];
    };
    /**
     * 出来事の列。セッションが閉じると（`closed` を流してから）終わる。
     * @param {{types?: SessionEventType[], signal?: AbortSignal}} [opts]
     * @returns {AsyncIterableIterator<SessionEvent>}
     */
    events(opts?: {
        types?: SessionEventType[];
        signal?: AbortSignal;
    }): AsyncIterableIterator<SessionEvent>;
    /**
     * 受けるトラックの列。今受けているものを先に流し、その後に新しく受けたものを流す。
     * @param {{kind?: Kind, participant?: string | Participant, signal?: AbortSignal}} [opts]
     * @returns {AsyncIterableIterator<RemoteTrack>}
     */
    remoteTracks(opts?: {
        kind?: Kind;
        participant?: string | Participant;
        signal?: AbortSignal;
    }): AsyncIterableIterator<RemoteTrack>;
    /**
     * 届いた message の列（ラベルを省けば全部）。
     * @param {string | {label?: string, signal?: AbortSignal}} [label]
     * @param {{signal?: AbortSignal}} [opts]
     * @returns {AsyncIterableIterator<InboundMessage>}
     */
    messages(label?: string | {
        label?: string;
        signal?: AbortSignal;
    }, opts?: {
        signal?: AbortSignal;
    }): AsyncIterableIterator<InboundMessage>;
    /**
     * 最初に来た出来事（`type` が一致し `filter` が真のもの）を待つ。
     * @template {SessionEventType} T
     * @param {T} type
     * @param {{filter?: (e: Extract<SessionEvent, {type: T}>) => boolean, signal?: AbortSignal, timeoutMs?: number}} [opts]
     * @returns {Promise<Extract<SessionEvent, {type: T}>>}
     */
    waitFor<T extends SessionEventType>(type: T, opts?: {
        filter?: (e: Extract<SessionEvent, {
            type: T;
        }>) => boolean;
        signal?: AbortSignal;
        timeoutMs?: number;
    }): Promise<Extract<SessionEvent, {
        type: T;
    }>>;
    /**
     * カメラ・マイク・画面などを公開する。交渉が済み、サーバーがトラック ID を付けたら解決する。
     * @param {MediaStream | MediaStreamTrack | MediaStreamTrack[]} source
     * @param {PublishOptions} [opts]
     * @returns {Promise<Publication>}
     */
    publish(source: MediaStream | MediaStreamTrack | MediaStreamTrack[], opts?: PublishOptions): Promise<Publication>;
    /**
     * アプリの message を送る。`to` を省けば、そのラベルを開いている他の全員へ（DataChannel、開いていなければシグナリング）。
     * `to` を書けばその参加者だけへ（シグナリング経由。受け手には `from` が付く）。
     * 文字列は text、Uint8Array・ArrayBuffer は binary、それ以外は JSON にして text で送る。64 KiB まで。
     * @param {string} label `#` で始まり、`dataChannels` で送る向きに宣言したもの
     * @param {string | ArrayBuffer | ArrayBufferView | object} data
     * @param {{to?: string | Participant | Array<string | Participant>, signal?: AbortSignal}} [opts]
     * @returns {Promise<void>}
     */
    send(label: string, data: string | ArrayBuffer | ArrayBufferView | object, opts?: {
        to?: string | Participant | Array<string | Participant>;
        signal?: AbortSignal;
    }): Promise<void>;
    /**
     * 統計（getStats の要約と、そのままの報告）。
     * @returns {Promise<SessionStats>}
     */
    stats(): Promise<SessionStats>;
    /**
     * 閉じる（サーバーへ `disconnect` を送る）。何度呼んでもよい。
     * @param {string} [reason]
     * @returns {Promise<CloseInfo>}
     */
    close(reason?: string): Promise<CloseInfo>;
    /** @internal */
    _start(): Promise<void>;
    /** @private 次に使う URL */
    private _pickUrl;
    /**
     * 1 回の参加（redirect は追う）。
     * @private
     * @param {AbortSignal | undefined} signal
     * @param {number} attempt
     */
    private _join;
    /**
     * @private
     * @param {string} url
     * @param {AbortSignal | undefined} signal
     * @param {number} attempt
     * @returns {Promise<string | null>} redirect の先（無ければ null）
     */
    private _attempt;
    /** @private @param {string | undefined} token @returns {ConnectMessage} */
    private _connectMessage;
    /** @private @param {Link} link */
    private _openWebSocket;
    /** @private @param {Link} link */
    private _wirePeer;
    /** @private @param {Link} link */
    private _restartIce;
    /** @private @param {Link} link @param {WireDataChannel} d */
    private _openDataChannel;
    /** @private @param {Link} link @param {RTCDataChannel} dc */
    private _wireDataChannel;
    /** @private @param {Link} link */
    private _openSignalingChannel;
    /** @private @param {Link} link */
    private _signalingOpen;
    /** @private @param {Link} link @param {ClientMessage} m */
    private _send;
    /** @private @param {Link} link @param {unknown} data */
    private _onText;
    /** @private @param {Link} link @param {ServerMessage} m */
    private _handle;
    /** @private @param {Link} link @param {Extract<ServerMessage, {type: 'notify'}>} m */
    private _onNotify;
    /** @private @param {Link} link @param {ConnectedMessage} c */
    private _applyConnected;
    /** @private @param {Link} link @param {WireSlot[]} slots */
    private _applySlots;
    /** @private @param {RemoteTrack} t @param {string} reason */
    private _endTrack;
    /** @private @param {string} trackId @param {string | null} rid */
    private _requestLayer;
    /**
     * offer を出して answer を待つ（1 つずつ）。交渉中に呼ばれたら、今の交渉の後にもう一度出す。
     * @private
     * @param {Link} link
     * @returns {Promise<void>}
     */
    private _negotiate;
    /** @private @param {Link} link */
    private _ensureRecv;
    /** @private @param {Link} link */
    private _applyCodecPreferences;
    /** @private @param {MediaStream | MediaStreamTrack | MediaStreamTrack[]} source @param {PublishOptions} opts */
    private _newPublication;
    /** @private @param {Link} link @param {Publication} pub */
    private _attachPublication;
    /** @private @param {Link} link @param {Array<{mid: string, track_id: string, kind: Kind, rids?: string[]}>} published */
    private _applyPublished;
    /** @private @param {Publication} pub */
    private _removePublication;
    /** @private @param {Link} link */
    private _startWatchdog;
    /** @private */
    private _startStatsReporting;
    /**
     * 接続が失われた（理由を決めて、閉じるか入り直す）。
     * @private
     * @param {Link} link
     * @param {{closeCode?: number, closeReason?: string, error?: SencaError}} info
     */
    private _lose;
    /** @private @param {SencaError | undefined} error @param {boolean} immediate */
    private _reconnect;
    /** @private @param {Link} link */
    private _teardown;
    /** @private @param {CloseInfo} info */
    private _finalize;
    /** @private @param {'connecting' | 'connected' | 'reconnecting' | 'closed'} s */
    private _setState;
    /** @private @param {AbortSignal | undefined} signal @returns {Promise<Link>} */
    private _whenConnected;
    /** @private 状態を読み直す（await の後で TS が絞り込みを持ち越さないように） */
    private _isClosed;
    /** @private */
    private _assertOpen;
    /** @private @param {SessionEvent} e */
    private _emit;
}
export type Role = import("./protocol.js").Role;
export type Kind = import("./protocol.js").Kind;
export type ServerMessage = import("./protocol.js").ServerMessage;
export type ClientMessage = import("./protocol.js").ClientMessage;
export type ConnectMessage = import("./protocol.js").ConnectMessage;
export type ConnectedMessage = import("./protocol.js").ConnectedMessage;
export type WireSlot = import("./protocol.js").WireSlot;
export type WireDataChannel = import("./protocol.js").WireDataChannel;
export type SessionStats = import("./media.js").SessionStats;
export type PublishOptions = import("./objects.js").PublishOptions;
/**
 * DataChannel の宣言（`'#chat'` だけでもよい）。
 */
export type DataChannelOption = string | {
    label: string;
    direction?: "sendrecv" | "sendonly" | "recvonly";
    ordered?: boolean;
    maxRetransmits?: number;
    maxPacketLifeTime?: number;
};
/**
 * 再接続の方針（`false` で再接続しない）。
 */
export type ReconnectOptions = {
    /**
     * あきらめるまでの回数（既定 10）
     */
    maxAttempts?: number | undefined;
    /**
     * 最初の待ち（既定 500）
     */
    initialDelayMs?: number | undefined;
    /**
     * 待ちの上限（既定 10000）
     */
    maxDelayMs?: number | undefined;
    /**
     * 倍率（既定 2）
     */
    factor?: number | undefined;
    /**
     * ゆらぎ（既定 0.2）
     */
    jitter?: number | undefined;
};
/**
 * WebRTC・WebSocket の実装（既定はブラウザーの globalThis。試験や Node.js で差し替える）。
 */
export type Environment = {
    WebSocket?: {
        new (url: string | URL, protocols?: string | string[]): WebSocket;
        prototype: WebSocket;
        readonly CONNECTING: 0;
        readonly OPEN: 1;
        readonly CLOSING: 2;
        readonly CLOSED: 3;
    } | undefined;
    RTCPeerConnection?: {
        new (configuration?: RTCConfiguration): RTCPeerConnection;
        prototype: RTCPeerConnection;
        generateCertificate(keygenAlgorithm: AlgorithmIdentifier): Promise<RTCCertificate>;
    } | undefined;
    MediaStream?: {
        new (): MediaStream;
        new (stream: MediaStream): MediaStream;
        new (tracks: MediaStreamTrack[]): MediaStream;
        prototype: MediaStream;
    } | undefined;
    RTCRtpSender?: {
        getCapabilities(kind: string): RTCRtpCapabilities | null;
    } | undefined;
    RTCRtpReceiver?: {
        getCapabilities(kind: string): RTCRtpCapabilities | null;
    } | undefined;
};
/**
 * `Senca.connect()` の指定。
 */
export type ConnectOptions = {
    /**
     * シグナリングの URL（`wss://node.example/signaling`。`https://node.example` なら `/signaling` を足す）。
     * 複数なら再接続のたびに順に替える
     */
    url: string | string[];
    /**
     * 部屋の ID
     */
    room: string;
    /**
     * 参加トークン。関数なら参加・再接続のたびに呼ぶ（期限の短いトークンを取り直す）
     */
    token?: string | ((ctx: {
        room: string;
        attempt: number;
    }) => string | Promise<string>) | undefined;
    /**
     * 既定 `sendrecv`
     */
    role?: import("./protocol.js").Role | undefined;
    clientId?: string | undefined;
    /**
     * 同じ値の接続どうしは互いのトラックを受けない
     */
    bundleId?: string | undefined;
    /**
     * 認証 Webhook へだけ渡す（他の参加者に見せる metadata はトークン・Webhook が決める）
     */
    metadata?: unknown;
    /**
     * 既定 true（false で種類ごとに 1 本だけ受ける）
     */
    multistream?: boolean | undefined;
    /**
     * 受け手としてのスポットライト
     */
    spotlight?: {
        number: number;
        focusLayer?: string;
        unfocused?: "lowest" | "off";
    } | undefined;
    /**
     * 受ける映像の最初の層（`'r0'` など。サイマルキャストのトラックを受けたら自動で選ぶ）
     */
    receiveLayer?: string | undefined;
    /**
     * 参加と同時に公開する（最初の offer に入れるので再交渉が 1 回少ない）
     */
    publish?: MediaStream | MediaStreamTrack[] | (import("./objects.js").PublishOptions & {
        stream: MediaStream | MediaStreamTrack[];
    }) | undefined;
    /**
     * 映像の希望（`false` で送らない）
     */
    video?: false | {
        codec?: import("./protocol.js").VideoCodec;
        bitrateKbps?: number;
    } | undefined;
    audio?: false | {
        codec?: "opus";
        bitrateKbps?: number;
    } | undefined;
    /**
     * 使う DataChannel（`#` で始まるラベル）
     */
    dataChannels?: DataChannelOption[] | undefined;
    /**
     * シグナリングを DataChannel へ切り替える（§6）
     */
    signaling?: {
        dataChannel?: boolean;
        closeWebSocket?: boolean;
    } | undefined;
    /**
     * E2EE の部屋に入る（部屋の設定と同じでなければ `e2ee_mismatch`。暗号化（SFrame）は利用者が Encoded Transform で行う）
     */
    e2ee?: boolean | undefined;
    /**
     * この接続の購読を絞る
     */
    forwardingFilter?: import("./protocol.js").ForwardingFilter | undefined;
    iceServers?: RTCIceServer[] | undefined;
    /**
     * RTCPeerConnection の設定（bundlePolicy は max-bundle に固定）
     */
    rtcConfiguration?: RTCConfiguration | undefined;
    /**
     * 既定 true
     */
    reconnect?: boolean | ReconnectOptions | undefined;
    /**
     * 1 回の参加の時間切れ（既定 20000。ピア接続がつながるまで）
     */
    connectTimeoutMs?: number | undefined;
    /**
     * getStats の要約を pong でサーバーへ送る（REST の統計に出る）
     */
    reportStats?: boolean | {
        intervalMs?: number;
    } | undefined;
    /**
     * 参加中に取り消すと AbortError。参加の後に取り消すとセッションを閉じる
     */
    signal?: AbortSignal | undefined;
    environment?: Environment | undefined;
    logger?: {
        debug?: (...a: unknown[]) => void;
        warn?: (...a: unknown[]) => void;
    } | undefined;
};
/**
 * 部屋の情報（`connected`）。
 */
export type RoomInfo = {
    id: string;
    videoCodec: import("./protocol.js").VideoCodec;
    audioCodec: "opus";
    e2ee: boolean;
    multistream: boolean;
    maxVideoBitrateKbps: number | undefined;
    spotlight: {
        number: number;
        focusLayer?: string;
        unfocused?: "lowest" | "off";
    } | undefined;
};
/**
 * 回線の状態（`network.status`、0 よい〜3 悪い）。
 */
export type NetworkStatus = {
    level: number;
    bandwidthBps: number;
    rttMs: number;
};
/**
 * 閉じた理由（`session.closed`）。
 */
export type CloseInfo = {
    /**
     * `normal`・`kicked`・`room_deleted`・`error`・`connection_failed`・`aborted` など
     */
    reason: string;
    /**
     * 誤りで閉じたとき
     */
    error: SencaError | undefined;
};
/**
 * セッションの出来事（`session.events()`）。`type` で見分ける。
 */
export type SessionEvent = {
    type: "participant.joined";
    participant: Participant;
} | {
    type: "participant.left";
    participant: Participant;
    reason: string;
} | {
    type: "participant.updated";
    participant: Participant;
} | {
    type: "track.published";
    participant: Participant | undefined;
    track: import("./objects.js").PublishedTrackInfo;
} | {
    type: "track.unpublished";
    participant: Participant | undefined;
    trackId: string;
} | {
    type: "track.subscribed";
    track: RemoteTrack;
} | {
    type: "track.unsubscribed";
    track: RemoteTrack;
    reason: string;
} | {
    type: "track.paused";
    track: RemoteTrack;
} | {
    type: "track.resumed";
    track: RemoteTrack;
} | {
    type: "layer.changed";
    track: RemoteTrack;
    rid: string | null;
} | {
    type: "spotlight.changed";
    focused: readonly string[];
} | {
    type: "network";
    status: NetworkStatus;
} | {
    type: "push";
    data: unknown;
} | {
    type: "signaling.switched";
} | {
    type: "ice.restarting";
} | {
    type: "ice.restarted";
} | {
    type: "reconnecting";
    attempt: number;
    delayMs: number;
    error: SencaError | undefined;
} | {
    type: "reconnected";
    connectionId: string;
} | {
    type: "redirected";
    location: string;
} | {
    type: "error";
    error: SencaError;
} | {
    type: "closed";
    reason: string;
    error: SencaError | undefined;
};
export type SessionEventType = SessionEvent["type"];
import { Participant } from './objects.js';
import { RemoteTrack } from './objects.js';
import { Publication } from './objects.js';
import { InboundMessage } from './objects.js';
import { SencaError } from '../errors.js';
