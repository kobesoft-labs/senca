// WHIP（RFC 9725）の送り手。ブラウザの標準の RTCPeerConnection だけを使う（仕様 6.3: ICE/DTLS は置き換えない）。
//
// 流れ:
//   1. offer を作り、ICE 候補の収集を最大 1 秒待ってから POST（Content-Type: application/sdp、
//      Authorization: Bearer）。201 Created の Location が WHIP セッションの URL、本文が answer。
//   2. 残りの候補は PATCH（application/trickle-ice-sdpfrag、RFC 8840、If-Match: ETag）で送る。
//      相手が 405 / 501 を返したら trickle をやめる（POST に入れた候補だけで続ける）。
//   3. 止めるときは DELETE。
//   4. 接続が failed（または disconnected が続く）になったら、セッションを消して最初からつなぎ直す（間隔は倍々、上限 15 秒）。
//   5. 201 の Link: rel="ice-server" があれば ICE サーバーとして使う（RFC 9725 4.6）。
//
// イベント（EventTarget）: 'state'（detail: {state, message}）

const GATHER_WAIT_MS = 1000;
const TRICKLE_BATCH_MS = 100;
const DISCONNECTED_GRACE_MS = 5000;

export class WhipError extends Error {
  constructor(message, status) {
    super(message);
    this.status = status;
  }
}

/** Link ヘッダーから ICE サーバーを読む（RFC 9725 4.6、RFC 8288）。 */
export function parseIceServerLinks(header) {
  if (!header) return [];
  const out = [];
  // カンマで区切る（< > と " " の中のカンマは除く）
  const parts = header.match(/<[^>]*>(?:\s*;\s*[^;,=\s]+(?:\s*=\s*(?:"[^"]*"|[^;,]*))?)*/g) || [];
  for (const p of parts) {
    const url = p.match(/^<([^>]*)>/)?.[1];
    const params = {};
    for (const m of p.matchAll(/;\s*([a-zA-Z-]+)\s*=\s*("([^"]*)"|[^;,]*)/g)) {
      params[m[1].toLowerCase()] = m[3] ?? m[2].trim();
    }
    if (!url || params.rel !== 'ice-server' || !/^(stuns?|turns?):/.test(url)) continue;
    const s = { urls: url };
    if (params.username) s.username = params.username;
    if (params.credential) s.credential = params.credential;
    out.push(s);
  }
  return out;
}

/** 送るときの設定。 */
export class PublishOptions {
  constructor({ videoBitrateKbps = 2500, audioBitrateKbps = 128, simulcast = false, codec = '' } = {}) {
    this.videoBitrateKbps = videoBitrateKbps;
    this.audioBitrateKbps = audioBitrateKbps;
    this.simulcast = simulcast;
    /** 'video/H264' など。空なら既定のまま。 */
    this.codec = codec;
  }
}

export class WhipClient extends EventTarget {
  /**
   * @param {{url: string, token?: string, iceServers?: RTCIceServer[]}} cfg
   */
  constructor(cfg) {
    super();
    this.url = cfg.url;
    this.token = cfg.token || '';
    this.iceServers = cfg.iceServers || [];
    this.pc = null;
    this.resource = null;
    this.etag = null;
    this.state = 'idle';
    this.stopped = false;
    this.trickle = true;
    this.pending = [];
    this.flushTimer = null;
    this.retryMs = 1000;
    this.retryTimer = null;
    this.discTimer = null;
  }

  setState(state, message = '') {
    this.state = state;
    this.dispatchEvent(new CustomEvent('state', { detail: { state, message } }));
  }

  headers(extra = {}) {
    const h = { ...extra };
    if (this.token) h.Authorization = `Bearer ${this.token}`;
    return h;
  }

  /**
   * 送り始める。
   * @param {MediaStream} stream
   * @param {PublishOptions} opts
   */
  async publish(stream, opts = new PublishOptions()) {
    this.stream = stream;
    this.opts = opts;
    this.stopped = false;
    await this.connect();
  }

  async connect() {
    this.setState('connecting');
    const pc = new RTCPeerConnection({ iceServers: this.iceServers, bundlePolicy: 'max-bundle' });
    this.pc = pc;
    this.trickle = true;
    this.pending = [];
    for (const track of this.stream.getTracks()) {
      if (track.kind === 'video') {
        const init = { direction: 'sendonly', streams: [this.stream] };
        const max = this.opts.videoBitrateKbps * 1000;
        if (this.opts.simulcast) {
          init.sendEncodings = [
            { rid: 'l', scaleResolutionDownBy: 4, maxBitrate: Math.round(max / 8) },
            { rid: 'm', scaleResolutionDownBy: 2, maxBitrate: Math.round(max / 3) },
            { rid: 'h', maxBitrate: max },
          ];
        } else {
          init.sendEncodings = [{ maxBitrate: max }];
        }
        const tr = pc.addTransceiver(track, init);
        this.preferCodec(tr, this.opts.codec);
      } else {
        pc.addTransceiver(track, {
          direction: 'sendonly',
          streams: [this.stream],
          sendEncodings: [{ maxBitrate: this.opts.audioBitrateKbps * 1000 }],
        });
      }
    }
    pc.addEventListener('icecandidate', (e) => this.onCandidate(e.candidate));
    pc.addEventListener('connectionstatechange', () => this.onConnectionState());

    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    await this.waitGathering(pc);
    // POST に入れた候補は trickle で送り直さない
    const sent = new Set(pc.localDescription.sdp.split(/\r?\n/).filter((l) => l.startsWith('a=candidate:')));
    this.pending = this.pending.filter((c) => !sent.has('a=' + c.candidate));

    let res;
    try {
      res = await fetch(this.url, {
        method: 'POST',
        headers: this.headers({ 'Content-Type': 'application/sdp' }),
        body: pc.localDescription.sdp,
      });
    } catch (e) {
      throw new WhipError(`サーバーにつながりません（${e.message}）`, 0);
    }
    if (res.status !== 201) {
      const text = await res.text().catch(() => '');
      pc.close();
      throw new WhipError(describeStatus(res.status, text), res.status);
    }
    const loc = res.headers.get('Location');
    if (!loc) {
      pc.close();
      throw new WhipError('サーバーの応答に Location がありません', res.status);
    }
    this.resource = new URL(loc, this.url).href;
    this.etag = res.headers.get('ETag');
    const ice = parseIceServerLinks(res.headers.get('Link'));
    if (ice.length) {
      try {
        pc.setConfiguration({ ...pc.getConfiguration(), iceServers: ice });
      } catch {
        // 変えられない構成なら最初の設定のまま
      }
    }
    const answer = await res.text();
    await pc.setRemoteDescription({ type: 'answer', sdp: answer });
    this.scheduleFlush();
  }

  preferCodec(transceiver, mime) {
    if (!mime || !transceiver.setCodecPreferences || !RTCRtpSender.getCapabilities) return;
    const caps = RTCRtpSender.getCapabilities('video')?.codecs || [];
    const first = caps.filter((c) => c.mimeType.toLowerCase() === mime.toLowerCase());
    if (!first.length) return;
    // 再送（rtx）・FEC は残す
    const rest = caps.filter((c) => c.mimeType.toLowerCase() !== mime.toLowerCase());
    try {
      transceiver.setCodecPreferences([...first, ...rest]);
    } catch {
      // 対応しないブラウザでは既定のまま
    }
  }

  waitGathering(pc) {
    if (pc.iceGatheringState === 'complete') return Promise.resolve();
    return new Promise((resolve) => {
      const t = setTimeout(done, GATHER_WAIT_MS);
      function done() {
        clearTimeout(t);
        pc.removeEventListener('icegatheringstatechange', check);
        resolve();
      }
      function check() {
        if (pc.iceGatheringState === 'complete') done();
      }
      pc.addEventListener('icegatheringstatechange', check);
    });
  }

  onCandidate(c) {
    // null は収集の終わり（end-of-candidates を送る）
    this.pending.push(c ? c : { end: true });
    if (this.resource) this.scheduleFlush();
  }

  scheduleFlush() {
    if (this.flushTimer || !this.trickle || !this.pending.length) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = null;
      this.flush().catch(() => {});
    }, TRICKLE_BATCH_MS);
  }

  /** 溜めた候補を 1 つの PATCH で送る（RFC 8840 の sdpfrag）。 */
  async flush() {
    if (!this.trickle || !this.resource || !this.pending.length || !this.pc) return;
    const batch = this.pending.splice(0);
    const sdp = this.pc.localDescription?.sdp || '';
    const ufrag = sdp.match(/a=ice-ufrag:(.*)/)?.[1]?.trim();
    const pwd = sdp.match(/a=ice-pwd:(.*)/)?.[1]?.trim();
    const lines = [];
    if (ufrag) lines.push(`a=ice-ufrag:${ufrag}`);
    if (pwd) lines.push(`a=ice-pwd:${pwd}`);
    // BUNDLE なので最初の m= 行にまとめる
    const mline = sdp.match(/^m=.*$/m)?.[0]?.trim();
    const mid = sdp.match(/a=mid:(.*)/)?.[1]?.trim();
    if (mline) lines.push(mline);
    if (mid !== undefined) lines.push(`a=mid:${mid}`);
    for (const c of batch) {
      if (c.end) lines.push('a=end-of-candidates');
      else if (c.candidate) lines.push(`a=${c.candidate}`);
    }
    const h = this.headers({ 'Content-Type': 'application/trickle-ice-sdpfrag' });
    h['If-Match'] = this.etag || '*';
    let res;
    try {
      res = await fetch(this.resource, { method: 'PATCH', headers: h, body: lines.join('\r\n') + '\r\n' });
    } catch {
      return;
    }
    if (res.status === 405 || res.status === 501) {
      // trickle に対応しないサーバー: POST に入れた候補だけで続ける
      this.trickle = false;
      this.pending = [];
    } else if (res.ok) {
      this.etag = res.headers.get('ETag') || this.etag;
    }
    this.scheduleFlush();
  }

  onConnectionState() {
    const s = this.pc?.connectionState;
    if (this.stopped) return;
    clearTimeout(this.discTimer);
    if (s === 'connected') {
      this.retryMs = 1000;
      this.setState('connected');
    } else if (s === 'disconnected') {
      this.setState('reconnecting', '回線が途切れています');
      this.discTimer = setTimeout(() => this.reconnect('回線が戻りません'), DISCONNECTED_GRACE_MS);
    } else if (s === 'failed') {
      this.reconnect('接続が切れました');
    }
  }

  async reconnect(reason) {
    if (this.stopped) return;
    await this.teardown();
    const wait = this.retryMs;
    this.retryMs = Math.min(this.retryMs * 2, 15000);
    this.setState('reconnecting', `${reason}。${Math.round(wait / 1000)} 秒後につなぎ直します`);
    clearTimeout(this.retryTimer);
    this.retryTimer = setTimeout(async () => {
      if (this.stopped) return;
      try {
        await this.connect();
      } catch (e) {
        // 認証の失敗はつなぎ直しても同じなので止める
        if (e.status === 401 || e.status === 403 || e.status === 404) {
          this.stopped = true;
          this.setState('failed', e.message);
        } else {
          this.reconnect(e.message);
        }
      }
    }, wait);
  }

  /** セッションを消して PeerConnection を閉じる（トラックは止めない）。 */
  async teardown() {
    clearTimeout(this.flushTimer);
    this.flushTimer = null;
    const res = this.resource;
    this.resource = null;
    if (this.pc) {
      this.pc.close();
      this.pc = null;
    }
    if (res) {
      try {
        await fetch(res, { method: 'DELETE', headers: this.headers() });
      } catch {
        // 消せなくても相手は時間切れで消す
      }
    }
  }

  async stop() {
    this.stopped = true;
    clearTimeout(this.retryTimer);
    clearTimeout(this.discTimer);
    await this.teardown();
    this.setState('stopped');
  }
}

function describeStatus(status, text) {
  const detail = text && text.length < 200 ? `: ${text}` : '';
  switch (status) {
    case 400:
      return `サーバーが offer を受け付けませんでした（400）${detail}`;
    case 401:
      return 'トークンが正しくないか、期限が切れています（401）';
    case 403:
      return 'この配信へ送る権限がありません（403）';
    case 404:
      return '配信が見つかりません（404）。URL を確かめてください';
    case 409:
      return 'この配信には別の送り手がつながっています（409）';
    case 415:
      return 'サーバーが SDP を受け付けませんでした（415）';
    case 422:
      return `コーデックなどの条件が合いません（422）${detail}`;
    case 429:
    case 503:
      return `サーバーが混み合っています（${status}）。しばらくしてから試してください`;
    default:
      return `サーバーの応答が想定外です（${status}）${detail}`;
  }
}
