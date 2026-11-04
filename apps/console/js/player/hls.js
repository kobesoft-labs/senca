// 外部ライブラリを使わない最小限の HLS 再生（fMP4 だけ、Media Source Extensions）。
//
// 流れ: プレイリストを読む → EXT-X-MAP の初期化セグメントからコーデック文字列を作って SourceBuffer を開く
// → セグメントを順に fetch して appendBuffer → ライブはプレイリストを読み直して続ける。
//
// - ライブは端から 3 本前のセグメントから始め、遅れすぎたら端へ寄せる。窓から落ちたら端へ飛ぶ。
// - 不連続（EXT-X-DISCONTINUITY・初期化セグメントの変更 = 送り手の再接続）では timestampOffset で
//   時刻をつなぐ。VOD はプレイリストの時刻（EXTINF の累計）に合わせるので、シークできる。
// - 署名付き URL の期限切れ（403）は refreshUrl() で URL を取り直す。
// - MPEG-TS のセグメントは扱わない（Safari のネイティブ再生へ回す。無理なら UnsupportedError）。
// - LL-HLS の part は使わない（完成したセグメントだけ）。ABR は無い（マスターなら最初の variant）。

import { t } from '../i18n/index.js';
import { parsePlaylist, containerOf } from './m3u8.js';
import { parseInit, firstDecodeTime, mimeFor, fixZeroSize } from './mp4.js';

export class UnsupportedError extends Error {
  constructor(kind, message) {
    super(message);
    this.name = 'UnsupportedError';
    this.kind = kind;
  }
}

class HttpError extends Error {
  constructor(status, url) {
    super(`HTTP ${status}`);
    this.status = status;
    this.url = url;
  }
}

const sleep = (ms, signal) => new Promise((res) => {
  const t = setTimeout(res, ms);
  if (signal) signal.addEventListener('abort', () => { clearTimeout(t); res(); }, { once: true });
});

function MediaSourceImpl() {
  return window.ManagedMediaSource || window.MediaSource || null;
}

export function mseSupported() {
  return !!MediaSourceImpl();
}

export function nativeHlsSupported(video = document.createElement('video')) {
  return !!video.canPlayType('application/vnd.apple.mpegurl');
}

function rangeKey(x) {
  return x ? `${x.uri}#${x.range ? `${x.range.offset}-${x.range.length}` : ''}` : '';
}

export class HlsPlayer {
  /**
   * @param {HTMLVideoElement} video
   * @param {object} opt
   * @param {string} opt.url プレイリストの URL（署名付き）
   * @param {() => Promise<string>} [opt.refreshUrl] 期限切れのとき新しい URL を返す
   * @param {boolean} [opt.startAtLiveEdge] ライブの端から始める（既定 true。録画中の EVENT は false で頭から）
   * @param {(s:{state:string, message:string}) => void} [opt.onStatus]
   */
  constructor(video, opt) {
    this.video = video;
    this.url = opt.url;
    this.refreshUrl = opt.refreshUrl || null;
    this.startAtLiveEdge = opt.startAtLiveEdge !== false;
    this.onStatus = opt.onStatus || (() => {});
    this.dead = false;
    this.ctrl = new AbortController();
    this.fetchCtrl = null;
    this.pl = null;
    this.mediaUrl = null;
    this.sb = null;
    this.ms = null;
    this.objectUrl = null;
    this.initKey = '';
    this.init = null;
    this.mime = '';
    this.offset = 0;
    this.lastDisc = null;
    this.needOffset = true;
    this.nextMsn = null;
    this.seekTarget = null;
    this.started = false;
    this.mode = null;
    this.stats = { segments: 0, bytes: 0, mime: '', latency: null };
    this.listeners = [];
    this.timer = null;
  }

  status(state, message) {
    if (!this.dead) this.onStatus({ state, message, mode: this.mode, mime: this.mime, stats: this.stats });
  }

  listen(target, type, fn) {
    target.addEventListener(type, fn);
    this.listeners.push(() => target.removeEventListener(type, fn));
  }

  async start() {
    this.status('loading', t('player.loadingPlaylist'));
    let pl = await this.loadPlaylist(true);
    // ライブで、まだセグメントが無ければ待つ
    while (!this.dead && !pl.segments.length && !pl.endList) {
      this.status('waiting', t('player.waitingSegment'));
      await sleep(1000, this.ctrl.signal);
      pl = await this.loadPlaylist(false);
    }
    if (this.dead) return;
    const container = containerOf(pl);
    if (container === 'ts' || container === 'unknown') {
      if (nativeHlsSupported(this.video)) return this.playNative();
      throw new UnsupportedError('ts', t('player.tsUnsupported'));
    }
    if (mseSupported()) return this.playMse();
    if (nativeHlsSupported(this.video)) return this.playNative();
    throw new UnsupportedError('mse', t('player.noMse'));
  }

  playNative() {
    this.mode = 'native';
    this.video.src = this.url;
    this.video.muted = true;
    this.video.play().catch(() => {});
    this.listen(this.video, 'playing', () => this.status('playing', t('player.nativePlaying')));
    this.listen(this.video, 'error', () => this.status('error', this.videoError()));
    this.status('loading', t('player.nativeStarting'));
  }

  videoError() {
    const e = this.video.error;
    return e ? t('player.mediaErrorDetail', { detail: `${e.code}${e.message ? `: ${e.message}` : ''}` }) : t('player.mediaError');
  }

  async playMse() {
    this.mode = 'mse';
    const MS = MediaSourceImpl();
    this.ms = new MS();
    if (window.ManagedMediaSource && MS === window.ManagedMediaSource) this.video.disableRemotePlayback = true;
    this.objectUrl = URL.createObjectURL(this.ms);
    this.video.src = this.objectUrl;
    await new Promise((res) => this.ms.addEventListener('sourceopen', res, { once: true }));
    if (this.dead) return;
    this.listen(this.video, 'seeking', () => this.onSeeking());
    this.listen(this.video, 'error', () => this.status('error', this.videoError()));
    this.listen(this.video, 'playing', () => this.status('playing', this.live() ? t('player.livePlaying') : t('player.playing')));
    this.listen(this.video, 'waiting', () => this.status('waiting', t('player.buffering')));
    this.timer = setInterval(() => this.watchdog(), 500);
    this.pump().catch((e) => {
      if (!this.dead) this.status('error', e.message || String(e));
    });
  }

  live() {
    return !!this.pl && !this.pl.endList;
  }

  async fetchBytes(item, kind) {
    this.fetchCtrl = new AbortController();
    const onAbort = () => this.fetchCtrl && this.fetchCtrl.abort();
    this.ctrl.signal.addEventListener('abort', onAbort, { once: true });
    try {
      const headers = {};
      if (item.range) headers.Range = `bytes=${item.range.offset}-${item.range.offset + item.range.length - 1}`;
      const res = await fetch(item.uri, { headers, signal: this.fetchCtrl.signal, cache: kind === 'init' ? 'default' : 'no-store', credentials: 'omit' });
      if (!res.ok) throw new HttpError(res.status, item.uri);
      const buf = new Uint8Array(await res.arrayBuffer());
      this.stats.bytes += buf.byteLength;
      return buf;
    } finally {
      this.ctrl.signal.removeEventListener('abort', onAbort);
    }
  }

  async loadPlaylist(first) {
    for (let attempt = 0; ; attempt += 1) {
      if (this.dead) throw new Error('stopped');
      try {
        const res = await fetch(this.mediaUrl || this.url, { cache: 'no-store', signal: this.ctrl.signal, credentials: 'omit' });
        if (!res.ok) throw new HttpError(res.status, this.url);
        let pl = parsePlaylist(await res.text(), res.url || this.url);
        if (pl.master) {
          if (!pl.variants.length) throw new Error(t('player.noVariant'));
          this.mediaUrl = pl.variants[0].uri;
          const r2 = await fetch(this.mediaUrl, { cache: 'no-store', signal: this.ctrl.signal, credentials: 'omit' });
          if (!r2.ok) throw new HttpError(r2.status, this.mediaUrl);
          pl = parsePlaylist(await r2.text(), r2.url || this.mediaUrl);
        }
        this.pl = pl;
        return pl;
      } catch (e) {
        if (this.dead || e.name === 'AbortError') throw new Error('stopped');
        if (e.status === 403 && this.refreshUrl) {
          this.status('loading', t('player.urlExpired'));
          this.url = await this.refreshUrl();
          this.mediaUrl = null;
          continue;
        }
        if (e.status === 404) this.status('waiting', t('player.noLive'));
        else this.status('waiting', t('player.playlistRetry', { reason: e.message }));
        if (first && attempt >= 2 && e.status !== 404) throw e;
        await sleep(Math.min(1000 * (attempt + 1), 5000), this.ctrl.signal);
      }
    }
  }

  bufferedAhead() {
    const v = this.video;
    const b = v.buffered;
    for (let i = 0; i < b.length; i += 1) {
      if (b.start(i) <= v.currentTime + 0.25 && v.currentTime < b.end(i)) return b.end(i) - v.currentTime;
    }
    return 0;
  }

  bufferedEnd() {
    const b = this.sb ? this.sb.buffered : this.video.buffered;
    return b.length ? b.end(b.length - 1) : null;
  }

  idle() {
    if (!this.sb || !this.sb.updating) return Promise.resolve();
    return new Promise((res) => this.sb.addEventListener('updateend', res, { once: true }));
  }

  async append(data) {
    for (let attempt = 0; ; attempt += 1) {
      await this.idle();
      try {
        this.sb.appendBuffer(data);
        await new Promise((res, rej) => {
          const ok = () => { cleanup(); res(); };
          const bad = () => {
            cleanup();
            // MediaSource は閉じるので、再試行せずに止める（理由は video.error に出る）
            setTimeout(() => {
              const detail = this.video.error && this.video.error.message ? `: ${this.video.error.message.slice(0, 200)}` : '';
              rej(new UnsupportedError('decode', t('player.appendFailed', { detail })));
            }, 0);
          };
          const cleanup = () => { this.sb.removeEventListener('updateend', ok); this.sb.removeEventListener('error', bad); };
          this.sb.addEventListener('updateend', ok);
          this.sb.addEventListener('error', bad);
        });
        return;
      } catch (e) {
        if (e.name === 'QuotaExceededError' && attempt < 3) {
          await this.evict(5);
          await sleep(500, this.ctrl.signal);
          continue;
        }
        throw e;
      }
    }
  }

  /** 再生位置より keep 秒より前を捨てる。 */
  async evict(keep) {
    if (!this.sb) return;
    const b = this.sb.buffered;
    if (!b.length) return;
    const cut = this.video.currentTime - keep;
    if (cut > b.start(0) + 1) {
      await this.idle();
      this.sb.remove(b.start(0), cut);
      await this.idle();
    }
  }

  /** 初期化セグメントが変わるなら読む（追加は appendInit で、最初のメディアセグメントを見てから）。 */
  async loadInit(seg) {
    if (!seg.map) throw new UnsupportedError('ts', t('player.noMap'));
    const key = rangeKey(seg.map);
    if (key === this.initKey) return null;
    const data = await this.fetchBytes(seg.map, 'init');
    return { key, data, init: parseInit(data) };
  }

  async appendInit(pending, firstSegment) {
    let { data } = pending;
    const { key, init } = pending;
    // 幅・高さが 0 の初期化セグメント（AV1 の Enhanced RTMP など）を補正する
    const fixed = fixZeroSize(data, init, firstSegment);
    if (fixed) {
      data = fixed;
      this.stats.patchedInit = true;
    }
    const mime = mimeFor(init);
    const MS = MediaSourceImpl();
    if (!MS.isTypeSupported(mime)) {
      throw new UnsupportedError('codec', t('player.codecUnsupported', { mime }));
    }
    if (!this.sb) {
      this.sb = this.ms.addSourceBuffer(mime);
      this.sb.mode = 'segments';
    } else if (mime !== this.mime) {
      await this.idle();
      if (typeof this.sb.changeType === 'function') this.sb.changeType(mime);
      else throw new UnsupportedError('codec', t('player.codecChanged'));
    }
    this.mime = mime;
    this.stats.mime = mime;
    await this.append(data);
    this.init = init;
    this.initKey = key;
    this.needOffset = true;
  }

  setDuration() {
    if (!this.pl || !this.pl.endList || !this.pl.segments.length || this.ms.readyState !== 'open') return;
    const last = this.pl.segments[this.pl.segments.length - 1];
    const d = last.start + last.duration;
    if (!this.sb.updating && Math.abs((this.ms.duration || 0) - d) > 0.5) {
      try { this.ms.duration = d; } catch { /* 追加中など */ }
    }
  }

  async appendSegment(seg) {
    const pending = await this.loadInit(seg);
    const data = await this.fetchBytes(seg, 'media');
    if (pending) await this.appendInit(pending, data);
    const tf = firstDecodeTime(data, this.init);
    await this.idle();
    if (tf !== null) {
      if (!this.startAtLiveEdge || !this.live()) {
        // VOD・録画中: プレイリストの時刻に合わせる（不連続・シーク・初期化の変更のとき）
        if (this.needOffset || seg.disc !== this.lastDisc) this.offset = seg.start - tf;
      } else if (this.lastDisc === null) {
        this.offset = 0;
      } else if (this.needOffset || seg.disc !== this.lastDisc) {
        // ライブの不連続: 今のバッファの終わりにつなぐ（つながっていればそのまま）
        const end = this.bufferedEnd();
        if (end !== null && Math.abs(tf + this.offset - end) > 1) this.offset = end - tf;
      }
      if (this.sb.timestampOffset !== this.offset) this.sb.timestampOffset = this.offset;
    }
    this.lastDisc = seg.disc;
    this.needOffset = false;
    await this.append(data);
    this.stats.segments += 1;
    this.setDuration();
    if (!this.started) {
      this.started = true;
      const b = this.sb.buffered;
      if (b.length && (this.video.currentTime < b.start(0) || this.video.currentTime > b.end(0))) {
        this.video.currentTime = b.start(0) + 0.05;
      }
      this.video.muted = true;
      this.video.play().catch(() => this.status('paused', t('player.autoplayBlocked')));
    }
  }

  startMsn(pl) {
    if (this.startAtLiveEdge && !pl.endList) {
      const i = Math.max(0, pl.segments.length - 3);
      return pl.segments[i].msn;
    }
    return pl.segments[0].msn;
  }

  async pump() {
    let failures = 0;
    this.nextMsn = this.startMsn(this.pl);
    while (!this.dead) {
      const pl = this.pl;
      if (this.seekTarget !== null) {
        const t = this.seekTarget;
        this.seekTarget = null;
        const seg = pl.segments.find((s) => t >= s.start && t < s.start + s.duration) || pl.segments[pl.segments.length - 1];
        if (seg) {
          this.nextMsn = seg.msn;
          this.needOffset = true;
        }
      }
      const first = pl.segments.length ? pl.segments[0].msn : 0;
      const seg = pl.segments.find((s) => s.msn === this.nextMsn);
      if (!seg) {
        if (this.live()) {
          if (this.nextMsn < first) {
            // 窓から落ちた: 端へ飛ぶ
            this.nextMsn = this.startMsn(pl);
            this.needOffset = true;
            continue;
          }
          await sleep(Math.max(500, (pl.targetDuration * 1000) / 2), this.ctrl.signal);
          if (this.dead) return;
          await this.loadPlaylist(false);
          continue;
        }
        // VOD の終わり
        if (this.ms.readyState === 'open' && this.nextMsn > pl.segments[pl.segments.length - 1].msn) {
          await this.idle();
          if (this.seekTarget === null && this.ms.readyState === 'open') {
            try { this.ms.endOfStream(); } catch { /* 何もしない */ }
            this.status('ended', t('player.ended'));
          }
        }
        await sleep(300, this.ctrl.signal);
        continue;
      }
      const maxAhead = this.live() ? Math.max(pl.targetDuration * 4, 8) : 30;
      if (this.started && this.bufferedAhead() > maxAhead) {
        await sleep(300, this.ctrl.signal);
        continue;
      }
      try {
        await this.evict(this.live() ? 30 : 60);
        await this.appendSegment(seg);
        failures = 0;
        this.nextMsn = seg.msn + 1;
      } catch (e) {
        if (this.dead || e.name === 'AbortError') {
          if (this.dead) return;
          continue; // シークで fetch を止めた
        }
        if (e instanceof UnsupportedError) throw e;
        if (e.status === 403 && this.refreshUrl) {
          this.url = await this.refreshUrl();
          this.mediaUrl = null;
          await this.loadPlaylist(false);
          continue;
        }
        if (e.status === 404 && this.live()) {
          this.nextMsn = seg.msn + 1; // 窓から消えた
          continue;
        }
        failures += 1;
        if (failures > 5) throw e;
        this.status('waiting', t('player.segmentRetry', { reason: e.message }));
        await sleep(1000 * failures, this.ctrl.signal);
      }
    }
  }

  onSeeking() {
    const t = this.video.currentTime;
    const b = this.video.buffered;
    for (let i = 0; i < b.length; i += 1) if (t >= b.start(i) && t < b.end(i)) return;
    if (this.live() && this.startAtLiveEdge) return;
    this.seekTarget = t;
    if (this.fetchCtrl) this.fetchCtrl.abort();
  }

  watchdog() {
    const v = this.video;
    if (this.dead || !this.sb) return;
    const b = v.buffered;
    if (!b.length) return;
    // 小さな穴を飛び越える
    let inRange = false;
    for (let i = 0; i < b.length; i += 1) if (v.currentTime >= b.start(i) && v.currentTime < b.end(i) - 0.1) inRange = true;
    if (!inRange && !v.seeking) {
      for (let i = 0; i < b.length; i += 1) {
        if (b.start(i) > v.currentTime && b.start(i) - v.currentTime < 5) {
          v.currentTime = b.start(i) + 0.05;
          break;
        }
      }
    }
    if (this.live() && this.startAtLiveEdge) {
      const end = b.end(b.length - 1);
      const td = this.pl.targetDuration;
      this.stats.latency = end - v.currentTime;
      // 遅れすぎたら端へ寄せる
      if (end - v.currentTime > td * 4 + 2 && !v.paused) v.currentTime = Math.max(b.start(b.length - 1), end - td * 1.5);
    }
  }

  destroy() {
    if (this.dead) return;
    this.dead = true;
    this.ctrl.abort();
    clearInterval(this.timer);
    for (const off of this.listeners) off();
    this.listeners = [];
    try { this.video.pause(); } catch { /* 何もしない */ }
    this.video.removeAttribute('src');
    try { this.video.load(); } catch { /* 何もしない */ }
    if (this.objectUrl) URL.revokeObjectURL(this.objectUrl);
  }
}
