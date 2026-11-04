// 画面に置く再生部品（video ＋ 状態表示 ＋ 開始・停止）。URL は署名付きのものをその都度取る。
//
// - getPlayback() があれば方式を選べる: 自動（WebRTC の WHEP → だめなら HLS）・WebRTC・HLS。
// - getUrl() だけなら HLS（録画の再生など）。

import { h, replace, select } from '../lib/dom.js';
import { copyField, describeError } from '../lib/ui.js';
import { HlsPlayer, UnsupportedError } from './hls.js';
import { WhepPlayer, whepSupported } from './whep.js';
import { fmtBps } from '../lib/format.js';
import { t } from '../i18n/index.js';
import { button, alert } from '../ui/index.js';

/**
 * @param {object} opt
 * @param {() => Promise<string>} [opt.getUrl] HLS の URL
 * @param {() => Promise<{hls:string, whep?:string}>} [opt.getPlayback] 再生 URL 一式（playback-url の playback）
 * @param {boolean} [opt.live]
 * @param {string} [opt.label]
 * @param {'auto'|'webrtc'|'hls'} [opt.mode]
 * @param {boolean} [opt.autostart]
 * @returns {{el: HTMLElement, stop: () => void, start: () => Promise<void>, video: HTMLVideoElement}}
 */
export function playerWidget({ getUrl, getPlayback, live = true, label = t('player.preview'), mode = 'auto', autostart = false }) {
  const video = h('video', { controls: true, playsinline: true, muted: true, autoplay: true, 'aria-label': label });
  const statusEl = h('div', { class: 'player-status', role: 'status', 'aria-live': 'polite' }, t('player.stopped'));
  const note = h('div');
  const modeSel = getPlayback ? select([['auto', t('player.modeAuto')], ['webrtc', t('player.modeWebrtc')], ['hls', t('player.modeHls')]], mode, { 'aria-label': t('player.mode'), class: 'small' }) : null;
  if (modeSel) modeSel.style.width = 'auto';
  let player = null;
  let lastUrl = null;
  let statsTimer = null;

  const setStatus = (s) => {
    const parts = [s.message];
    if (s.mode === 'webrtc') {
      parts.push('WebRTC');
      const st = s.stats;
      if (st) {
        if (st.width) parts.push(`${st.width}×${st.height}`);
        if (st.fps) parts.push(`${Math.round(st.fps)} fps`);
        parts.push(t('player.decoded', { n: st.framesDecoded }));
        if (st.bitrate) parts.push(fmtBps(Math.round(st.bitrate)));
        if (!st.audio) parts.push(t('player.noAudio'));
      }
    } else {
      if (s.mode) parts.push(s.mode === 'mse' ? 'MSE' : 'native');
      if (s.mime) parts.push(s.mime);
      if (s.stats && s.stats.latency !== null && s.stats.latency !== undefined) parts.push(t('player.latency', { s: s.stats.latency.toFixed(1) }));
    }
    statusEl.textContent = parts.join(' · ');
    statusEl.dataset.state = s.state;
    statusEl.dataset.mode = s.mode || '';
  };

  const stop = () => {
    clearInterval(statsTimer);
    if (player) player.destroy();
    player = null;
    startBtn.disabled = false;
    stopBtn.disabled = true;
    statusEl.textContent = t('player.stopped');
    statusEl.dataset.state = 'stopped';
  };

  const showUrl = (msg, url, kind) => replace(note, alert('warn', [
    h('div', null, msg),
    kind === 'ts' ? h('div', { class: 'small' }, t('player.tsHint')) : null,
    url ? h('div', { class: 'mt-2' }, copyField(url, t('io.pb.hlsUrl'))) : null,
  ]));

  const startHls = async (url) => {
    lastUrl = url;
    player = new HlsPlayer(video, {
      url,
      refreshUrl: async () => { lastUrl = getPlayback ? (await getPlayback()).hls : await getUrl(); return lastUrl; },
      startAtLiveEdge: live,
      onStatus: setStatus,
    });
    const p = player;
    statsTimer = setInterval(() => {
      if (p.mode === 'mse' && p.stats && statusEl.dataset.state === 'playing') setStatus({ state: 'playing', message: live ? t('player.livePlaying') : t('player.playing'), mode: p.mode, mime: p.mime, stats: p.stats });
    }, 1000);
    try {
      await p.start();
    } catch (e) {
      if (p !== player) return;
      const msg = e instanceof UnsupportedError ? e.message : t('player.cannotPlay', { reason: e.message });
      statusEl.textContent = msg;
      statusEl.dataset.state = 'error';
      showUrl(msg, lastUrl, e instanceof UnsupportedError ? e.kind : null);
    }
  };

  const startWhep = async (url) => {
    const p = new WhepPlayer(video, { url, onStatus: setStatus });
    player = p;
    statsTimer = setInterval(() => { if (p === player && p.stats) setStatus({ state: statusEl.dataset.state || 'playing', message: t('player.webrtcPlaying'), mode: 'webrtc', stats: p.stats }); }, 1000);
    await p.start();
  };

  const start = async () => {
    stop();
    replace(note);
    startBtn.disabled = true;
    stopBtn.disabled = false;
    statusEl.textContent = t('player.gettingUrl');
    let pb;
    try {
      pb = getPlayback ? await getPlayback() : { hls: await getUrl() };
    } catch (e) {
      statusEl.textContent = describeError(e);
      statusEl.dataset.state = 'error';
      startBtn.disabled = false;
      stopBtn.disabled = true;
      return;
    }
    const m = modeSel ? modeSel.value : 'hls';
    if (m !== 'hls' && pb.whep && whepSupported()) {
      try {
        await startWhep(pb.whep);
        return;
      } catch (e) {
        clearInterval(statsTimer);
        if (player) player.destroy();
        player = null;
        if (m === 'webrtc') {
          statusEl.textContent = e.message;
          statusEl.dataset.state = 'error';
          return;
        }
        replace(note, alert('info', t('player.fellBack', { reason: e.message }), { small: true }));
      }
    }
    if (m === 'webrtc') {
      statusEl.textContent = pb.whep ? t('player.noWebrtc') : t('player.noWhepUrl');
      statusEl.dataset.state = 'error';
      return;
    }
    await startHls(pb.hls);
  };

  const startBtn = button(t('player.play'), { variant: 'primary', icon: 'play', onClick: () => { start(); } });
  const stopBtn = button(t('player.stop'), { icon: 'stop', onClick: stop, attrs: { disabled: true } });
  const el = h('div', { class: 'player-widget' },
    h('div', { class: 'player' }, video),
    h('div', { class: 'player-controls' }, startBtn, stopBtn, modeSel, statusEl),
    note);
  el.video = video;
  if (autostart) setTimeout(start, 0);
  return { el, stop, start, video, status: statusEl, get player() { return player; } };
}
