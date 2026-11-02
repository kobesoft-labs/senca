// 画面に置く再生部品（video ＋ 状態表示 ＋ 開始・停止）。URL は署名付きのものをその都度取る。
//
// - getPlayback() があれば方式を選べる: 自動（WebRTC の WHEP → だめなら HLS）・WebRTC・HLS。
// - getUrl() だけなら HLS（録画の再生など）。

import { h, replace, select } from '../lib/dom.js';
import { copyField, describeError } from '../lib/ui.js';
import { HlsPlayer, UnsupportedError } from './hls.js';
import { WhepPlayer, whepSupported } from './whep.js';
import { fmtBps } from '../lib/format.js';

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
export function playerWidget({ getUrl, getPlayback, live = true, label = 'プレビュー', mode = 'auto', autostart = false }) {
  const video = h('video', { controls: true, playsinline: true, muted: true, autoplay: true, 'aria-label': label });
  const statusEl = h('div', { class: 'player-status', role: 'status', 'aria-live': 'polite' }, '停止中');
  const note = h('div');
  const modeSel = getPlayback ? select([['auto', '自動（WebRTC → HLS）'], ['webrtc', 'WebRTC（WHEP、低遅延）'], ['hls', 'HLS（MSE）']], mode, { 'aria-label': '再生の方式', class: 'small' }) : null;
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
        parts.push(`復号 ${st.framesDecoded}`);
        if (st.bitrate) parts.push(fmtBps(Math.round(st.bitrate)));
        if (!st.audio) parts.push('音声なし');
      }
    } else {
      if (s.mode) parts.push(s.mode === 'mse' ? 'MSE' : 'native');
      if (s.mime) parts.push(s.mime);
      if (s.stats && s.stats.latency !== null && s.stats.latency !== undefined) parts.push(`遅れ ${s.stats.latency.toFixed(1)} 秒`);
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
    statusEl.textContent = '停止中';
    statusEl.dataset.state = 'stopped';
  };

  const showUrl = (msg, url, kind) => replace(note, h('div', { class: 'alert warn' },
    h('div', null, msg),
    kind === 'ts' ? h('div', { class: 'small' }, 'HLS の URL（期限付き）を VLC・ffplay などで開けます。Chrome・Firefox で見るには、配信の設定で HLS のコンテナを fMP4 にしてください。') : null,
    url ? h('div', { style: { marginTop: '6px' } }, copyField(url, 'HLS の URL')) : null));

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
      if (p.mode === 'mse' && p.stats && statusEl.dataset.state === 'playing') setStatus({ state: 'playing', message: live ? 'ライブ再生中' : '再生中', mode: p.mode, mime: p.mime, stats: p.stats });
    }, 1000);
    try {
      await p.start();
    } catch (e) {
      if (p !== player) return;
      const msg = e instanceof UnsupportedError ? e.message : `再生できません: ${e.message}`;
      statusEl.textContent = msg;
      statusEl.dataset.state = 'error';
      showUrl(msg, lastUrl, e instanceof UnsupportedError ? e.kind : null);
    }
  };

  const startWhep = async (url) => {
    const p = new WhepPlayer(video, { url, onStatus: setStatus });
    player = p;
    statsTimer = setInterval(() => { if (p === player && p.stats) setStatus({ state: statusEl.dataset.state || 'playing', message: 'WebRTC で再生中', mode: 'webrtc', stats: p.stats }); }, 1000);
    await p.start();
  };

  const start = async () => {
    stop();
    replace(note);
    startBtn.disabled = true;
    stopBtn.disabled = false;
    statusEl.textContent = '再生 URL を取得中';
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
        replace(note, h('div', { class: 'alert info small' }, `WebRTC で再生できないので HLS にしました（${e.message}）`));
      }
    }
    if (m === 'webrtc') {
      statusEl.textContent = pb.whep ? 'このブラウザは WebRTC を使えません' : 'WHEP の URL がありません';
      statusEl.dataset.state = 'error';
      return;
    }
    await startHls(pb.hls);
  };

  const startBtn = h('button', { class: 'primary', onClick: start }, '▶ 再生');
  const stopBtn = h('button', { onClick: stop, disabled: true }, '■ 停止');
  const el = h('div', { class: 'player-widget' },
    h('div', { class: 'player' }, video),
    h('div', { class: 'row', style: { marginTop: '8px' } }, startBtn, stopBtn, modeSel, statusEl),
    note);
  el.video = video;
  if (autostart) setTimeout(start, 0);
  return { el, stop, start, video, status: statusEl, get player() { return player; } };
}
