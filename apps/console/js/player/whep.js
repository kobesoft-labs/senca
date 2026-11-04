// WHEP（WebRTC の再生、draft-ietf-wish-whep）の小さなクライアント。低遅延のプレビュー・スイッチャーのモニター用。
//
// 流れ: RTCPeerConnection（映像・音声とも recvonly）→ offer を再生 URL（playback.whep、署名付き）へ POST
// （application/sdp）→ 201 の answer を setRemoteDescription → 届いたトラックを video に。止めるときは
// Location のセッションを DELETE する。
//
// - 署名は URL のクエリ（exp・sig）にあるので Authorization は付けない（管理 API のトークンを送らない）。
// - ノードは配る stream のコーデックだけを申し出る（AAC は WebRTC で運べないので音声が無いことがある）。
// - stream が配信中でない・コーデックを運べないときは 422（WhepError.status）。呼び手は HLS に戻せる。

import { t } from '../i18n/index.js';
export class WhepError extends Error {
  constructor(status, message) {
    super(message);
    this.name = 'WhepError';
    this.status = status;
  }
}

export function whepSupported() {
  return typeof RTCPeerConnection === 'function';
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export class WhepPlayer {
  /**
   * @param {HTMLVideoElement} video
   * @param {object} opt
   * @param {string} opt.url WHEP の URL（playback.whep）
   * @param {(s:{state:string, message:string, stats?:object}) => void} [opt.onStatus]
   * @param {number} [opt.iceWaitMs] offer に候補を入れるために待つ上限（既定 1000 ms）
   */
  constructor(video, opt) {
    this.video = video;
    this.url = opt.url;
    this.onStatus = opt.onStatus || (() => {});
    this.iceWaitMs = opt.iceWaitMs ?? 1000;
    this.pc = null;
    this.location = null;
    this.dead = false;
    this.stats = null;
    this.prev = null;
    this.timer = null;
    this.mode = 'webrtc';
  }

  status(state, message) {
    if (!this.dead) this.onStatus({ state, message, mode: 'webrtc', stats: this.stats });
  }

  async start() {
    if (!whepSupported()) throw new WhepError(0, t('player.noWebrtc'));
    const pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
    this.pc = pc;
    pc.addTransceiver('video', { direction: 'recvonly' });
    pc.addTransceiver('audio', { direction: 'recvonly' });
    const stream = new MediaStream();
    pc.ontrack = (e) => {
      stream.addTrack(e.track);
      if (this.video.srcObject !== stream) this.video.srcObject = stream;
    };
    pc.onconnectionstatechange = () => {
      const s = pc.connectionState;
      if (s === 'connected') this.status('playing', t('player.webrtcPlaying'));
      else if (s === 'failed') this.status('error', t('player.webrtcFailed'));
      else if (s === 'disconnected') this.status('loading', t('player.webrtcDisconnected'));
    };
    this.status('loading', t('player.webrtcConnecting'));
    await pc.setLocalDescription(await pc.createOffer());
    // 候補を offer に入れる（ノードは trickle の PATCH も受けるが、1 往復で済ませる）
    if (pc.iceGatheringState !== 'complete') {
      await Promise.race([
        new Promise((res) => pc.addEventListener('icegatheringstatechange', () => { if (pc.iceGatheringState === 'complete') res(); })),
        sleep(this.iceWaitMs),
      ]);
    }
    if (this.dead) return;
    let r;
    try {
      r = await fetch(this.url, { method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: pc.localDescription.sdp, credentials: 'omit', cache: 'no-store' });
    } catch (e) {
      throw new WhepError(0, t('player.whepConnectFailed', { reason: e.message }));
    }
    const body = await r.text();
    if (r.status !== 201 && r.status !== 200) {
      const why = ([403, 404, 422, 503].includes(r.status) ? t(`player.whep${r.status}`) : undefined);
      throw new WhepError(r.status, `WHEP ${r.status}: ${why || body || r.statusText}`);
    }
    const loc = r.headers.get('Location');
    this.location = loc ? new URL(loc, this.url).toString() : null;
    if (this.dead) { this.deleteSession(); return; }
    await pc.setRemoteDescription({ type: 'answer', sdp: body });
    this.video.muted = true;
    this.video.play().catch(() => { /* 自動再生の制限: 利用者の操作で再生できる */ });
    this.timer = setInterval(() => this.poll(), 1000);
  }

  async poll() {
    if (!this.pc || this.dead) return;
    let v = null;
    let a = null;
    try {
      const rep = await this.pc.getStats();
      rep.forEach((x) => {
        if (x.type === 'inbound-rtp' && x.kind === 'video') v = x;
        if (x.type === 'inbound-rtp' && x.kind === 'audio') a = x;
      });
    } catch { return; }
    const now = performance.now();
    const bytes = (v ? v.bytesReceived : 0) + (a ? a.bytesReceived : 0);
    const bps = this.prev ? ((bytes - this.prev.bytes) * 8 * 1000) / (now - this.prev.t) : null;
    this.prev = { bytes, t: now };
    this.stats = {
      framesDecoded: v ? v.framesDecoded || 0 : 0,
      framesDropped: v ? v.framesDropped || 0 : 0,
      width: v ? v.frameWidth || 0 : 0,
      height: v ? v.frameHeight || 0 : 0,
      fps: v ? v.framesPerSecond || 0 : 0,
      packetsLost: (v ? v.packetsLost || 0 : 0) + (a ? a.packetsLost || 0 : 0),
      jitter: v ? v.jitter : null,
      audio: !!a,
      bitrate: bps,
    };
    if (this.pc.connectionState === 'connected') this.status('playing', t('player.webrtcPlaying'));
  }

  deleteSession() {
    if (!this.location) return;
    const u = this.location;
    this.location = null;
    fetch(u, { method: 'DELETE', credentials: 'omit', keepalive: true }).catch(() => { /* 片付けだけ */ });
  }

  destroy() {
    this.dead = true;
    clearInterval(this.timer);
    this.deleteSession();
    if (this.pc) {
      try { this.pc.close(); } catch { /* 何もしない */ }
    }
    this.pc = null;
    if (this.video.srcObject) {
      try { this.video.srcObject.getTracks().forEach((t) => t.stop()); } catch { /* 何もしない */ }
    }
    this.video.srcObject = null;
  }
}
