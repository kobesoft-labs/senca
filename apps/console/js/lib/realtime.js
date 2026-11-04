// 部屋とメディアパイプラインの画面で使う、画面に依存しない計算（tools/interop/console.sh の C で試す）。
//
// - 部屋の状態（配置と参加者の数）、設定のバッジ、設定の差分（JSON Merge Patch）
// - 参加者の名前・トラックの要約（REST の参加者＋統計＋クライアントが送った getStats の要約）
// - 参加の手順のコード（SDK・curl）
// - メディアパイプラインの用途（録画・合成して配信・書き出し・文字起こし）→ 作成の本文、作られるものの一覧

export const ROOM_ID_RE = /^[A-Za-z0-9_.:@-]{1,128}$/;
export const MP_NAME_RE = /^[A-Za-z0-9_-][A-Za-z0-9_.-]{0,39}$/;

export const ROOM_DEFAULTS = Object.freeze({
  max_participants: 50, e2ee: false, video_codec: 'vp8', audio_codec: 'opus', recording: 'none', data_labels: ['#*'], empty_timeout_secs: 60, cascade: false,
});

/**
 * 部屋の表示の状態。
 * count は今の参加者の数（読めなければ null）。
 * @returns {'active'|'empty'|'pending'|'unschedulable'|'unknown'}
 */
export function roomState(room, count) {
  const st = room && room.placement_status ? room.placement_status.state : null;
  if (st === 'unschedulable') return 'unschedulable';
  if (st === 'pending' || st === 'not_scheduled') return 'pending';
  if (count === null || count === undefined) return 'unknown';
  return count > 0 ? 'active' : 'empty';
}

/** 設定の特徴（一覧・詳細のバッジ）。既定のままのものは出さない。 */
export function roomFeatures(settings = {}) {
  const f = [];
  if (settings.e2ee) f.push('e2ee');
  if (settings.cascade) f.push('cascade');
  if (settings.spotlight) f.push('spotlight');
  if (settings.recording && settings.recording !== 'none') f.push(`recording.${settings.recording}`);
  if (settings.forwarding_filter) f.push('filter');
  if (settings.auth_webhook_url) f.push('webhook');
  return f;
}

/**
 * 設定の差分（PATCH の settings。JSON Merge Patch）。
 * 変えた項目だけを書き、消した任意の項目（spotlight など）は null にする。差が無ければ null。
 */
export function settingsPatch(cur = {}, next = {}) {
  const out = {};
  const keys = new Set([...Object.keys(cur), ...Object.keys(next)]);
  for (const k of keys) {
    const a = cur[k];
    const b = next[k];
    if (JSON.stringify(a ?? null) === JSON.stringify(b ?? null)) continue;
    out[k] = b === undefined ? null : b;
  }
  return Object.keys(out).length ? out : null;
}

const str = (v) => (typeof v === 'string' && v.trim() ? v.trim() : null);

/** 参加者の呼び名: metadata の name・display_name、client_id、connection_id の先頭。 */
export function participantName(p) {
  const m = p && p.metadata && typeof p.metadata === 'object' ? p.metadata : {};
  return str(m.name) || str(m.display_name) || str(m.displayName) || p.client_id || shortId(p.connection_id);
}

export function shortId(id) {
  const s = String(id || '');
  return s.length > 10 ? `${s.slice(0, 8)}…` : s;
}

/**
 * 参加者のトラックの要約。stats は GET /rooms/{id}/stats の participants[] の 1 つ（無ければ null）。
 * 戻り値: [{track_id, kind, codec, rids, bitrate_bps, layers:[{rid, bitrate_bps, width, height, fps}], width, height, fps}]
 */
export function trackSummary(p, stats, room) {
  const codec = (kind) => (kind === 'video' ? (room && room.settings ? room.settings.video_codec : null) : 'opus');
  const published = stats && Array.isArray(stats.published) ? stats.published : [];
  const outbound = stats && stats.client && Array.isArray(stats.client.outbound) ? stats.client.outbound : [];
  return (p.tracks || []).map((tr) => {
    const pub = published.find((x) => x.track_id === tr.track_id);
    // クライアントの outbound はトラック ID が SDK の側の ID のことがあるので、種類で寄せる
    const outs = outbound.filter((o) => o.kind === tr.kind);
    const order = (rid) => { const i = (tr.rids || []).indexOf(rid); return i < 0 ? 99 : i; };
    const layers = (pub ? pub.layers.slice().sort((a, b) => order(a.rid) - order(b.rid)) : []).map((l) => {
      const o = outs.find((x) => (x.rid || null) === (l.rid || null)) || (outs.length === 1 ? outs[0] : null);
      return { rid: l.rid, bitrate_bps: l.bitrate_bps, width: o ? o.width ?? null : null, height: o ? o.height ?? null : null, fps: o ? o.fps ?? null : null };
    });
    const sum = layers.reduce((a, l) => a + (l.bitrate_bps || 0), 0);
    const top = layers.filter((l) => l.width).sort((a, b) => b.width - a.width)[0]
      || (outs.filter((o) => o.width).sort((a, b) => b.width - a.width)[0] ?? null);
    return {
      track_id: tr.track_id, kind: tr.kind, codec: codec(tr.kind), rids: tr.rids || null,
      bitrate_bps: pub ? sum : null, layers,
      width: top ? top.width : null, height: top ? top.height : null, fps: top ? top.fps ?? null : null,
    };
  });
}

/** 受信の損失（クライアントの inbound の packetsLost の合計）。報告が無ければ null。 */
export function packetsLost(stats) {
  const inbound = stats && stats.client && Array.isArray(stats.client.inbound) ? stats.client.inbound : null;
  if (!inbound) return null;
  return inbound.reduce((a, i) => a + (Number.isFinite(i.lost) ? i.lost : 0), 0);
}

/** 部屋の送受信の合計（統計から）。 */
export function roomTotals(stats) {
  const ps = stats && Array.isArray(stats.participants) ? stats.participants : [];
  let inBps = 0;
  let outBps = 0;
  for (const p of ps) {
    for (const t of p.published || []) for (const l of t.layers || []) inBps += l.bitrate_bps || 0;
    outBps += p.send_bitrate_bps || 0;
  }
  return { inBps, outBps };
}

/** シグナリングの URL（部屋・トークンの signaling_url。無ければこのページのホストの /signaling）。 */
export function signalingUrl(fromApi, loc = globalThis.location) {
  if (fromApi) return fromApi;
  if (!loc) return null;
  return `${loc.protocol === 'https:' ? 'wss' : 'ws'}://${loc.host}/signaling`;
}

const q = (v) => JSON.stringify(v);

/** ブラウザー SDK で参加するコード（トークンを埋め込んだ、すぐ試せる形）。 */
export function sdkSnippet({ url, room, token, role = 'sendrecv' }) {
  const send = role !== 'recvonly';
  return [
    "import { Senca } from '@senca/sdk';",
    '',
    send ? 'const camera = await navigator.mediaDevices.getUserMedia({ video: true, audio: true });' : null,
    'const session = await Senca.connect({',
    `  url: ${q(url)},`,
    `  room: ${q(room)},`,
    `  token: ${q(token || '<TOKEN>')},`,
    role !== 'sendrecv' ? `  role: ${q(role)},` : null,
    send ? '  publish: { stream: camera, simulcast: true },' : null,
    '});',
    '',
    'for await (const track of session.remoteTracks()) {',
    "  document.body.append(track.attach(document.createElement(track.kind === 'video' ? 'video' : 'audio')));",
    '}',
  ].filter((l) => l !== null).join('\n');
}

/** アプリのサーバーがトークンを発行する curl（管理 API のトークンは環境変数のまま）。 */
export function tokenCurl({ apiBase, room, body }) {
  return [
    `curl -s -X POST "${apiBase}/rooms/${encodeURIComponent(room)}/tokens" \\`,
    '  -H "Authorization: Bearer $SENCA_TOKEN" -H \'Content-Type: application/json\' \\',
    `  -d '${JSON.stringify(body)}'`,
  ].join('\n');
}

/** 参加トークンの本文（画面の入力から）。 */
export function tokenBody({ role = 'sendrecv', clientId, name, ttl = 600, publish = ['audio', 'video'], subscribe = true, data = ['#*'] }) {
  const b = { role, ttl_secs: Number(ttl) || 600 };
  if (clientId && clientId.trim()) b.client_id = clientId.trim();
  if (name && name.trim()) b.metadata = { name: name.trim() };
  const allPub = publish.length === 2 && publish.includes('audio') && publish.includes('video');
  const allData = data.length === 1 && data[0] === '#*';
  if (!allPub || !subscribe || !allData) b.permissions = { publish, subscribe, data };
  return b;
}

/** 文字の並び（改行・カンマ・空白区切り）→ 重複の無い配列。 */
export function splitList(text) {
  return [...new Set(String(text || '').split(/[\n,\s]+/).map((x) => x.trim()).filter(Boolean))];
}

/** メディアパイプラインの名前の候補（部屋の ID と用途から。英数字と -_. で 40 文字まで）。 */
export function pipelineName(room, use) {
  const base = String(room || 'room').replace(/[^A-Za-z0-9_.-]/g, '-').replace(/^[.]+/, '').slice(0, 40 - use.length - 1) || 'room';
  return `${base}-${use}`.slice(0, 40);
}

export const USE_CASES = ['record', 'broadcast', 'export', 'captions'];

/** 用途ごとの最初の値（作成の画面の初期値）。 */
export function useCaseDefaults(use) {
  const base = {
    participants: [], export_participants: true,
    composite: null, recording: null, egress: [], captions: null,
  };
  const composite = { layout: 'grid', width: 1280, height: 720, fps: 30, video_codec: 'av1', bitrate_kbps: 2500 };
  if (use === 'record') return { ...base, recording: { participants: true, composite: false } };
  if (use === 'broadcast') return { ...base, composite, egress: [{ protocol: 'rtmp_push', url: '' }] };
  if (use === 'captions') return { ...base, captions: {} };
  return base; // export
}

/**
 * 作成の本文（POST /media-pipelines）。空の配列・null の設定は省く（API の既定に任せる）。
 * recording は composite が無いとき composite: false を明示する（省くと 400）。
 */
export function pipelineBody(v) {
  const b = { name: v.name, room: v.room };
  if (v.participants && v.participants.length) b.participants = v.participants;
  if (v.export_participants === false) b.export_participants = false;
  if (v.composite) b.composite = { ...v.composite };
  if (v.recording) {
    b.recording = { participants: !!v.recording.participants, composite: !!(v.composite && v.recording.composite) };
    if (v.recording.retention_days) b.recording.retention_days = Number(v.recording.retention_days);
  }
  const egress = (v.egress || []).filter((e) => e && (e.protocol === 'paas' ? e.provider : e.url));
  if (egress.length) {
    b.egress = egress.map((e) => {
      const o = { protocol: e.protocol };
      if (e.protocol === 'paas') o.provider = e.provider;
      else o.url = e.url.trim();
      if (e.credential_ref) o.credential_ref = e.credential_ref;
      return o;
    });
  }
  if (v.captions) b.captions = v.captions;
  if (v.desired_state === 'stopped') b.desired_state = 'stopped';
  return b;
}

/**
 * 作成の前の検査（画面の言葉の鍵を返す。問題が無ければ空）。
 * room は選んだ部屋（e2ee の確認に使う。分からなければ null）。
 */
export function pipelineProblems(b, room) {
  const p = [];
  if (!MP_NAME_RE.test(b.name || '')) p.push('name');
  if (!b.room) p.push('room');
  if (b.egress && b.egress.length && !b.composite) p.push('egressNeedsComposite');
  if (b.export_participants === false && ((b.recording && b.recording.participants) || b.captions)) p.push('exportNeeded');
  if (room && room.settings && room.settings.e2ee && (b.composite || b.captions)) p.push('e2ee');
  if (!b.composite && !b.recording && !b.captions && b.export_participants === false) p.push('nothing');
  return p;
}

/**
 * 作られるもの（説明の行）。[{kind, ...}] を返し、画面が言葉にする。
 * kind: participantStreams・composite・recording・egress・captions
 */
export function pipelineProducts(b) {
  const out = [];
  out.push({ kind: 'participantStreams', who: b.participants && b.participants.length ? b.participants : null, artifact: b.export_participants !== false });
  if (b.composite) out.push({ kind: 'composite', ...b.composite });
  if (b.recording) {
    if (b.recording.participants && b.export_participants !== false) out.push({ kind: 'recording', target: 'participants', days: b.recording.retention_days || null });
    if (b.recording.composite && b.composite) out.push({ kind: 'recording', target: 'composite', days: b.recording.retention_days || null });
  }
  for (const e of b.egress || []) out.push({ kind: 'egress', protocol: e.protocol, url: e.url || null, provider: e.provider || null });
  if (b.captions) out.push({ kind: 'captions', language: b.captions.language || null, targets: b.captions.targets || [] });
  return out;
}

/** 用途の推定（一覧の説明に使う）。 */
export function pipelineUse(mp) {
  if (mp.egress && mp.egress.length) return 'broadcast';
  if (mp.captions) return 'captions';
  if (mp.recording) return 'record';
  return 'export';
}

/**
 * メディアパイプラインの状態（一覧・詳細）。
 * @returns {'running'|'idle'|'stopped'|'waiting'}
 *   running = 書き出している、idle = 動かしているが誰も送っていない、waiting = 状態がまだ無い（作成直後・別のノード）
 */
export function pipelineState(mp) {
  if (mp.desired_state === 'stopped') return 'stopped';
  if (!mp.status) return 'waiting';
  return mp.status.participants && mp.status.participants.length ? 'running' : 'idle';
}

/**
 * 部屋の出来事（SSE）→ 時系列の 1 行。{at, icon, key, params, tone} を返す（key は rooms.ev.* の言葉）。知らない種類は null。
 * nameOf(connection_id, client_id) は参加者の呼び名（退出の後も引けるように画面が覚えておく）。
 */
export function roomEventItem(ev, nameOf = (c, cl) => cl || shortId(c)) {
  const d = (ev && ev.data) || {};
  const body = d.data || {};
  const at = d.occurred_at || null;
  switch (ev.type) {
    case 'room.connection.created': {
      const p = body.participant || {};
      return { at, icon: 'user-plus', tone: 'ok', key: 'rooms.ev.joined', params: { who: participantName(p) || nameOf(p.connection_id, p.client_id) } };
    }
    case 'room.connection.destroyed':
      return {
        at, icon: body.reason === 'kicked' ? 'user-x' : 'logout', tone: body.reason === 'kicked' ? 'warn' : null,
        key: 'rooms.ev.left', params: { who: nameOf(body.connection_id, body.client_id), reason: body.reason || '', ms: body.duration_ms ?? 0 },
      };
    case 'room.connection.updated':
      return { at, icon: 'edit', tone: null, key: 'rooms.ev.updated', params: { who: nameOf(body.connection_id) } };
    case 'room.track.published': {
      const tr = body.track || {};
      return { at, icon: tr.kind === 'audio' ? 'mic' : 'send', tone: null, key: tr.kind === 'audio' ? 'rooms.ev.pubAudio' : 'rooms.ev.pubVideo', params: { who: nameOf(tr.connection_id), layers: (tr.rids || []).length } };
    }
    case 'room.track.unpublished':
      return { at, icon: 'stop', tone: null, key: 'rooms.ev.unpub', params: { who: nameOf(body.connection_id) } };
    case 'room.spotlight.changed':
      return { at, icon: 'zap', tone: null, key: 'rooms.ev.spotlight', params: { who: (body.focused || []).map((c) => nameOf(c)).join(', ') || '—' } };
    case 'room.started':
      return { at, icon: 'play', tone: 'ok', key: 'rooms.ev.started', params: {} };
    case 'room.stopped':
      return { at, icon: 'stop', tone: null, key: 'rooms.ev.stopped', params: { reason: body.reason || '' } };
    case 'room.updated':
      return { at, icon: 'settings', tone: null, key: 'rooms.ev.settings', params: {} };
    default:
      return null;
  }
}

/** YouTube などの行き先の型（URL と、鍵を入れる秘密値の案内）。 */
export const EGRESS_PRESETS = [
  { id: 'youtube', protocol: 'rtmp_push', url: 'rtmp://a.rtmp.youtube.com/live2' },
  { id: 'twitch', protocol: 'rtmp_push', url: 'rtmp://live.twitch.tv/app' },
  { id: 'rtmp', protocol: 'rtmp_push', url: '' },
  { id: 'srt', protocol: 'srt_push', url: '' },
  { id: 'paas', protocol: 'paas', url: '' },
];
