// このブラウザから送る（WHIP、RFC 9725）: カメラ・マイク、または画面 → POST /whip/{stream}（Bearer の stream key）。
//
// - stream key は貼り付けてもらい、どこにも保存しない（localStorage・sessionStorage・URL に入れない）。送り始めたら欄を空にする。
// - 映像のコーデックは選べる（既定は H.264: HLS・録画とそのままつながる）。royalty_free の stream なら VP8・VP9・AV1。
// - 統計は getStats の outbound-rtp（送信のビットレート・fps・大きさ・符号化したフレーム）と、往復時間。
// - 止めるときは Location のセッションを DELETE して、トラックを止める。

import { h, replace, select } from '../lib/dom.js';
import { toast, reportError } from '../lib/ui.js';
import { streams, listAll } from '../api/resources.js';
import { fmtBps } from '../lib/format.js';
import { t } from '../i18n/index.js';
import { button, pageHeader, panel, kvList, formField, alert, helpHint } from '../ui/index.js';

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
  if (!want.length) throw new Error(t('send.codecUnsupported', { codec: mime.slice(6) }));
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

  const streamSel = h('select', { 'data-testid': 'send-stream' });
  const key = h('input', { type: 'password', autocomplete: 'off', spellcheck: 'false', placeholder: 'sk_…' });
  const source = select([['camera', t('send.camera')], ['screen', t('send.screen')]], 'camera');
  const size = select(SIZES, '1280x720');
  const codec = select(CODECS, 'video/H264');
  const bitrate = h('input', { type: 'number', min: 100, max: 20000, step: 100, value: 2500 });
  const audioOn = h('input', { type: 'checkbox', checked: true });
  const preview = h('video', { muted: true, playsinline: true, autoplay: true, 'aria-label': t('send.previewLabel') });
  const statusEl = h('div', { role: 'status', 'aria-live': 'polite', class: 'player-status', 'data-state': 'idle' }, t('send.idle'));
  const statsBox = h('div', { 'data-stats': '' });
  const whipInfo = h('div', { class: 'small muted' });
  const startBtn = button(t('send.start'), { variant: 'primary', icon: 'record', testid: 'send-start' });
  const stopBtn = button(t('send.stop'), { icon: 'stop', testid: 'send-stop', attrs: { disabled: true } });

  const setStatus = (state, text) => {
    statusEl.dataset.state = state;
    statusEl.textContent = text;
  };
  const current = () => streamList.find((x) => x.id === streamSel.value);
  const renderInfo = () => {
    const s = current();
    replace(whipInfo, s ? (s.ingest.whip_url ? ['WHIP: ', h('code', { class: 'break' }, s.ingest.whip_url), ` (${s.codec_profile})`] : t('send.noWhipUrlLong')) : '');
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
    setStatus('idle', why || t('send.idle'));
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
    replace(statsBox, kvList([
      [t('send.stats.connection'), pc.connectionState],
      [t('send.stats.video'), out ? `${out.frameWidth || '?'}×${out.frameHeight || '?'} · ${Math.round(out.framesPerSecond || 0)} fps · ${bps !== null ? fmtBps(Math.round(bps)) : '—'}` : '—'],
      [t('send.stats.frames'), out ? String(out.framesEncoded || 0) : '0'],
      [t('send.codec'), out && out.encoderImplementation ? `${codec.selectedOptions[0].textContent} (${out.encoderImplementation})` : codec.selectedOptions[0].textContent],
      [t('send.stats.audio'), audio ? t('send.stats.packets', { n: audio.packetsSent || 0 }) : t('common.no')],
      [t('send.stats.rtt'), rtt !== null ? `${Math.round(rtt * 1000)} ms` : '—'],
    ]));
    statsBox.dataset.frames = String(out ? out.framesEncoded || 0 : 0);
  };

  const start = async () => {
    const s = current();
    if (!s) { toast(t('send.chooseStream'), 'warn'); return; }
    if (!s.ingest.whip_url) { toast(t('send.noWhipUrl'), 'warn'); return; }
    const k = key.value.trim();
    if (!k) { toast(t('send.needKey'), 'warn'); key.focus(); return; }
    startBtn.disabled = true;
    stopBtn.disabled = false;
    setStatus('loading', t('send.opening'));
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
        if (st === 'connected') setStatus('live', t('send.live'));
        else if (st === 'failed') stop(t('send.failed'));
        else if (st === 'disconnected') setStatus('loading', t('send.disconnected'));
      };
      for (const t of media.getTracks()) {
        const tr = pc.addTransceiver(t, { direction: 'sendonly', ...(t.kind === 'video' ? { sendEncodings: [{ maxBitrate: Math.max(100, Number(bitrate.value) || 2500) * 1000 }] } : {}) });
        if (t.kind === 'video') preferCodec(tr, codec.value);
      }
      setStatus('loading', t('send.connecting'));
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
        const why = [401, 409, 415, 503].includes(r.status) ? t(`send.whip${r.status}`) : null;
        throw new Error(`WHIP ${r.status}: ${why || body}`);
      }
      const loc = r.headers.get('Location');
      location_ = loc ? new URL(loc, s.ingest.whip_url).toString() : null;
      await pc.setRemoteDescription({ type: 'answer', sdp: body });
      key.value = ''; // 鍵は持ち続けない
      timer = setInterval(poll, 1000);
      toast(t('send.started'), 'ok');
    } catch (e) {
      await stop(t('send.cannotSend', { reason: e.message }));
      reportError(e, t('send.what'));
    }
  };
  startBtn.addEventListener('click', start);
  stopBtn.addEventListener('click', () => stop(t('send.stoppedByUser')));

  root.append(
    pageHeader({
      title: t('send.title'),
      description: t('send.lead'),
      help: helpHint({ title: t('send.title'), body: t('send.help'), guide: '04-ingest-playback.md' }),
    }),
    h('div', { class: 'grid cols-2' },
      panel({ title: t('send.settings'), icon: 'settings' },
        h('div', { class: 'form' },
          formField({ label: t('send.target'), input: streamSel, required: true }), whipInfo,
          formField({ label: t('send.key'), input: key, required: true, hint: t('send.keyHint') }),
          h('div', { class: 'cols' },
            formField({ label: t('send.source'), input: source }),
            formField({ label: t('send.size'), input: size })),
          h('div', { class: 'cols' },
            formField({ label: t('send.codec'), input: codec, hint: t('send.codecHint') }),
            formField({ label: t('send.bitrate'), input: bitrate })),
          h('label', { class: 'check' }, audioOn, t('send.withMic')),
          h('div', { class: 'actions' }, stopBtn, startBtn))),
      panel({ title: t('send.previewTitle'), icon: 'camera' },
        h('div', { class: 'player' }, preview), h('div', { class: 'player-controls' }, statusEl), statsBox)));

  try { streamList = await listAll(streams.list); } catch (e) { reportError(e, t('schedules.streamList')); }
  const want = new URLSearchParams(location.hash.split('?')[1] || '').get('stream');
  replace(streamSel, streamList.length ? streamList.map((x) => h('option', { value: x.id, selected: x.id === want }, `${x.name} (${x.observed_state})`)) : h('option', { value: '' }, t('send.noStreams')));
  if (!streamList.length) {
    root.querySelector('.grid').before(alert('info', t('send.needStreamBody'), { title: t('send.needStream'), actions: [h('a', { class: 'btn primary sm', href: '#/streams?new=1' }, t('streams.create'))] }));
  }
  renderInfo();

  return () => { stop(); key.value = ''; };
}
