// このブラウザから送る（WHIP、RFC 9725）: カメラ・マイク、または画面 → POST /whip/{stream}（Bearer の stream key）。
//
// - stream key は貼り付けてもらい、どこにも保存しない（localStorage・sessionStorage・URL に入れない）。送り始めたら欄を空にする。
// - 映像のコーデックは選べる（既定は H.264: HLS・録画とそのままつながる）。royalty_free の stream なら VP8・VP9・AV1。
// - 統計は getStats の outbound-rtp（送信のビットレート・fps・大きさ・符号化したフレーム）と、往復時間。
// - 止めるときは Location のセッションを DELETE して、トラックを止める。

import { h, replace, select, field, kv } from '../lib/dom.js';
import { toast, reportError } from '../lib/ui.js';
import { streams, listAll } from '../api/resources.js';
import { fmtBps } from '../lib/format.js';

const CODECS = [['video/H264', 'H.264'], ['video/VP8', 'VP8'], ['video/VP9', 'VP9'], ['video/AV1', 'AV1']];
const SIZES = [['1280x720', '1280×720'], ['1920x1080', '1920×1080'], ['640x360', '640×360']];

function preferCodec(transceiver, mime) {
  if (!RTCRtpSender.getCapabilities) return;
  const caps = RTCRtpSender.getCapabilities('video');
  if (!caps) return;
  let want = caps.codecs.filter((c) => c.mimeType === mime);
  if (mime === 'video/H264') {
    const pm1 = want.filter((c) => /packetization-mode=1/.test(c.sdpFmtpLine || ''));
    if (pm1.length) want = pm1;
  }
  if (!want.length) throw new Error(`このブラウザは ${mime.slice(6)} で送れません`);
  const rest = caps.codecs.filter((c) => /rtx|red|ulpfec/i.test(c.mimeType));
  transceiver.setCodecPreferences([...want, ...rest]);
}

export async function render(root) {
  let streamList = [];
  let pc = null;
  let media = null;
  let location_ = null;
  let timer = null;
  let prev = null;

  const streamSel = h('select');
  const key = h('input', { type: 'password', autocomplete: 'off', spellcheck: 'false', placeholder: 'sk_…' });
  const source = select([['camera', 'カメラとマイク'], ['screen', '画面（＋マイク）']], 'camera', { 'aria-label': '送るもの' });
  const size = select(SIZES, '1280x720', { 'aria-label': '大きさ' });
  const codec = select(CODECS, 'video/H264', { 'aria-label': '映像のコーデック' });
  const bitrate = h('input', { type: 'number', min: 100, max: 20000, step: 100, value: 2500, 'aria-label': '映像のビットレート（kbps）', style: { width: '100px' } });
  const audioOn = h('input', { type: 'checkbox', checked: true });
  const preview = h('video', { muted: true, playsinline: true, autoplay: true, 'aria-label': '送っている映像' });
  const statusEl = h('div', { role: 'status', 'aria-live': 'polite', class: 'player-status', 'data-state': 'idle' }, '止まっています');
  const statsBox = h('div', { 'data-stats': '' });
  const whipInfo = h('div', { class: 'small muted' });
  const startBtn = h('button', { class: 'primary' }, '● 送信を開始');
  const stopBtn = h('button', { disabled: true }, '■ 止める');

  const setStatus = (state, text) => {
    statusEl.dataset.state = state;
    statusEl.textContent = text;
  };
  const current = () => streamList.find((x) => x.id === streamSel.value);
  const renderInfo = () => {
    const s = current();
    replace(whipInfo, s ? (s.ingest.whip_url ? ['WHIP: ', h('code', { class: 'break' }, s.ingest.whip_url), `（${s.codec_profile}）`] : 'このノードは WHIP の URL を出していません（公開の HTTP の URL が無い）') : '');
    if (s && s.codec_profile === 'royalty_free' && (codec.value === 'video/H264')) codec.value = 'video/VP8';
  };
  streamSel.addEventListener('change', renderInfo);

  const stop = async (why) => {
    clearInterval(timer);
    timer = null;
    prev = null;
    if (location_) {
      const u = location_;
      location_ = null;
      fetch(u, { method: 'DELETE', credentials: 'omit', keepalive: true }).catch(() => { /* 片付けだけ */ });
    }
    if (pc) { try { pc.close(); } catch { /* 何もしない */ } }
    pc = null;
    if (media) media.getTracks().forEach((t) => t.stop());
    media = null;
    preview.srcObject = null;
    startBtn.disabled = false;
    stopBtn.disabled = true;
    setStatus('idle', why || '止まっています');
  };

  const poll = async () => {
    if (!pc) return;
    let out = null;
    let rtt = null;
    let audio = null;
    try {
      (await pc.getStats()).forEach((x) => {
        if (x.type === 'outbound-rtp' && x.kind === 'video') out = x;
        if (x.type === 'outbound-rtp' && x.kind === 'audio') audio = x;
        if (x.type === 'remote-inbound-rtp' && x.kind === 'video' && x.roundTripTime !== undefined) rtt = x.roundTripTime;
        if (x.type === 'candidate-pair' && x.nominated && x.currentRoundTripTime !== undefined && rtt === null) rtt = x.currentRoundTripTime;
      });
    } catch { return; }
    const now = performance.now();
    const bytes = out ? out.bytesSent : 0;
    const bps = prev ? ((bytes - prev.bytes) * 8 * 1000) / (now - prev.t) : null;
    prev = { bytes, t: now };
    replace(statsBox, kv([
      ['接続', pc.connectionState],
      ['映像', out ? `${out.frameWidth || '?'}×${out.frameHeight || '?'}・${Math.round(out.framesPerSecond || 0)} fps・${bps !== null ? fmtBps(Math.round(bps)) : '—'}` : '—'],
      ['符号化したフレーム', out ? String(out.framesEncoded || 0) : '0'],
      ['コーデック', out && out.encoderImplementation ? `${codec.selectedOptions[0].textContent}（${out.encoderImplementation}）` : codec.selectedOptions[0].textContent],
      ['音声', audio ? `${audio.packetsSent || 0} パケット` : 'なし'],
      ['往復時間', rtt !== null ? `${Math.round(rtt * 1000)} ms` : '—'],
    ]));
    statsBox.dataset.frames = String(out ? out.framesEncoded || 0 : 0);
  };

  const start = async () => {
    const s = current();
    if (!s) { toast('送り先の配信を選んでください', 'warn'); return; }
    if (!s.ingest.whip_url) { toast('このノードは WHIP の URL を出していません', 'warn'); return; }
    const k = key.value.trim();
    if (!k) { toast('ストリームキーを貼り付けてください（鍵は保存しません）', 'warn'); key.focus(); return; }
    startBtn.disabled = true;
    stopBtn.disabled = false;
    setStatus('loading', 'カメラ・画面を開いています');
    const [w, hh] = size.value.split('x').map(Number);
    try {
      if (source.value === 'screen') {
        media = await navigator.mediaDevices.getDisplayMedia({ video: { width: w, height: hh, frameRate: 30 }, audio: false });
        if (audioOn.checked) {
          const mic = await navigator.mediaDevices.getUserMedia({ audio: true }).catch(() => null);
          if (mic) mic.getAudioTracks().forEach((t) => media.addTrack(t));
        }
      } else {
        media = await navigator.mediaDevices.getUserMedia({ video: { width: w, height: hh, frameRate: 30 }, audio: audioOn.checked });
      }
      preview.srcObject = media;
      pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
      pc.onconnectionstatechange = () => {
        if (!pc) return;
        const st = pc.connectionState;
        if (st === 'connected') setStatus('live', '送信中');
        else if (st === 'failed') stop('接続に失敗しました');
        else if (st === 'disconnected') setStatus('loading', '接続が途切れています');
      };
      for (const t of media.getTracks()) {
        const tr = pc.addTransceiver(t, { direction: 'sendonly', ...(t.kind === 'video' ? { sendEncodings: [{ maxBitrate: Math.max(100, Number(bitrate.value) || 2500) * 1000 }] } : {}) });
        if (t.kind === 'video') preferCodec(tr, codec.value);
      }
      setStatus('loading', 'WHIP で接続中');
      await pc.setLocalDescription(await pc.createOffer());
      if (pc.iceGatheringState !== 'complete') {
        await Promise.race([
          new Promise((res) => pc.addEventListener('icegatheringstatechange', () => { if (pc && pc.iceGatheringState === 'complete') res(); })),
          new Promise((res) => setTimeout(res, 1000)),
        ]);
      }
      const r = await fetch(s.ingest.whip_url, {
        method: 'POST', credentials: 'omit', cache: 'no-store',
        headers: { 'Content-Type': 'application/sdp', Authorization: `Bearer ${k}` },
        body: pc.localDescription.sdp,
      });
      const body = await r.text();
      if (r.status !== 201) {
        const why = { 401: 'ストリームキーが違います', 409: 'この配信には別の送り手がいます', 415: '本文の形式が違います', 503: 'このノードでは WebRTC が動いていません' }[r.status];
        throw new Error(`WHIP ${r.status}: ${why || body}`);
      }
      const loc = r.headers.get('Location');
      location_ = loc ? new URL(loc, s.ingest.whip_url).toString() : null;
      await pc.setRemoteDescription({ type: 'answer', sdp: body });
      key.value = ''; // 鍵は持ち続けない
      timer = setInterval(poll, 1000);
      toast('送信を始めました。配信の状態が live になるまで数秒かかります', 'ok');
    } catch (e) {
      await stop(`送れません: ${e.message}`);
      reportError(e, '送信');
    }
  };
  startBtn.addEventListener('click', start);
  stopBtn.addEventListener('click', () => stop('止めました'));

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, 'このブラウザから送る')),
    h('div', { class: 'alert info small' }, 'カメラ・マイク、または画面を WebRTC（WHIP）で配信へ送ります。ストリームキーは貼り付けてください。この画面は鍵を保存しません（送り始めると欄を空にします）。'),
    h('div', { class: 'grid cols-2' },
      h('section', { class: 'panel', 'aria-labelledby': 'snd-set' }, h('h2', { id: 'snd-set' }, '設定'),
        h('div', { class: 'form' },
          field('送り先の配信', streamSel), whipInfo,
          field('ストリームキー（保存しません）', key),
          field('送るもの', source), field('大きさ', size), field('映像のコーデック', codec, 'royalty_free の配信には VP8・VP9・AV1 を選びます'),
          field('映像のビットレート（kbps）', bitrate),
          h('label', { class: 'check' }, audioOn, 'マイクの音声も送る'),
          h('div', { class: 'actions' }, startBtn, stopBtn))),
      h('section', { class: 'panel', 'aria-labelledby': 'snd-pv' }, h('h2', { id: 'snd-pv' }, '送っている映像'),
        h('div', { class: 'player' }, preview), statusEl, statsBox)));

  try { streamList = await listAll(streams.list); } catch (e) { reportError(e, '配信の一覧'); }
  const want = new URLSearchParams(location.hash.split('?')[1] || '').get('stream');
  replace(streamSel, streamList.length ? streamList.map((x) => h('option', { value: x.id, selected: x.id === want }, `${x.name}（${x.observed_state}）`)) : h('option', { value: '' }, '配信がありません'));
  renderInfo();

  return () => { stop(); key.value = ''; };
}
