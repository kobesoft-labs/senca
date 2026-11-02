// getStats から送信のビットレート・fps・解像度・往復時間を求める。

export class StatsMeter {
  constructor() {
    this.prev = new Map();
  }

  /**
   * @param {RTCPeerConnection} pc
   * @returns {Promise<{videoKbps:number, audioKbps:number, fps:number, width:number, height:number,
   *   rttMs:number|null, limitation:string, layers:number}>}
   */
  async sample(pc) {
    const report = await pc.getStats();
    const out = { videoKbps: 0, audioKbps: 0, fps: 0, width: 0, height: 0, rttMs: null, limitation: '', layers: 0 };
    report.forEach((s) => {
      if (s.type === 'outbound-rtp') {
        const p = this.prev.get(s.id);
        this.prev.set(s.id, { bytes: s.bytesSent, ts: s.timestamp });
        const kbps = p && s.timestamp > p.ts ? ((s.bytesSent - p.bytes) * 8) / (s.timestamp - p.ts) : 0;
        if (s.kind === 'video') {
          out.videoKbps += kbps;
          out.layers += 1;
          // simulcast では一番大きい層の値を出す
          if ((s.frameWidth || 0) >= out.width) {
            out.width = s.frameWidth || 0;
            out.height = s.frameHeight || 0;
            out.fps = s.framesPerSecond || 0;
            if (s.qualityLimitationReason && s.qualityLimitationReason !== 'none') {
              out.limitation = s.qualityLimitationReason;
            }
          }
        } else if (s.kind === 'audio') {
          out.audioKbps += kbps;
        }
      } else if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') {
        if (typeof s.currentRoundTripTime === 'number') out.rttMs = s.currentRoundTripTime * 1000;
      }
    });
    return out;
  }
}
