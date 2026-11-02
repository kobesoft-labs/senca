// 画面の配線: 入力の取得（カメラ・マイク・画面）、設定の読み取り、WHIP の開始・停止、統計の表示。

import { ConfigError, parseIngest } from './config.js';
import { StatsMeter } from './stats.js';
import { PublishOptions, WhipClient } from './whip.js';

const $ = (id) => document.getElementById(id);
const STATE_TEXT = {
  idle: '待機中',
  connecting: '接続中…',
  connected: '送信中',
  reconnecting: 'つなぎ直し中…',
  stopped: '停止',
  failed: '失敗',
};
// 秘密ではない設定だけを覚えておく（トークン・設定文字列は保存しない）
const PREF_KEYS = ['source', 'resolution', 'fps', 'vbr', 'abr', 'codec', 'simulcast', 'screen-audio'];

let client = null;
let stream = null;
let statsTimer = null;

function showState(state, message = '') {
  const el = $('state');
  el.textContent = STATE_TEXT[state] || state;
  el.className = `state ${state}`;
  $('message').textContent = message;
}

function savePrefs() {
  const p = {};
  for (const k of PREF_KEYS) {
    const el = $(k);
    p[k] = el.type === 'checkbox' ? el.checked : el.value;
  }
  try {
    localStorage.setItem('senca-sender-prefs', JSON.stringify(p));
  } catch {
    // 保存できない環境では覚えない
  }
}

function loadPrefs() {
  let p = {};
  try {
    p = JSON.parse(localStorage.getItem('senca-sender-prefs') || '{}');
  } catch {
    p = {};
  }
  for (const k of PREF_KEYS) {
    if (!(k in p)) continue;
    const el = $(k);
    if (el.type === 'checkbox') el.checked = !!p[k];
    else if ([...el.options].some((o) => o.value === p[k])) el.value = p[k];
  }
}

async function listDevices() {
  const devs = await navigator.mediaDevices.enumerateDevices();
  for (const [sel, kind, none] of [
    [$('camera'), 'videoinput', 'カメラが見つかりません'],
    [$('mic'), 'audioinput', 'マイクが見つかりません'],
  ]) {
    const cur = sel.value;
    sel.replaceChildren();
    const list = devs.filter((d) => d.kind === kind);
    if (kind === 'audioinput') sel.append(new Option('なし', ''));
    // 許可の前は deviceId が空なので「既定の機器」（'*'）として扱う
    list.forEach((d, i) =>
      sel.append(new Option(d.label || `${kind === 'videoinput' ? 'カメラ' : 'マイク'} ${i + 1}`, d.deviceId || '*')),
    );
    if (!list.length && kind === 'videoinput') sel.append(new Option(none, ''));
    // 利用者が選んだものは残す。まだ選んでいなければ最初の機器（マイクは「なし」ではなく最初のマイク）
    if (sel.dataset.touched && [...sel.options].some((o) => o.value === cur)) sel.value = cur;
    else if (list.length) sel.value = list[0].deviceId || '*';
  }
}

async function capture() {
  const [w, h] = $('resolution').value.split('x').map(Number);
  const fps = Number($('fps').value);
  const source = $('source').value;
  const micId = $('mic').value;
  const tracks = [];
  if (source === 'camera') {
    const s = await navigator.mediaDevices.getUserMedia({
      video: {
        deviceId: $('camera').value && $('camera').value !== '*' ? { exact: $('camera').value } : undefined,
        width: { ideal: w },
        height: { ideal: h },
        frameRate: { ideal: fps },
      },
    });
    tracks.push(...s.getVideoTracks());
  } else if (source === 'screen') {
    const s = await navigator.mediaDevices.getDisplayMedia({
      video: { width: { ideal: w }, height: { ideal: h }, frameRate: { ideal: fps } },
      audio: $('screen-audio').checked,
    });
    tracks.push(...s.getTracks());
    // 画面の共有をブラウザの操作で止めたら送信も止める
    s.getVideoTracks()[0]?.addEventListener('ended', () => stop());
  }
  const hasAudio = tracks.some((t) => t.kind === 'audio');
  if (micId && !hasAudio) {
    const s = await navigator.mediaDevices.getUserMedia({
      audio: { deviceId: micId === '*' ? undefined : { exact: micId }, echoCancellation: true, noiseSuppression: true },
    });
    tracks.push(...s.getAudioTracks());
  }
  if (!tracks.length) throw new Error('送る映像・音声がありません');
  for (const t of tracks) if (t.kind === 'video') t.contentHint = source === 'screen' ? 'detail' : 'motion';
  return new MediaStream(tracks);
}

function fmtKbps(k) {
  return k >= 1000 ? `${(k / 1000).toFixed(2)} Mbps` : `${Math.round(k)} kbps`;
}

function startStats() {
  const meter = new StatsMeter();
  const LIMIT = { bandwidth: '帯域', cpu: 'CPU', other: 'その他' };
  statsTimer = setInterval(async () => {
    if (!client?.pc) return;
    const s = await meter.sample(client.pc).catch(() => null);
    if (!s) return;
    $('st-video').textContent = `${fmtKbps(s.videoKbps)}${s.layers > 1 ? `（${s.layers} 層）` : ''}`;
    $('st-audio').textContent = fmtKbps(s.audioKbps);
    $('st-res').textContent = s.width ? `${s.width}×${s.height}` : '—';
    $('st-fps').textContent = s.fps ? s.fps.toFixed(0) : '—';
    $('st-rtt').textContent = s.rttMs === null ? '—' : `${Math.round(s.rttMs)} ms`;
    $('st-limit').textContent = s.limitation ? LIMIT[s.limitation] || s.limitation : 'なし';
  }, 1000);
}

function setBusy(busy) {
  $('start').disabled = busy;
  $('stop').disabled = !busy;
  for (const el of $('form').querySelectorAll('input, select, textarea')) el.disabled = busy;
}

async function start(ev) {
  ev.preventDefault();
  let cfg;
  try {
    cfg = parseIngest($('config').value);
  } catch (e) {
    showState('failed', e instanceof ConfigError ? e.message : String(e));
    return;
  }
  const token = cfg.token || $('token').value.trim();
  savePrefs();
  setBusy(true);
  try {
    stream = await capture();
    $('preview').srcObject = stream;
    // 機器の名前は許可の後に見えるようになる
    listDevices().catch(() => {});
    client = new WhipClient({ url: cfg.url, token });
    client.addEventListener('state', (e) => {
      showState(e.detail.state, e.detail.message);
      if (e.detail.state === 'failed') stop();
    });
    await client.publish(
      stream,
      new PublishOptions({
        videoBitrateKbps: Number($('vbr').value),
        audioBitrateKbps: Number($('abr').value),
        simulcast: $('simulcast').checked,
        codec: $('codec').value,
      }),
    );
    startStats();
  } catch (e) {
    const msg = e.name === 'NotAllowedError' ? 'カメラ・マイク・画面の利用が許可されませんでした' : e.message || String(e);
    await stop();
    showState('failed', msg);
  }
}

async function stop() {
  clearInterval(statsTimer);
  statsTimer = null;
  const c = client;
  client = null;
  if (c) await c.stop().catch(() => {});
  if (stream) for (const t of stream.getTracks()) t.stop();
  stream = null;
  $('preview').srcObject = null;
  setBusy(false);
  if ($('state').classList.contains('failed')) return;
  showState('stopped');
}

function init() {
  loadPrefs();
  const q = new URLSearchParams(location.search).get('config');
  if (q) $('config').value = q;
  $('form').addEventListener('submit', start);
  $('stop').addEventListener('click', () => stop());
  for (const id of ['camera', 'mic']) $(id).addEventListener('change', (e) => (e.target.dataset.touched = '1'));
  $('refresh').addEventListener('click', () => listDevices().catch((e) => showState('failed', e.message)));
  if (!navigator.mediaDevices?.getUserMedia) {
    showState('failed', 'このブラウザ（または http の接続）ではカメラを使えません。https で開いてください');
    $('start').disabled = true;
    return;
  }
  if (!navigator.mediaDevices.getDisplayMedia) $('source').querySelector('[value=screen]').disabled = true;
  listDevices().catch(() => {});
  navigator.mediaDevices.addEventListener?.('devicechange', () => listDevices().catch(() => {}));
  window.addEventListener('pagehide', () => {
    // 閉じるときはセッションを消す（届かなくても相手は時間切れで消す）
    if (client?.resource) {
      fetch(client.resource, { method: 'DELETE', headers: client.headers(), keepalive: true }).catch(() => {});
    }
  });
}

init();
