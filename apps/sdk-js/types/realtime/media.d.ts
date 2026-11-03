/**
 * 公開の指定からサイマルキャストの層（低い順）を決める。無ければ null。
 * @param {boolean | SimulcastLayer[] | {layers?: SimulcastLayer[]} | undefined} sc
 * @param {number} [topKbps] 一番上の層の上限（部屋の max_video_bitrate_kbps・公開の bitrate）
 * @returns {SimulcastLayer[] | null}
 */
export function simulcastLayers(sc: boolean | SimulcastLayer[] | {
    layers?: SimulcastLayer[];
} | undefined, topKbps?: number): SimulcastLayer[] | null;
/**
 * 層 → RTCRtpEncodingParameters（低い順。SDP の a=simulcast もこの順になる）。
 * @param {SimulcastLayer[]} layers
 * @returns {RTCRtpEncodingParameters[]}
 */
export function encodingsFor(layers: SimulcastLayer[]): RTCRtpEncodingParameters[];
/**
 * 部屋のコーデックだけを残した優先順（再送の rtx・FEC は残す）。H.264 は packetization-mode=1 の baseline を先に。
 * 見つからなければ null（そのブラウザーでは送受できない）。
 * @param {ReadonlyArray<RTCRtpCodec>} codecs `RTCRtpReceiver.getCapabilities(kind).codecs`
 * @param {string} name `vp8`・`vp9`・`h264`・`av1`・`opus`
 * @returns {RTCRtpCodec[] | null}
 */
export function preferCodec(codecs: ReadonlyArray<RTCRtpCodec>, name: string): RTCRtpCodec[] | null;
/**
 * 受信の統計（1 トラック）。
 * @typedef {object} InboundStats
 * @property {string} trackId SENCA のトラック ID
 * @property {string | undefined} participantId 送り手の connection_id
 * @property {string | undefined} clientId
 * @property {'audio' | 'video'} kind
 * @property {string} mid
 * @property {string | null} layer サーバーが今配っている層（`layer.changed`）
 * @property {number} bytesReceived
 * @property {number} packetsReceived
 * @property {number} packetsLost
 * @property {number | undefined} jitterMs
 * @property {number | undefined} framesDecoded
 * @property {number | undefined} framesDropped
 * @property {number | undefined} framesPerSecond
 * @property {number | undefined} frameWidth
 * @property {number | undefined} frameHeight
 * @property {number | undefined} audioLevel
 * @property {number | undefined} bitrateBps 前回の stats() からの受信の速さ
 */
/**
 * 送信の統計（1 つの層・トラック）。
 * @typedef {object} OutboundStats
 * @property {string | undefined} trackId SENCA のトラック ID（answer の `published`）
 * @property {'audio' | 'video'} kind
 * @property {string | undefined} rid
 * @property {string} mid
 * @property {number} bytesSent
 * @property {number} packetsSent
 * @property {number | undefined} framesSent
 * @property {number | undefined} framesPerSecond
 * @property {number | undefined} frameWidth
 * @property {number | undefined} frameHeight
 * @property {number | undefined} targetBitrateBps
 * @property {string | undefined} qualityLimitationReason
 * @property {boolean | undefined} active
 * @property {number | undefined} bitrateBps 前回の stats() からの送信の速さ
 */
/**
 * セッションの統計（`session.stats()`）。
 * @typedef {object} SessionStats
 * @property {number} at 測った時刻（UNIX ミリ秒）
 * @property {{state: string, rttMs: number | undefined, availableOutgoingBitrateBps: number | undefined,
 *   availableIncomingBitrateBps: number | undefined, bytesSent: number, bytesReceived: number}} transport
 * @property {OutboundStats[]} outbound
 * @property {InboundStats[]} inbound
 * @property {Array<{label: string, state: string, messagesSent: number, messagesReceived: number, bytesSent: number, bytesReceived: number}>} dataChannels
 * @property {RTCStatsReport} raw ブラウザーの getStats() そのもの
 */
/**
 * getStats() の報告を要約する。
 * @param {RTCStatsReport} report
 * @param {{state: string, slotOf: (mid: string) => ({trackId: string, participantId?: string, clientId?: string, layer: string | null} | undefined),
 *   publishedOf: (mid: string) => (string | undefined), previous?: SessionStats | null}} ctx
 * @returns {SessionStats}
 */
export function summarizeStats(report: RTCStatsReport, ctx: {
    state: string;
    slotOf: (mid: string) => ({
        trackId: string;
        participantId?: string;
        clientId?: string;
        layer: string | null;
    } | undefined);
    publishedOf: (mid: string) => (string | undefined);
    previous?: SessionStats | null;
}): SessionStats;
/**
 * サーバーへ `pong.stats` で送る小さな要約（64 KiB に収める）。
 * @param {SessionStats} s
 */
export function compactStats(s: SessionStats): {
    at: number;
    rtt_ms: number | undefined;
    available_outgoing_bitrate_bps: number | undefined;
    outbound: {
        track_id: string | undefined;
        rid: string | undefined;
        kind: "audio" | "video";
        bitrate_bps: number | undefined;
        fps: number | undefined;
        width: number | undefined;
        height: number | undefined;
        limit: string | undefined;
    }[];
    inbound: {
        track_id: string;
        kind: "audio" | "video";
        layer: string | null;
        bitrate_bps: number | undefined;
        lost: number;
        fps: number | undefined;
        width: number | undefined;
        height: number | undefined;
        decoded: number | undefined;
    }[];
};
/**
 * サイマルキャストの 1 層。
 * @typedef {object} SimulcastLayer
 * @property {string} rid 層の名前（英数字 16 文字まで）
 * @property {number} [scaleResolutionDownBy] 解像度を何分の 1 にするか（既定: 低い層から 4・2・1）
 * @property {number} [maxBitrateKbps] この層の上限（kbit/s）
 * @property {number} [maxFramerate]
 */
/** 既定の 3 層（低い順。Chrome で 3 層にするには送る映像が 1280x720 以上であること）。 */
export const DEFAULT_SIMULCAST: readonly (Readonly<{
    rid: "r0";
    scaleResolutionDownBy: 4;
    maxBitrateKbps: 150;
}> | Readonly<{
    rid: "r1";
    scaleResolutionDownBy: 2;
    maxBitrateKbps: 500;
}> | Readonly<{
    rid: "r2";
    scaleResolutionDownBy: 1;
    maxBitrateKbps: 1500;
}>)[];
/**
 * 受信の統計（1 トラック）。
 */
export type InboundStats = {
    /**
     * SENCA のトラック ID
     */
    trackId: string;
    /**
     * 送り手の connection_id
     */
    participantId: string | undefined;
    clientId: string | undefined;
    kind: "audio" | "video";
    mid: string;
    /**
     * サーバーが今配っている層（`layer.changed`）
     */
    layer: string | null;
    bytesReceived: number;
    packetsReceived: number;
    packetsLost: number;
    jitterMs: number | undefined;
    framesDecoded: number | undefined;
    framesDropped: number | undefined;
    framesPerSecond: number | undefined;
    frameWidth: number | undefined;
    frameHeight: number | undefined;
    audioLevel: number | undefined;
    /**
     * 前回の stats() からの受信の速さ
     */
    bitrateBps: number | undefined;
};
/**
 * 送信の統計（1 つの層・トラック）。
 */
export type OutboundStats = {
    /**
     * SENCA のトラック ID（answer の `published`）
     */
    trackId: string | undefined;
    kind: "audio" | "video";
    rid: string | undefined;
    mid: string;
    bytesSent: number;
    packetsSent: number;
    framesSent: number | undefined;
    framesPerSecond: number | undefined;
    frameWidth: number | undefined;
    frameHeight: number | undefined;
    targetBitrateBps: number | undefined;
    qualityLimitationReason: string | undefined;
    active: boolean | undefined;
    /**
     * 前回の stats() からの送信の速さ
     */
    bitrateBps: number | undefined;
};
/**
 * セッションの統計（`session.stats()`）。
 */
export type SessionStats = {
    /**
     * 測った時刻（UNIX ミリ秒）
     */
    at: number;
    transport: {
        state: string;
        rttMs: number | undefined;
        availableOutgoingBitrateBps: number | undefined;
        availableIncomingBitrateBps: number | undefined;
        bytesSent: number;
        bytesReceived: number;
    };
    outbound: OutboundStats[];
    inbound: InboundStats[];
    dataChannels: Array<{
        label: string;
        state: string;
        messagesSent: number;
        messagesReceived: number;
        bytesSent: number;
        bytesReceived: number;
    }>;
    /**
     * ブラウザーの getStats() そのもの
     */
    raw: RTCStatsReport;
};
/**
 * サイマルキャストの 1 層。
 */
export type SimulcastLayer = {
    /**
     * 層の名前（英数字 16 文字まで）
     */
    rid: string;
    /**
     * 解像度を何分の 1 にするか（既定: 低い層から 4・2・1）
     */
    scaleResolutionDownBy?: number | undefined;
    /**
     * この層の上限（kbit/s）
     */
    maxBitrateKbps?: number | undefined;
    maxFramerate?: number | undefined;
};
