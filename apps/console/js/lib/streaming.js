// 配信の画面で共通に使う小さな道具: 状態の読み分け・受信中の映像と音声の要約・ビットレートの履歴・今の画の小さな画像。
//
// - ビットレートの履歴は API に無いので、画面が GET /streams・/streams/{id} を読むたびにこのタブのメモリに溜める（5 分ぶん）。
//   一覧と詳細で同じ履歴を使うので、一覧から詳細へ移っても線が途切れない。
// - 今の画（GET /streams/{id}/snapshot）は復号が要るので重い。30 秒は使い回し、作れなかったら 60 秒は取り直さない。

import { streams, recordings } from '../api/resources.js';
import { t } from '../i18n/index.js';
import { fmtBps } from './format.js';

/** 配信の状態（観測した状態と、望む状態）→ 表示の状態（live・waiting・stopped）。 */
export function streamState(s) {
  if (s.observed_state === 'live') return 'live';
  if (s.observed_state === 'stopped' || s.desired_state === 'stopped') return 'stopped';
  return 'waiting';
}

const CODEC_NAMES = {
  h264: 'H.264', avc: 'H.264', avc1: 'H.264', h265: 'H.265', hevc: 'H.265', hvc1: 'H.265', av1: 'AV1', av01: 'AV1', vp9: 'VP9', vp8: 'VP8',
  aac: 'AAC', opus: 'Opus', mp3: 'MP3', pcm: 'PCM', g711: 'G.711', ac3: 'AC-3', eac3: 'E-AC-3',
};
const AUDIO = new Set(['aac', 'opus', 'mp3', 'pcm', 'g711', 'ac3', 'eac3', 'pcma', 'pcmu', 'flac']);

/** コーデックの表示名（h264 → H.264）。 */
export function codecName(c) {
  if (!c) return '?';
  const k = String(c).toLowerCase();
  return CODEC_NAMES[k] || String(c).toUpperCase();
}

/**
 * 受信中の入力（live）の要約。トラックの形は決まっていないので、知っている項目だけ読む。
 * @returns {{video: object|null, audio: object|null, transport: string, text: string}|null}
 */
export function liveSummary(s) {
  const live = s && s.live;
  if (!live) return null;
  const tracks = Array.isArray(live.tracks) ? live.tracks : [];
  const isAudio = (tr) => tr.kind === 'audio' || tr.type === 'audio' || AUDIO.has(String(tr.codec || '').toLowerCase()) || tr.sample_rate !== undefined || tr.channels !== undefined;
  const v = tracks.find((tr) => !isAudio(tr));
  const a = tracks.find((tr) => isAudio(tr));
  const video = v ? { codec: codecName(v.codec), width: v.width || null, height: v.height || null, fps: v.fps || v.frame_rate || null } : null;
  const audio = a ? { codec: codecName(a.codec), channels: a.channels || null, rate: a.sample_rate || null } : null;
  const parts = [String(live.transport || '').toUpperCase()];
  if (video) parts.push(video.width && video.height ? `${video.codec} ${video.width}×${video.height}` : video.codec);
  if (audio) parts.push(audio.codec);
  return { video, audio, transport: live.transport, text: parts.filter(Boolean).join(' · ') };
}

/** 受信を始めてからの時間（ミリ秒）。 */
export function uptimeMs(s) {
  if (!s || !s.live || !s.live.started_at) return null;
  return Math.max(0, Date.now() - new Date(s.live.started_at).getTime());
}

/** 視聴者の数（このノード）。viewers が無いノードでは null。 */
export function viewerCount(s) {
  const v = s && s.live && s.live.viewers;
  return v && typeof v.total === 'number' ? v.total : null;
}

// ---------------------------------------------------------------- ビットレートの履歴
const HISTORY = new Map();
const KEEP_MS = 5 * 60_000;

/** 読んだ配信の今のビットレートを履歴に足す（同じ配信を 2 秒以内に重ねて足さない）。 */
export function recordHealth(s) {
  if (!s) return;
  const now = Date.now();
  let h = HISTORY.get(s.id);
  if (!h) { h = []; HISTORY.set(s.id, h); }
  // 受信が切れたら 0 を入れる（線が落ちて見える）
  const bps = s.live ? s.live.bitrate_bps : 0;
  const epoch = s.live ? s.live.epoch : null;
  if (h.length && now - h[h.length - 1].t < 2000) {
    h[h.length - 1] = { t: h[h.length - 1].t, bps, epoch };
  } else h.push({ t: now, bps, epoch });
  while (h.length && now - h[0].t > KEEP_MS) h.shift();
}

/** 履歴（古い順の bps）。 */
export function healthSamples(id) {
  return (HISTORY.get(id) || []).map((x) => x.bps);
}

/** 履歴から読める直近の目安（平均・最小・最大）。 */
export function healthStats(id) {
  const v = healthSamples(id).filter((x) => x > 0);
  if (!v.length) return null;
  return { avg: v.reduce((a, b) => a + b, 0) / v.length, min: Math.min(...v), max: Math.max(...v), n: v.length };
}

/** ビットレートの読み上げ用の文（「直近 5 分のビットレート、今 3.1 Mbps」）。 */
export function bitrateLabel(s) {
  return t('streams.health.sparkLabel', { now: fmtBps(s.live ? s.live.bitrate_bps : 0) });
}

// ---------------------------------------------------------------- 小さな画像（今の画・録画の代表画）
const THUMBS = new Map();

async function cachedBlob(key, ttl, failTtl, fetcher) {
  const now = Date.now();
  const c = THUMBS.get(key);
  if (c && c.pending) return c.pending;
  if (c && c.url && now - c.at < ttl) return c.url;
  if (c && c.failed && now - c.at < failTtl) return c.prev || null;
  const pending = fetcher().then(({ data }) => {
    const url = URL.createObjectURL(data);
    if (c && c.url) setTimeout(() => URL.revokeObjectURL(c.url), 10_000);
    THUMBS.set(key, { url, at: Date.now() });
    return url;
  }).catch(() => {
    THUMBS.set(key, { failed: true, at: Date.now(), prev: c && c.url ? c.url : null, url: c && c.url ? c.url : null });
    return c && c.url ? c.url : null;
  });
  THUMBS.set(key, { ...(c || {}), pending });
  const r = await pending;
  return r;
}

/** 配信中の今の画（blob の URL）。作れなければ null（理由は問わない: 一覧の飾りなので）。 */
export function liveThumb(id) {
  return cachedBlob(`s:${id}`, 30_000, 60_000, () => streams.snapshot(id));
}

/** 録画の代表画（blob の URL）。 */
export function recordingThumb(id) {
  return cachedBlob(`r:${id}`, 3600_000, 300_000, () => recordings.thumbnail(id));
}

// ---------------------------------------------------------------- 送り手の設定（作成の直後に一度だけ使う鍵の受け渡し）
const HANDOFF = new Map();

/** 「このブラウザから送る」へ鍵を渡す（メモリだけ。1 回取り出したら消える）。 */
export function handOffKey(streamId, key) {
  if (key) HANDOFF.set(streamId, key);
}

export function takeHandOffKey(streamId) {
  const k = HANDOFF.get(streamId) || null;
  HANDOFF.delete(streamId);
  return k;
}

// ---------------------------------------------------------------- URL の ?… を読み書きする（画面を作り直さない）
export function hashParams() {
  return new URLSearchParams(location.hash.split('?')[1] || '');
}

/** ?key=value を置き換える（空・null は消す）。履歴は増やさない。 */
export function setHashParams(values) {
  const [path] = location.hash.split('?');
  const p = hashParams();
  for (const [k, v] of Object.entries(values)) {
    if (v === null || v === undefined || v === '') p.delete(k);
    else p.set(k, v);
  }
  const q = p.toString();
  history.replaceState(history.state, '', `${location.pathname}${location.search}${path}${q ? `?${q}` : ''}`);
}
