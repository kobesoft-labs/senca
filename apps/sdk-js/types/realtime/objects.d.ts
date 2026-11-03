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
    constructor(w: WireParticipant, port: SessionPort);
    /** @type {SessionPort} */
    _port: SessionPort;
    /** connection_id（接続ごとに変わる）。 */
    id: string;
    /** client_id（トークン・認証 Webhook が決めたもの）。 */
    clientId: string | undefined;
    bundleId: string;
    /** @type {Role} */
    role: Role;
    /** 参加トークン・認証 Webhook・REST が決めた、他の参加者に見せる metadata。 */
    /** @type {unknown} */
    metadata: unknown;
    /** 別のノードの参加者ならそのノード（グリッドのカスケード）。 */
    node: string | undefined;
    /** @type {Map<string, PublishedTrackInfo>} 公開しているトラック（受けているかどうかに依らない）。 */
    tracks: Map<string, PublishedTrackInfo>;
    /** 別のノードにいる参加者か。 */
    get remote(): boolean;
    /** スポットライトで注目されているか。 */
    get focused(): boolean;
    /** この参加者から受けているトラック。 */
    get subscriptions(): RemoteTrack[];
    toJSON(): {
        id: string;
        clientId: string | undefined;
        bundleId: string;
        role: import("./protocol.js").Role;
        metadata: unknown;
        node: string | undefined;
        tracks: PublishedTrackInfo[];
    };
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
    constructor(port: SessionPort, p: {
        id: string;
        kind: Kind;
        participant: Participant | undefined;
        participantId: string | undefined;
        clientId: string | undefined;
        mid: string;
        track: MediaStreamTrack;
        rids: readonly string[];
        paused: boolean;
        MediaStream?: typeof MediaStream;
    });
    /** @type {SessionPort} */
    _port: SessionPort;
    /** トラック ID（`{connection_id}.{mid}`）。 */
    id: string;
    /** @type {Kind} */
    kind: Kind;
    /** 送り手（`connected`・`connection.created` で知らない送り手なら undefined）。 */
    participant: Participant | undefined;
    /** 送り手の connection_id。 */
    participantId: string | undefined;
    clientId: string | undefined;
    /** 受信の枠（m 行）の mid。 */
    mid: string;
    /** ブラウザーの受信のトラック。 */
    mediaStreamTrack: MediaStreamTrack;
    /** `mediaStreamTrack` だけを入れた MediaStream（`<video>.srcObject` にそのまま渡せる）。 */
    stream: MediaStream | undefined;
    /** サイマルキャストの層（低い順）。 */
    rids: readonly string[];
    /** サーバーが今配っている層（`layer.changed` で更新。まだ知らなければ null）。 @type {string | null} */
    layer: string | null;
    /** 選んだ層（`setLayer`、既定 `'auto'`）。 @type {string} */
    requestedLayer: string;
    /** `active`（届いている）・`paused`（スポットライトの外などで止めている）・`ended`。 @type {'active' | 'paused' | 'ended'} */
    state: "active" | "paused" | "ended";
    /** @type {Broadcaster<RemoteTrackEvent>} */
    _hub: Broadcaster<RemoteTrackEvent>;
    /** @type {import('../internal/async.js').Deferred<{reason: string}>} */
    _ended: import("../internal/async.js").Deferred<{
        reason: string;
    }>;
    /** 終わったとき（理由つき）に解決する。 */
    ended: Promise<{
        reason: string;
    }>;
    /** @type {Array<{rid: string, done: () => void}>} */
    _layerWaiters: Array<{
        rid: string;
        done: () => void;
    }>;
    /**
     * 受ける層を選ぶ。`'auto'` はサーバーが帯域で選ぶ。層を指定したときは、サーバーがその層へ切り替えた（`layer.changed`）ときに
     * 解決する（キーフレームを待つので少しかかる）。送り手がその層を送っていなければ `timeoutMs`（既定 5000）で `timeout`。
     * @param {string} layer `'r0'` などの rid か `'auto'`
     * @param {{signal?: AbortSignal, timeoutMs?: number}} [opts]
     * @returns {Promise<void>}
     */
    setLayer(layer: string, opts?: {
        signal?: AbortSignal;
        timeoutMs?: number;
    }): Promise<void>;
    /**
     * このトラックの出来事（層の変化・一時停止・終わり）。終わると反復も終わる。
     * @param {{signal?: AbortSignal}} [opts]
     * @returns {AsyncIterableIterator<RemoteTrackEvent>}
     */
    events(opts?: {
        signal?: AbortSignal;
    }): AsyncIterableIterator<RemoteTrackEvent>;
    /** 受信の統計（終わっていれば undefined）。 */
    stats(): Promise<import("./media.js").InboundStats | undefined>;
    /**
     * `<video>`・`<audio>` に付ける（`srcObject` を設定して返す）。
     * @template {HTMLMediaElement} E
     * @param {E} el
     * @returns {E}
     */
    attach<E extends HTMLMediaElement>(el: E): E;
    /** @param {string | null} rid @internal */
    _onLayer(rid: string | null): void;
    /** @param {boolean} paused @internal */
    _onPaused(paused: boolean): void;
    /** @param {string} reason @internal */
    _end(reason: string): void;
    toJSON(): {
        id: string;
        kind: import("./protocol.js").Kind;
        participantId: string | undefined;
        clientId: string | undefined;
        mid: string;
        rids: readonly string[];
        layer: string | null;
        state: "active" | "paused" | "ended";
    };
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
    constructor(track: MediaStreamTrack, pub: Publication);
    /** @type {Kind} */
    kind: Kind;
    /** 送っている MediaStreamTrack（`replace()` で替わる）。 */
    mediaStreamTrack: MediaStreamTrack;
    /** サーバーが付けたトラック ID（answer の `published`。再接続で変わる）。 @type {string | undefined} */
    id: string | undefined;
    /** 送る m 行の mid。 @type {string | undefined} */
    mid: string | undefined;
    /** サイマルキャストの層（低い順）。 @type {readonly string[]} */
    rids: readonly string[];
    /** @type {Publication} */
    publication: Publication;
    /** @type {RTCRtpTransceiver | undefined} @internal */
    _transceiver: RTCRtpTransceiver | undefined;
    /**
     * 別の MediaStreamTrack に差し替える（再交渉しない。カメラの切り替えなど）。
     * @param {MediaStreamTrack | null} track
     */
    replace(track: MediaStreamTrack | null): Promise<void>;
    set enabled(v: boolean);
    /** 送るのを止める・再開する（MediaStreamTrack.enabled。止めている間は黒・無音を送る）。 */
    get enabled(): boolean;
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
    constructor(tracks: MediaStreamTrack[], opts: PublishOptions, stream: MediaStream | undefined, unpublish: (p: Publication) => Promise<void>);
    /** @type {LocalTrack[]} */
    tracks: LocalTrack[];
    options: PublishOptions;
    stream: MediaStream | undefined;
    /** @internal */
    _unpublish: (p: Publication) => Promise<void>;
    active: boolean;
    /** @param {Kind} kind */
    track(kind: Kind): LocalTrack | undefined;
    get audio(): LocalTrack | undefined;
    get video(): LocalTrack | undefined;
    /** 公開をやめる（送る m 行を止めて再交渉する。他の参加者には track.unpublished が届く）。 */
    unpublish(): Promise<void>;
    /**
     * 映像の上限を変える（再交渉しない。サイマルキャストなら一番上の層を、他の層は比で縮める）。
     * @param {number} kbps
     */
    setMaxBitrate(kbps: number): Promise<void>;
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
    constructor(label: string, data: string | Uint8Array, from: string | null, via: "datachannel" | "signaling", lookup: (id: string) => Participant | undefined);
    label: string;
    /** 本文（text は string、binary は Uint8Array）。 */
    data: string | Uint8Array<ArrayBufferLike>;
    /**
     * 送り手の connection_id。DataChannel で部屋全体に届いたもの（送り手を運ばない）と API からのものは null。
     * 送り手を知りたいなら `send(label, data, {to})`（シグナリング経由、`from` が付く）を使うか本文に入れる。
     */
    from: string | null;
    /** 届いた経路。 */
    via: "signaling" | "datachannel";
    /** @internal */
    _lookup: (id: string) => Participant | undefined;
    receivedAt: number;
    /** 送り手（分からなければ undefined）。 */
    get sender(): Participant | undefined;
    get binary(): boolean;
    /** 本文を文字列で。 */
    text(): string;
    /** 本文を JSON として読む（`send()` にオブジェクトを渡すと JSON で送る）。 */
    json(): any;
}
export type Role = import("./protocol.js").Role;
export type Kind = import("./protocol.js").Kind;
export type WireParticipant = import("./protocol.js").WireParticipant;
export type InboundStats = import("./media.js").InboundStats;
/**
 * 参加者が公開しているトラック（受けているかどうかに依らない）。
 */
export type PublishedTrackInfo = {
    /**
     * トラック ID
     */
    id: string;
    kind: Kind;
    /**
     * サイマルキャストの層（低い順、無ければ空）
     */
    rids: readonly string[];
};
/**
 * セッションの中から RemoteTrack・Participant が使う口（内部）。
 */
export type SessionPort = {
    requestLayer: (trackId: string, rid: string | null) => Promise<void>;
    inboundStats: (mid: string, trackId: string) => Promise<InboundStats | undefined>;
    focused: () => readonly string[];
    subscriptionsOf: (participantId: string) => RemoteTrack[];
};
/**
 * RemoteTrack の出来事。
 */
export type RemoteTrackEvent = {
    type: "layer";
    rid: string | null;
} | {
    type: "paused";
} | {
    type: "resumed";
} | {
    type: "ended";
    reason: string;
};
/**
 * 公開の指定。
 */
export type PublishOptions = {
    /**
     * サイマルキャスト（`true` で既定の 3 層 r0・r1・r2）
     */
    simulcast?: boolean | import("./media.js").SimulcastLayer[] | {
        layers?: import("./media.js").SimulcastLayer[];
    } | undefined;
    /**
     * 映像の上限（サイマルキャストなら一番上の層）。部屋の上限があればそれ以下
     */
    maxBitrateKbps?: number | undefined;
    /**
     * 音声の上限
     */
    audioBitrateKbps?: number | undefined;
    /**
     * 部屋のコーデックと違えば `unsupported_codec`（部屋が決める。確かめるために書ける）
     */
    codec?: import("./protocol.js").VideoCodec | undefined;
    signal?: AbortSignal | undefined;
};
import { Broadcaster } from '../internal/broadcast.js';
