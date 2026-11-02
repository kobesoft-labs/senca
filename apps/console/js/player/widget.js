// 画面に置く再生部品（video ＋ 状態表示 ＋ 開始・停止）。URL は getUrl() で署名付きのものを取る。

import { h, replace } from '../lib/dom.js';
import { copyField, describeError } from '../lib/ui.js';
import { HlsPlayer, UnsupportedError } from './hls.js';

/**
 * @param {object} opt
 * @param {() => Promise<string>} opt.getUrl
 * @param {boolean} [opt.live]
 * @param {string} [opt.label]
 * @returns {{el: HTMLElement, stop: () => void}}
 */
export function playerWidget({ getUrl, live = true, label = 'プレビュー' }) {
  const video = h('video', { controls: true, playsinline: true, muted: true, 'aria-label': label });
  const statusEl = h('div', { class: 'player-status', role: 'status', 'aria-live': 'polite' }, '停止中');
  const note = h('div');
  let player = null;
  let lastUrl = null;
  let statsTimer = null;

  const setStatus = (s) => {
    const parts = [s.message];
    if (s.mode) parts.push(s.mode === 'mse' ? 'MSE' : 'native');
    if (s.mime) parts.push(s.mime);
    if (s.stats && s.stats.latency !== null && s.stats.latency !== undefined) parts.push(`遅れ ${s.stats.latency.toFixed(1)} 秒`);
    statusEl.textContent = parts.join(' · ');
    statusEl.dataset.state = s.state;
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

  const start = async () => {
    stop();
    replace(note);
    startBtn.disabled = true;
    stopBtn.disabled = false;
    statusEl.textContent = '再生 URL を取得中';
    try {
      lastUrl = await getUrl();
    } catch (e) {
      statusEl.textContent = describeError(e);
      statusEl.dataset.state = 'error';
      startBtn.disabled = false;
      stopBtn.disabled = true;
      return;
    }
    player = new HlsPlayer(video, {
      url: lastUrl,
      refreshUrl: async () => { lastUrl = await getUrl(); return lastUrl; },
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
      replace(note, h('div', { class: 'alert warn' },
        h('div', null, msg),
        e instanceof UnsupportedError && e.kind === 'ts'
          ? h('div', { class: 'small' }, 'HLS の URL（期限付き）を VLC・ffplay などで開けます。Chrome・Firefox で見るには、配信の設定で HLS のコンテナを fMP4 にしてください。')
          : null,
        h('div', { style: { marginTop: '6px' } }, copyField(lastUrl, 'HLS の URL'))));
    }
  };

  const startBtn = h('button', { class: 'primary', onClick: start }, '▶ 再生');
  const stopBtn = h('button', { onClick: stop, disabled: true }, '■ 停止');
  const el = h('div', { class: 'player-widget' },
    h('div', { class: 'player' }, video),
    h('div', { class: 'row', style: { marginTop: '8px' } }, startBtn, stopBtn, statusEl),
    note);
  el.video = video;
  return { el, stop, start, video };
}
