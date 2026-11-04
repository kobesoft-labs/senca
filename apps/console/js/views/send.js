// このブラウザから送る（WHIP、RFC 9725）: カメラ・マイク、または画面 → POST /whip/{stream}（Bearer の stream key）。
//
// 流れ: 送り先と鍵を選ぶ →「カメラをオンにする」で映像と音の大きさを確かめる（まだ送らない）→「配信を開始」→「止める」。
// - stream key は貼り付けてもらい、どこにも保存しない（localStorage・sessionStorage・URL に入れない）。送り始めたら欄を空にする。
//   配信の作成の直後の案内から来たときだけ、メモリで受け取った鍵を欄に入れておく（lib/streaming.js の handOffKey）。
// - 機器はカメラ・マイクを選べる（許可の後でないと名前が分からないので、許可の後に一覧を作り直す）。
// - 音の大きさは Web Audio の AnalyserNode で見る（送る音そのものは変えない）。
// - 許可されない・機器が無い・ほかのアプリが使っている・HTTPS でない、などは理由と直し方を出す。
// - 統計は getStats の outbound-rtp（送信のビットレート・fps・大きさ・符号化したフレーム）と、往復時間。

import { h, replace, select } from '../lib/dom.js';
import { toast, reportError } from '../lib/ui.js';
import { streams, listAll } from '../api/resources.js';
import { fmtBps, fmtDuration } from '../lib/format.js';
import { t } from '../i18n/index.js';
import { button, pageHeader, panel, kvList, formField, alert, helpHint, segmented, badge } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { takeHandOffKey, hashParams, setHashParams, streamState } from '../lib/streaming.js';

const CODECS = [['video/H264', 'H.264'], ['video/VP8', 'VP8'], ['video/VP9', 'VP9'], ['video/AV1', 'AV1']];
const SIZES = [['1280x720', '720p（1280×720）'], ['1920x1080', '1080p（1920×1080）'], ['640x360', '360p（640×360）']];

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

/** getUserMedia・getDisplayMedia の失敗 → 理由と直し方。 */
export function mediaErrorText(e, source) {
  const n = e && e.name;
  if (!window.isSecureContext) return t('send.err.insecure');
  if (n === 'NotAllowedError' || n === 'PermissionDeniedError') return source === 'screen' ? t('send.err.screenDenied') : t('send.err.denied');
  if (n === 'NotFoundError' || n === 'DevicesNotFoundError') return t('send.err.notFound');
  if (n === 'NotReadableError' || n === 'TrackStartError') return t('send.err.inUse');
  if (n === 'OverconstrainedError') return t('send.err.overconstrained');
  if (n === 'AbortError') return t('send.err.aborted');
  return t('send.cannotSend', { reason: (e && e.message) || String(e) });
}

export async function render(root) {
  let streamList = [];
  let pc = null;
  let media = null;
  let location_ = null;
  let timer = null;
  let prev = null;
  let liveSince = null;
  let audioCtx = null;
  let raf = null;
  const params = hashParams();

  const streamSel = h('select', { 'data-testid': 'send-stream' });
  const key = h('input', { type: 'password', autocomplete: 'off', spellcheck: 'false', placeholder: 'sk_…', 'data-testid': 'send-key' });
  const source = segmented({ label: t('send.source'), testid: 'send-source', value: 'camera', options: [['camera', t('send.camera'), 'camera'], ['screen', t('send.screen'), 'monitor']], onChange: () => { syncSource(); if (media && !pc) openPreview(); } });
  const camSel = h('select', { 'data-testid': 'send-camera' }, h('option', { value: '' }, t('send.devDefault')));
  const micSel = h('select', { 'data-testid': 'send-mic' }, h('option', { value: '' }, t('send.devDefault')));
  const size = select(SIZES, '1280x720');
  const codec = select(CODECS, 'video/H264');
  const bitrate = h('input', { type: 'number', min: 100, max: 20000, step: 100, value: 2500 });
  const audioOn = h('input', { type: 'checkbox', checked: true, 'data-testid': 'send-audio' });
  const preview = h('video', { muted: true, playsinline: true, autoplay: true, 'aria-label': t('send.previewLabel') });
  const statusEl = h('div', { role: 'status', 'aria-live': 'polite', class: 'player-status', 'data-state': 'idle' }, t('send.idle'));
  const statsBox = h('div', { 'data-stats': '' });
  const whipInfo = h('div', { class: 'small muted' });
  const errBox = h('div', { 'data-testid': 'send-error' });
  const levelFill = h('div', { class: 'lvl-fill' });
  const level = h('div', { class: 'level', role: 'meter', 'aria-label': t('send.level'), 'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': '0', 'data-testid': 'send-level' }, icon('mic'), h('div', { class: 'lvl-track' }, levelFill));
  const onAir = h('div', { class: 'send-chip', 'data-state': 'off' });
  const placeholder = h('div', { class: 'send-placeholder' },
    icon('camera'),
    h('strong', null, t('send.offTitle')),
    h('span', null, t('send.offBody')));
  const previewBtn = button(t('send.preview'), { icon: 'camera', testid: 'send-preview', onClick: () => openPreview() });
  const startBtn = button(t('send.start'), { variant: 'primary', icon: 'record', testid: 'send-start', size: 'lg' });
  const stopBtn = button(t('send.stop'), { icon: 'stop', testid: 'send-stop', size: 'lg', attrs: { disabled: true } });
  const camField = formField({ label: t('send.cameraDev'), input: camSel });
  const micField = formField({ label: t('send.micDev'), input: micSel });

  const setStatus = (state, text) => {
    statusEl.dataset.state = state;
    statusEl.textContent = text;
    onAir.dataset.state = state === 'live' ? 'live' : media ? 'preview' : 'off';
    replace(onAir, state === 'live' ? [h('span', { class: 'dot live' }), t('send.chipLive')] : media ? [icon('eye', { size: 'sm' }), t('send.chipPreview')] : [icon('eye-off', { size: 'sm' }), t('send.chipOff')]);
  };
  const showError = (msg, title) => replace(errBox, msg ? alert('error', msg, { title: title || t('send.errTitle') }) : null);
  const current = () => streamList.find((x) => x.id === streamSel.value);
  const renderInfo = () => {
    const s = current();
    replace(whipInfo, s
      ? (s.ingest.whip_url
        ? [streamState(s) === 'live' ? badge(t('status.live'), 'live') : null, ' ', h('a', { href: `#/streams/${encodeURIComponent(s.id)}` }, t('send.openStream'), icon('arrow-right', { size: 'sm' }))]
        : alert('warn', t('send.noWhipUrlLong'), { small: true }))
      : '');
    if (s && s.codec_profile === 'royalty_free' && codec.value === 'video/H264') codec.value = 'video/VP8';
    setHashParams({ stream: s ? s.id : null });
  };
  streamSel.addEventListener('change', renderInfo);
  const syncSource = () => { camField.hidden = source.value === 'screen'; };

  const listDevices = async () => {
    if (!navigator.mediaDevices || !navigator.mediaDevices.enumerateDevices) return;
    const devs = await navigator.mediaDevices.enumerateDevices().catch(() => []);
    const fill = (sel, kind, fallback) => {
      const cur = sel.value;
      const list = devs.filter((d) => d.kind === kind);
      replace(sel, list.length && list.some((d) => d.label)
        ? list.map((d, i) => h('option', { value: d.deviceId, selected: d.deviceId === cur }, d.label || `${fallback} ${i + 1}`))
        : h('option', { value: '' }, t('send.devDefault')));
    };
    fill(camSel, 'videoinput', t('send.cameraDev'));
    fill(micSel, 'audioinput', t('send.micDev'));
  };

  const stopLevel = () => {
    if (raf) cancelAnimationFrame(raf);
    raf = null;
    if (audioCtx) audioCtx.close().catch(() => {});
    audioCtx = null;
    levelFill.style.width = '0%';
    level.setAttribute('aria-valuenow', '0');
    level.classList.add('off');
  };
  const startLevel = () => {
    stopLevel();
    const tr = media && media.getAudioTracks()[0];
    if (!tr || !window.AudioContext) return;
    level.classList.remove('off');
    audioCtx = new AudioContext();
    const src = audioCtx.createMediaStreamSource(new MediaStream([tr]));
    const an = audioCtx.createAnalyser();
    an.fftSize = 512;
    src.connect(an);
    const buf = new Float32Array(an.fftSize);
    let lastAria = 0;
    const loop = () => {
      an.getFloatTimeDomainData(buf);
      let peak = 0;
      for (const v of buf) peak = Math.max(peak, Math.abs(v));
      // dBFS（-60〜0）を 0〜100 に
      const db = peak > 0 ? 20 * Math.log10(peak) : -60;
      const pct = Math.max(0, Math.min(100, ((db + 60) / 60) * 100));
      levelFill.style.width = `${pct.toFixed(0)}%`;
      level.dataset.level = pct > 95 ? 'clip' : pct > 75 ? 'hot' : 'ok';
      if (Date.now() - lastAria > 500) { level.setAttribute('aria-valuenow', pct.toFixed(0)); lastAria = Date.now(); }
      raf = requestAnimationFrame(loop);
    };
    loop();
  };

  const releaseMedia = () => {
    if (media) media.getTracks().forEach((x) => x.stop());
    media = null;
    preview.srcObject = null;
    placeholder.hidden = false;
    stopLevel();
  };

  /** 映像と音を開く（送らない）。失敗したら理由を出して null。 */
  const openMedia = async () => {
    showError(null);
    if (!navigator.mediaDevices) { showError(t('send.err.insecure')); return null; }
    const [w, hh] = size.value.split('x').map(Number);
    const src = source.value;
    try {
      let m;
      if (src === 'screen') {
        m = await navigator.mediaDevices.getDisplayMedia({ video: { width: w, height: hh, frameRate: 30 }, audio: false });
        if (audioOn.checked) {
          const mic = await navigator.mediaDevices.getUserMedia({ audio: micSel.value ? { deviceId: { exact: micSel.value } } : true }).catch(() => null);
          if (mic) mic.getAudioTracks().forEach((x) => m.addTrack(x));
          else toast(t('send.micUnavailable'), 'warn');
        }
      } else {
        m = await navigator.mediaDevices.getUserMedia({
          video: { width: w, height: hh, frameRate: 30, ...(camSel.value ? { deviceId: { exact: camSel.value } } : {}) },
          audio: audioOn.checked ? (micSel.value ? { deviceId: { exact: micSel.value } } : true) : false,
        });
      }
      // 画面の共有をブラウザの「共有を停止」で止めたら、送信も止める
      m.getVideoTracks().forEach((x) => x.addEventListener('ended', () => { if (media === m) stop(t('send.sourceEnded')); }));
      return m;
    } catch (e) {
      showError(mediaErrorText(e, src), src === 'screen' ? t('send.err.screenTitle') : t('send.err.cameraTitle'));
      return null;
    }
  };

  const openPreview = async () => {
    if (pc) return;
    releaseMedia();
    setStatus('loading', t('send.opening'));
    const m = await openMedia();
    if (!m) { setStatus('idle', t('send.idle')); return; }
    media = m;
    preview.srcObject = media;
    placeholder.hidden = true;
    startLevel();
    await listDevices();
    setStatus('idle', t('send.previewing'));
  };
  for (const el of [camSel, micSel, size, audioOn]) el.addEventListener('change', () => { if (media && !pc) openPreview(); });

  const stop = async (why) => {
    clearInterval(timer);
    timer = null;
    prev = null;
    liveSince = null;
    if (location_) {
      const u = location_;
      location_ = null;
      fetch(u, { method: 'DELETE', credentials: 'omit', keepalive: true }).catch(() => { /* 片付けだけ */ });
    }
    if (pc) { try { pc.close(); } catch { /* 何もしない */ } }
    pc = null;
    releaseMedia();
    startBtn.disabled = false;
    stopBtn.disabled = true;
    previewBtn.disabled = false;
    root.querySelector('.send-layout')?.classList.remove('is-live');
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
      [t('send.stats.time'), liveSince ? fmtDuration(Date.now() - liveSince) : '—'],
      [t('send.stats.video'), out ? `${out.frameWidth || '?'}×${out.frameHeight || '?'} · ${Math.round(out.framesPerSecond || 0)} fps · ${bps !== null ? fmtBps(Math.round(bps)) : '—'}` : '—'],
      [t('send.stats.frames'), out ? String(out.framesEncoded || 0) : '0'],
      [t('send.codec'), out && out.encoderImplementation ? `${codec.selectedOptions[0].textContent} (${out.encoderImplementation})` : codec.selectedOptions[0].textContent],
      [t('send.stats.audio'), audio ? t('send.stats.packets', { n: audio.packetsSent || 0 }) : t('common.no')],
      [t('send.stats.rtt'), rtt !== null ? `${Math.round(rtt * 1000)} ms` : '—'],
      [t('send.stats.connection'), pc.connectionState],
    ]));
    statsBox.dataset.frames = String(out ? out.framesEncoded || 0 : 0);
  };

  const start = async () => {
    const s = current();
    showError(null);
    if (!s) { showError(t('send.chooseStream')); streamSel.focus(); return; }
    if (!s.ingest.whip_url) { showError(t('send.noWhipUrlLong')); return; }
    const k = key.value.trim();
    if (!k) { keyField.setError(t('send.needKey')); key.focus(); return; }
    keyField.setError(null);
    startBtn.disabled = true;
    previewBtn.disabled = true;
    stopBtn.disabled = false;
    try {
      if (!media) {
        setStatus('loading', t('send.opening'));
        media = await openMedia();
        if (!media) { await stop(t('send.idle')); return; }
        preview.srcObject = media;
        placeholder.hidden = true;
        startLevel();
      }
      pc = new RTCPeerConnection({ bundlePolicy: 'max-bundle' });
      pc.onconnectionstatechange = () => {
        if (!pc) return;
        const st = pc.connectionState;
        if (st === 'connected') {
          liveSince = liveSince || Date.now();
          root.querySelector('.send-layout')?.classList.add('is-live');
          setStatus('live', t('send.live'));
        } else if (st === 'failed') { showError(t('send.err.iceFailed')); stop(t('send.failed')); }
        else if (st === 'disconnected') setStatus('loading', t('send.disconnected'));
      };
      for (const tr of media.getTracks()) {
        const x = pc.addTransceiver(tr, { direction: 'sendonly', ...(tr.kind === 'video' ? { sendEncodings: [{ maxBitrate: Math.max(100, Number(bitrate.value) || 2500) * 1000 }] } : {}) });
        if (tr.kind === 'video') preferCodec(x, codec.value);
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
        const why = [401, 403, 409, 415, 422, 503].includes(r.status) ? t(`send.whip${r.status}`) : null;
        const err = new Error(why || `WHIP ${r.status}: ${body.slice(0, 200)}`);
        err.whip = r.status;
        throw err;
      }
      const loc = r.headers.get('Location');
      location_ = loc ? new URL(loc, s.ingest.whip_url).toString() : null;
      await pc.setRemoteDescription({ type: 'answer', sdp: body });
      key.value = ''; // 鍵は持ち続けない
      timer = setInterval(poll, 1000);
      toast(t('send.started'), 'ok');
    } catch (e) {
      await stop(t('send.cannotSend', { reason: e.message }));
      showError(e.whip === 401 ? t('send.whip401Long') : e.message, t('send.errTitle'));
      if (e.whip === 401) key.focus();
    }
  };
  startBtn.addEventListener('click', start);
  stopBtn.addEventListener('click', () => stop(t('send.stoppedByUser')));

  const keyField = formField({ label: t('send.key'), input: key, required: true, hint: t('send.keyHint') });
  const advanced = h('details', { class: 'disclosure' },
    h('summary', null, icon('settings'), h('span', null, t('send.advanced'))),
    h('div', { class: 'form' },
      h('div', { class: 'cols' },
        formField({ label: t('send.codec'), input: codec, hint: t('send.codecHint') }),
        formField({ label: t('send.bitrate'), input: bitrate }))));

  root.append(
    pageHeader({
      title: t('send.title'),
      description: t('send.lead'),
      help: helpHint({ title: t('send.title'), body: t('send.help'), guide: '04-ingest-playback.md' }),
    }),
    errBox,
    h('div', { class: 'send-layout' },
      h('div', { class: 'send-stage' },
        h('div', { class: 'player send-player' }, preview, placeholder, onAir),
        level,
        h('div', { class: 'send-actions' }, previewBtn, h('span', { class: 'spacer' }), stopBtn, startBtn),
        h('div', { class: 'player-controls' }, statusEl),
        statsBox),
      panel({ title: t('send.settings'), icon: 'settings', class: 'send-settings' },
        h('div', { class: 'form' },
          formField({ label: t('send.target'), input: streamSel, required: true }), whipInfo,
          keyField,
          h('div', { class: 'field' }, h('span', { class: 'label' }, t('send.source')), source),
          camField, micField,
          formField({ label: t('send.size'), input: size }),
          h('label', { class: 'check' }, audioOn, t('send.withMic')),
          advanced))));
  setStatus('idle', t('send.idle'));
  stopLevel();
  syncSource();

  try { streamList = await listAll(streams.list); } catch (e) { reportError(e, t('schedules.streamList')); }
  const want = params.get('stream');
  replace(streamSel, streamList.length ? streamList.map((x) => h('option', { value: x.id, selected: x.id === want }, x.name)) : h('option', { value: '' }, t('send.noStreams')));
  if (!streamList.length) {
    errBox.after(alert('info', t('send.needStreamBody'), { title: t('send.needStream'), actions: [h('a', { class: 'btn primary sm', href: '#/streams?new=1' }, t('streams.create'))] }));
    startBtn.disabled = true;
  }
  renderInfo();
  const handed = want ? takeHandOffKey(want) : null;
  if (handed) {
    key.value = handed;
    keyField.querySelector('.hint').textContent = t('send.keyHanded');
  }
  listDevices();

  return () => { stop(); key.value = ''; };
}
