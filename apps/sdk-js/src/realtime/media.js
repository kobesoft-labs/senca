// メディアの小道具: コーデックの優先順、サイマルキャストの層、getStats の要約。

/**
 * サイマルキャストの 1 層。
 * @typedef {object} SimulcastLayer
 * @property {string} rid 層の名前（英数字 16 文字まで）
 * @property {number} [scaleResolutionDownBy] 解像度を何分の 1 にするか（既定: 低い層から 4・2・1）
 * @property {number} [maxBitrateKbps] この層の上限（kbit/s）
 * @property {number} [maxFramerate]
 */

/** 既定の 3 層（低い順。Chrome で 3 層にするには送る映像が 1280x720 以上であること）。 */
export const DEFAULT_SIMULCAST = Object.freeze([
  Object.freeze({ rid: 'r0', scaleResolutionDownBy: 4, maxBitrateKbps: 150 }),
  Object.freeze({ rid: 'r1', scaleResolutionDownBy: 2, maxBitrateKbps: 500 }),
  Object.freeze({ rid: 'r2', scaleResolutionDownBy: 1, maxBitrateKbps: 1500 }),
]);

const RID = /^[A-Za-z0-9]{1,16}$/;

/**
 * 公開の指定からサイマルキャストの層（低い順）を決める。無ければ null。
 * @param {boolean | SimulcastLayer[] | {layers?: SimulcastLayer[]} | undefined} sc
 * @param {number} [topKbps] 一番上の層の上限（部屋の max_video_bitrate_kbps・公開の bitrate）
 * @returns {SimulcastLayer[] | null}
 */
export function simulcastLayers(sc, topKbps) {
  if (!sc) return null;
  /** @type {SimulcastLayer[]} */
  let layers = sc === true ? DEFAULT_SIMULCAST.map((l) => ({ ...l })) : Array.isArray(sc) ? sc.map((l) => ({ ...l })) : (sc.layers ?? DEFAULT_SIMULCAST).map((l) => ({ ...l }));
  if (layers.length < 1 || layers.length > 4) throw new RangeError('simulcast: 層は 1〜4 個');
  const seen = new Set();
  for (const l of layers) {
    if (!RID.test(l.rid)) throw new RangeError(`simulcast: rid ${JSON.stringify(l.rid)} は英数字 1〜16 文字`);
    if (seen.has(l.rid)) throw new RangeError(`simulcast: rid ${l.rid} が重複`);
    seen.add(l.rid);
  }
  if (topKbps) {
    const top = layers[layers.length - 1];
    top.maxBitrateKbps = Math.min(top.maxBitrateKbps ?? topKbps, topKbps);
    for (const l of layers) if (l.maxBitrateKbps && l.maxBitrateKbps > topKbps) l.maxBitrateKbps = topKbps;
  }
  return layers;
}

/**
 * 層 → RTCRtpEncodingParameters（低い順。SDP の a=simulcast もこの順になる）。
 * @param {SimulcastLayer[]} layers
 * @returns {RTCRtpEncodingParameters[]}
 */
export function encodingsFor(layers) {
  const n = layers.length;
  return layers.map((l, i) => {
    /** @type {RTCRtpEncodingParameters} */
    const e = { rid: l.rid, active: true, scaleResolutionDownBy: l.scaleResolutionDownBy ?? Math.pow(2, n - 1 - i) };
    if (l.maxBitrateKbps) e.maxBitrate = l.maxBitrateKbps * 1000;
    if (l.maxFramerate) e.maxFramerate = l.maxFramerate;
    return e;
  });
}

/**
 * 部屋のコーデックだけを残した優先順（再送の rtx・FEC は残す）。H.264 は packetization-mode=1 の baseline を先に。
 * 見つからなければ null（そのブラウザーでは送受できない）。
 * @param {ReadonlyArray<RTCRtpCodec>} codecs `RTCRtpReceiver.getCapabilities(kind).codecs`
 * @param {string} name `vp8`・`vp9`・`h264`・`av1`・`opus`
 * @returns {RTCRtpCodec[] | null}
 */
export function preferCodec(codecs, name) {
  const want = name.toLowerCase();
  const main = codecs.filter((c) => c.mimeType.split('/')[1]?.toLowerCase() === want);
  if (!main.length) return null;
  if (want === 'h264') {
    const score = (/** @type {RTCRtpCodec} */ c) => {
      const f = c.sdpFmtpLine ?? '';
      return (f.includes('packetization-mode=1') ? 2 : 0) + (/profile-level-id=42e0/i.test(f) ? 1 : 0);
    };
    main.sort((a, b) => score(b) - score(a));
  }
  const extra = codecs.filter((c) => /\/(rtx|red|ulpfec|flexfec-03)$/i.test(c.mimeType));
  return [...main, ...extra];
}

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
export function summarizeStats(report, ctx) {
  const at = Date.now();
  /** @type {SessionStats} */
  const out = {
    at,
    transport: { state: ctx.state, rttMs: undefined, availableOutgoingBitrateBps: undefined, availableIncomingBitrateBps: undefined, bytesSent: 0, bytesReceived: 0 },
    outbound: [],
    inbound: [],
    dataChannels: [],
    raw: report,
  };
  const prev = ctx.previous;
  const dt = prev ? (at - prev.at) / 1000 : 0;
  /** @param {number} now @param {number | undefined} before */
  const rate = (now, before) => (dt > 0 && before !== undefined && now >= before ? Math.round(((now - before) * 8) / dt) : undefined);
  report.forEach((/** @type {any} */ r) => {
    switch (r.type) {
      case 'candidate-pair':
        if (r.nominated || r.selected || r.state === 'succeeded') {
          if (r.currentRoundTripTime !== undefined) out.transport.rttMs = Math.round(r.currentRoundTripTime * 1000);
          if (r.availableOutgoingBitrate !== undefined) out.transport.availableOutgoingBitrateBps = r.availableOutgoingBitrate;
          if (r.availableIncomingBitrate !== undefined) out.transport.availableIncomingBitrateBps = r.availableIncomingBitrate;
          out.transport.bytesSent = Math.max(out.transport.bytesSent, r.bytesSent ?? 0);
          out.transport.bytesReceived = Math.max(out.transport.bytesReceived, r.bytesReceived ?? 0);
        }
        break;
      case 'inbound-rtp': {
        const s = ctx.slotOf(r.mid);
        if (!s) break;
        const before = prev?.inbound.find((x) => x.mid === r.mid && x.trackId === s.trackId);
        out.inbound.push({
          trackId: s.trackId, participantId: s.participantId, clientId: s.clientId, kind: r.kind, mid: r.mid, layer: s.layer,
          bytesReceived: r.bytesReceived ?? 0, packetsReceived: r.packetsReceived ?? 0, packetsLost: r.packetsLost ?? 0,
          jitterMs: r.jitter !== undefined ? r.jitter * 1000 : undefined,
          framesDecoded: r.framesDecoded, framesDropped: r.framesDropped, framesPerSecond: r.framesPerSecond,
          frameWidth: r.frameWidth, frameHeight: r.frameHeight, audioLevel: r.audioLevel,
          bitrateBps: rate(r.bytesReceived ?? 0, before?.bytesReceived),
        });
        break;
      }
      case 'outbound-rtp': {
        const before = prev?.outbound.find((x) => x.mid === r.mid && x.rid === r.rid);
        out.outbound.push({
          trackId: ctx.publishedOf(r.mid), kind: r.kind, rid: r.rid, mid: r.mid,
          bytesSent: r.bytesSent ?? 0, packetsSent: r.packetsSent ?? 0, framesSent: r.framesSent,
          framesPerSecond: r.framesPerSecond, frameWidth: r.frameWidth, frameHeight: r.frameHeight,
          targetBitrateBps: r.targetBitrate, qualityLimitationReason: r.qualityLimitationReason, active: r.active,
          bitrateBps: rate(r.bytesSent ?? 0, before?.bytesSent),
        });
        break;
      }
      case 'data-channel':
        out.dataChannels.push({ label: r.label, state: r.state, messagesSent: r.messagesSent ?? 0, messagesReceived: r.messagesReceived ?? 0, bytesSent: r.bytesSent ?? 0, bytesReceived: r.bytesReceived ?? 0 });
        break;
      default:
        break;
    }
  });
  return out;
}

/**
 * サーバーへ `pong.stats` で送る小さな要約（64 KiB に収める）。
 * @param {SessionStats} s
 */
export function compactStats(s) {
  return {
    at: s.at,
    rtt_ms: s.transport.rttMs,
    available_outgoing_bitrate_bps: s.transport.availableOutgoingBitrateBps,
    outbound: s.outbound.slice(0, 16).map((o) => ({ track_id: o.trackId, rid: o.rid, kind: o.kind, bitrate_bps: o.bitrateBps, fps: o.framesPerSecond, width: o.frameWidth, height: o.frameHeight, limit: o.qualityLimitationReason })),
    inbound: s.inbound.slice(0, 64).map((i) => ({ track_id: i.trackId, kind: i.kind, layer: i.layer, bitrate_bps: i.bitrateBps, lost: i.packetsLost, fps: i.framesPerSecond, width: i.frameWidth, height: i.frameHeight, decoded: i.framesDecoded })),
  };
}
