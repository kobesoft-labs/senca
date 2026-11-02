// パイプライン定義（senca-pipeline のグラフ JSON、`version: 1`）と編集用のモデル。画面に依存しない（node でも読める）。
//
// グラフの形は crates/senca-pipeline/src/model.rs が正（知らない項目は拒否される）。編集用のモデルは
// 部品（位置・平らにした設定）と線（出力ポート → 入力ポート）を持ち、toGraph / fromGraph で行き来する。
//
// - 部品の種類: source・decode・board・scale・crop・compose・audio_mix・encode・snapshot_on_change・sink
// - 入力の参照は `"部品"` か `"部品.ポート"`（source は video・audio、compose は program・preview、ほかは out）
// - 場所（どのノードで動かすか）は持たない（REQ-005）。画面の位置はグラフに入れられないので、画面側が別に覚える。
// - 取り込んだグラフの、この画面で編集しない項目（compose の images、board の corners など）はそのまま残す。

export const GRAPH_VERSION = 1;

/** ポートの型。ev/ea = 符号化済みの映像・音声、v/a = 生のフレーム。raw/enc は上流が決まっていないとき。 */
export const PORT_LABEL = {
  ev: '映像（符号化済み）', ea: '音声（符号化済み）', v: '映像（生）', a: '音声（生）', raw: '生（上流しだい）', enc: '符号化済み（上流しだい）',
};

const VIDEO_CODECS = ['av1', 'vp9', 'vp8', 'h264', 'h265'];
const AUDIO_CODECS = ['opus', 'aac', 'pcma', 'pcmu'];
const ENCODE_CODECS = ['av1', 'vp9', 'vp8', 'h264', 'h265', 'opus'];
const FILTERS = ['bilinear', 'nearest', 'bicubic', 'lanczos3', 'area'];
export const TRANSITIONS = [['cut', 'カット'], ['fade', 'フェード'], ['wipe_left', 'ワイプ（左から）'], ['wipe_right', 'ワイプ（右から）']];
/** ロイヤリティのあるコーデック（royalty_free の stream が関わると使えない。REQ-007）。 */
export const ROYALTY_CODECS = new Set(['h264', 'h265', 'aac']);

const num = (path, label, extra = {}) => ({ path, label, type: 'number', ...extra });
const sel = (path, label, options, extra = {}) => ({ path, label, type: 'select', options, ...extra });
const chk = (path, label, def) => ({ path, label, type: 'check', def });

/**
 * 部品の種類。
 * - in: 入力ポート（固定）。名前付きの入力（compose・audio_mix）は部品ごとの `ins` から作る。
 * - out: 出力ポートの名前（型は outType で上流から決める）。
 * - fields: 検査器に出す設定（path はグラフの中の場所）。
 */
export const KINDS = {
  source: {
    label: '入力（stream）', color: '#2f80ed', in: [], out: ['video', 'audio'],
    fields: [
      { path: 'stream', label: 'stream', type: 'stream', required: true },
      sel('video.codec', '映像のコーデック（空 = 使わない）', ['', ...VIDEO_CODECS]),
      num('video.track', '映像のトラック番号', { min: 0, when: (p) => !!p['video.codec'] }),
      num('video.width', '映像の幅（見込み）', { min: 16, when: (p) => !!p['video.codec'] }),
      num('video.height', '映像の高さ（見込み）', { min: 16, when: (p) => !!p['video.codec'] }),
      num('video.fps', '映像の fps（見込み）', { min: 1, step: 'any', when: (p) => !!p['video.codec'] }),
      sel('audio.codec', '音声のコーデック（空 = 使わない）', ['', ...AUDIO_CODECS]),
      num('audio.track', '音声のトラック番号', { min: 0, when: (p) => !!p['audio.codec'] }),
      num('audio.sample_rate', 'サンプルレート', { min: 8000, when: (p) => !!p['audio.codec'] }),
      num('audio.channels', 'チャンネル数', { min: 1, max: 8, when: (p) => !!p['audio.codec'] }),
    ],
    defaults: { 'video.codec': 'vp9' },
  },
  decode: {
    label: '復号', color: '#8e44ad', in: [['input', ['ev', 'ea', 'enc']]], out: ['out'],
    fields: [num('max_width', '解像度の上限（幅）', { min: 16 }), num('max_height', '解像度の上限（高さ）', { min: 16 }), num('threads', 'スレッド数（0 = 自動）', { min: 0, def: 0 })],
  },
  board: {
    label: '板書の補正', color: '#16a085', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [
      sel('params.mode', '板の種類', [['whiteboard', 'ホワイトボード'], ['blackboard', '黒板']], { def: 'whiteboard' }),
      chk('params.auto_detect', '4 隅を自動で検出', false),
      num('params.redetect_frames', '検出のやり直し間隔（フレーム、0 = しない）', { min: 0, def: 0 }),
      num('params.output_width', '出力の幅', { min: 16, def: 1280 }),
      num('params.output_height', '出力の高さ', { min: 16, def: 720 }),
      chk('params.enhance', '照明むら・反射の補正と強調', true),
      num('params.stroke_gain', '線の強調（任意）', { step: 'any' }),
      num('params.chroma_gain', '色の強調（任意）', { step: 'any' }),
      chk('params.invert_to_white', '黒板を白地に反転', false),
      chk('params.clahe', '輝度の CLAHE', false),
    ],
  },
  scale: {
    label: '拡大縮小', color: '#16a085', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [num('width', '幅', { min: 16, required: true, def: 1280 }), num('height', '高さ', { min: 16, required: true, def: 720 }), sel('filter', 'フィルター', FILTERS, { def: 'bilinear' })],
  },
  crop: {
    label: '切り抜き', color: '#16a085', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [num('x', 'x', { min: 0, required: true, def: 0 }), num('y', 'y', { min: 0, required: true, def: 0 }), num('width', '幅', { min: 2, required: true, def: 640 }), num('height', '高さ', { min: 2, required: true, def: 360 })],
  },
  compose: {
    label: '合成（スイッチャー）', color: '#d35400', named: true, out: ['program', 'preview'],
    accept: ['v', 'raw'],
    fields: [
      num('canvas.width', 'キャンバスの幅', { min: 16, required: true, def: 1280 }),
      num('canvas.height', 'キャンバスの高さ', { min: 16, required: true, def: 720 }),
      num('canvas.fps_num', 'fps（分子）', { min: 1, required: true, def: 30 }),
      num('canvas.fps_den', 'fps（分母）', { min: 1, def: 1 }),
      { path: 'canvas.background', label: '地の色', type: 'color', def: '#000000' },
      { path: 'program', label: 'プログラムのシーン', type: 'scene', required: true },
      { path: 'preview', label: 'プレビューのシーン（任意）', type: 'scene' },
      sel('transition.kind', '既定のトランジション（TAKE）', TRANSITIONS, { def: 'cut' }),
      num('transition.duration_ms', 'トランジションの時間（ms）', { min: 0, def: 0 }),
      num('missing.hold_ms', '入力が切れたとき最後の画を保つ時間（ms）', { min: 0, def: 2000 }),
      { path: 'missing.slate_color', label: 'スレートの色', type: 'color', def: '#000000' },
      num('threads', '帯の並列度（0 = CPU の数）', { min: 0, def: 0 }),
    ],
  },
  audio_mix: {
    label: '音声ミキサー', color: '#d35400', named: true, out: ['out'], accept: ['a', 'raw'],
    fields: [num('master_gain_db', 'マスター（dB）', { step: 0.5, def: 0 }), chk('limiter', 'リミッター', true)],
  },
  encode: {
    label: '符号化', color: '#c0392b', in: [['input', ['v', 'a', 'raw']]], out: ['out'],
    fields: [
      sel('codec', 'コーデック', ENCODE_CODECS, { required: true, def: 'av1' }),
      num('bitrate_kbps', 'ビットレート（kbps）', { min: 1, required: true, def: 2000 }),
      num('gop', 'キーフレーム間隔（フレーム、空 = 2 秒）', { min: 1 }),
      chk('low_latency', '低遅延', true),
      sel('rate_control', '符号量の制御', ['cbr', 'vbr'], { def: 'cbr' }),
      num('width', '出力の幅（空 = 入力のまま）', { min: 16 }),
      num('height', '出力の高さ（空 = 入力のまま）', { min: 16 }),
      num('frame_ms', 'Opus のパケット長（ms）', { min: 2.5, step: 'any', when: (p) => p.codec === 'opus' }),
      num('threads', 'スレッド数（0 = 自動）', { min: 0, def: 0 }),
    ],
  },
  snapshot_on_change: {
    label: '板書の変化で静止画', color: '#7f8c8d', in: [['input', ['v', 'raw']]], out: [],
    fields: [
      sel('params.mode', '板の種類', [['whiteboard', 'ホワイトボード'], ['blackboard', '黒板']], { def: 'whiteboard' }),
      chk('params.enabled', '始めから動かす', true),
      num('params.analysis_fps', '解析の回数（毎秒、0 = 全フレーム）', { min: 0, step: 'any', def: 2 }),
      num('params.stable_frames', '落ち着いたとみなす回数', { min: 1, def: 3 }),
      num('params.change_fraction', '変化の割合', { min: 0, step: 'any', def: 0.004 }),
      num('params.jpeg_quality', 'JPEG の品質', { min: 1, max: 100, def: 85 }),
    ],
  },
  sink: {
    label: '出力（stream）', color: '#27ae60', in: [['入力 1', ['ev', 'ea', 'enc']], ['入力 2', ['ev', 'ea', 'enc']]], out: [],
    fields: [{ path: 'stream', label: '出力先の stream', type: 'stream', required: true }],
  },
};

export const OPTION_FIELDS = [
  sel('clock', '時刻の進め方', [['realtime', 'realtime（ライブ）'], ['input', 'input（入力の時刻）']], { def: 'realtime' }),
  sel('overflow', 'キューが一杯のとき', [['drop', 'drop（古いものを捨てる）'], ['block', 'block（上流を待たせる）']], { def: 'drop' }),
  num('latency_ms', '揺らぎ吸収の遅延（ms）', { min: 0, def: 100 }),
  num('missing_timeout_ms', '入力が切れたとみなす時間（ms）', { min: 1, def: 2000 }),
  num('block_timeout_ms', 'block の待ちの上限（ms）', { min: 1, def: 2000 }),
  num('input_queue_aus', '入力キュー（AU 数）', { min: 1, def: 256 }),
  num('frame_queue', 'フレームのキュー', { min: 1, def: 8 }),
  num('status_interval_ms', '状態・メーターの間隔（ms、0 = 出さない）', { min: 0, def: 100 }),
  num('commit_timeout_ms', '世代の切り替えの待ちの上限（ms）', { min: 1, def: 10000 }),
];

export const NODE_W = 180;
const PORT_STEP = 18;

/** 部品の高さ（ポートの数で伸びる）。 */
export function nodeHeight(n) {
  const k = Math.max(inPorts(n).length, outPorts(n).length);
  return Math.max(60, (k + 1) * PORT_STEP);
}

/** 入力ポート: [{name, accept}] */
export function inPorts(n) {
  const k = KINDS[n.type];
  if (k.named) return (n.ins || []).map((i) => ({ name: i.name, accept: k.accept }));
  return k.in.map(([name, accept]) => ({ name, accept }));
}

/** 出力ポート: [{name}] */
export function outPorts(n) {
  return KINDS[n.type].out.map((name) => ({ name }));
}

const ID_RE = /^[A-Za-z0-9_-]{1,64}$/;
const NAME_RE = /^[A-Za-z0-9._-]{1,64}$/;
export const isNodeName = (s) => ID_RE.test(s || '');
export const isPipelineName = (s) => NAME_RE.test(s || '');

let seq = 0;
function uid(prefix) {
  seq += 1;
  return `${prefix}${Date.now().toString(36)}${seq.toString(36)}`;
}

export function emptyPipeline(name = 'new-pipeline') {
  return { key: uid('pl_'), remoteId: null, etag: null, name, options: {}, nodes: [], edges: [], dirty: false };
}

const SHORT = { source: 'src', decode: 'dec', board: 'board', scale: 'scale', crop: 'crop', compose: 'prod', audio_mix: 'mix', encode: 'enc', snapshot_on_change: 'snap', sink: 'out' };

/** 部品名（グラフの id）の候補。 */
export function freshName(pl, type) {
  const base = SHORT[type] || type;
  for (let i = 1; ; i += 1) {
    const name = `${base}${i}`;
    if (!pl.nodes.some((n) => n.name === name)) return name;
  }
}

export function addNode(pl, type, x, y) {
  const k = KINDS[type];
  const p = { ...(k.defaults || {}) };
  for (const f of k.fields) if (f.def !== undefined && p[f.path] === undefined) p[f.path] = f.def;
  const n = { id: uid('n'), type, name: freshName(pl, type), x: Math.round(x / 10) * 10, y: Math.round(y / 10) * 10, p, extra: {} };
  if (type === 'compose') {
    n.ins = [{ name: 'cam1' }, { name: 'cam2' }];
    n.p.scenes = {
      cam1: { layers: [{ source: 'cam1', x: 0, y: 0, width: 1280, height: 720 }] },
      cam2: { layers: [{ source: 'cam2', x: 0, y: 0, width: 1280, height: 720 }] },
    };
    n.p.program = 'cam1';
    n.p.preview = 'cam2';
  } else if (type === 'audio_mix') {
    n.ins = [{ name: 'in1', gain_db: 0, mute: false, solo: false, pan: 0, delay_ms: 0 }];
  }
  pl.nodes.push(n);
  return n;
}

export function removeNode(pl, id) {
  pl.nodes = pl.nodes.filter((n) => n.id !== id);
  pl.edges = pl.edges.filter((e) => e.from.node !== id && e.to.node !== id);
}

/** 名前付きの入力を消す（線も消し、後ろの線の番号を詰める）。 */
export function removeNamedInput(pl, n, index) {
  n.ins.splice(index, 1);
  pl.edges = pl.edges.filter((e) => !(e.to.node === n.id && e.to.port === index));
  for (const e of pl.edges) if (e.to.node === n.id && e.to.port > index) e.to.port -= 1;
}

const byId = (pl, id) => pl.nodes.find((m) => m.id === id);
const incoming = (pl, id, port) => pl.edges.find((e) => e.to.node === id && e.to.port === port);

/** 出力ポートの型（上流をたどって決める）。 */
export function outType(pl, ref, seen = new Set()) {
  const n = byId(pl, ref.node);
  if (!n || seen.has(n.id)) return null;
  seen.add(n.id);
  const up = () => {
    const e = incoming(pl, n.id, 0);
    return e ? outType(pl, e.from, seen) : null;
  };
  switch (n.type) {
    case 'source': return ref.port === 0 ? 'ev' : 'ea';
    case 'decode': { const t = up(); return t === 'ev' ? 'v' : t === 'ea' ? 'a' : 'raw'; }
    case 'encode': { const t = up(); return t === 'v' ? 'ev' : t === 'a' ? 'ea' : 'enc'; }
    case 'board': case 'scale': case 'crop': case 'compose': return 'v';
    case 'audio_mix': return 'a';
    default: return null;
  }
}

/** 型 t が受け口 accept に入るか（決まっていない型は広く受ける）。 */
export function compatible(t, accept) {
  if (!t || !accept) return false;
  if (accept.includes(t)) return true;
  if (t === 'raw') return accept.includes('v') || accept.includes('a');
  if (t === 'enc') return accept.includes('ev') || accept.includes('ea');
  return false;
}

export function connect(pl, from, to) {
  if (from.node === to.node) return { ok: false, reason: '同じ部品にはつなげません' };
  const tn = byId(pl, to.node);
  if (!tn) return { ok: false, reason: '部品がありません' };
  const port = inPorts(tn)[to.port];
  if (!port) return { ok: false, reason: '入力がありません' };
  const t = outType(pl, from);
  if (!compatible(t, port.accept)) {
    const hint = (t === 'ev' || t === 'ea') && port.accept.includes('v') ? '（間に「復号」を挟んでください）'
      : (t === 'v' || t === 'a') && port.accept.includes('ev') ? '（間に「符号化」を挟んでください）' : '';
    return { ok: false, reason: `${PORT_LABEL[t] || t} は ${port.accept.map((x) => PORT_LABEL[x]).filter(Boolean)[0] || ''} の入力につなげません${hint}` };
  }
  if (incoming(pl, to.node, to.port)) return { ok: false, reason: 'この入力にはもうつながっています' };
  const e = { id: uid('e'), from: { ...from }, to: { ...to } };
  pl.edges.push(e);
  if (hasCycle(pl)) {
    pl.edges.pop();
    return { ok: false, reason: '輪になるのでつなげません（DAG）' };
  }
  return { ok: true, edge: e };
}

export function hasCycle(pl) {
  const adj = new Map(pl.nodes.map((n) => [n.id, []]));
  for (const e of pl.edges) if (adj.has(e.from.node)) adj.get(e.from.node).push(e.to.node);
  const state = new Map();
  const visit = (id) => {
    if (state.get(id) === 1) return true;
    if (state.get(id) === 2) return false;
    state.set(id, 1);
    for (const nx of adj.get(id) || []) if (visit(nx)) return true;
    state.set(id, 2);
    return false;
  };
  return pl.nodes.some((n) => visit(n.id));
}

const empty = (v) => v === undefined || v === null || v === '';

/** 参照されている stream（source と sink）。 */
export function referencedStreams(pl) {
  return [...new Set(pl.nodes.filter((n) => n.type === 'source' || n.type === 'sink').map((n) => n.p.stream).filter(Boolean))];
}

/**
 * この画面での検査（保存前）。ノードの検証（POST /pipelines/{id}/validate）の方が正。
 * issues: [{level: 'error'|'warn', node?, edge?, text}]
 * @param {object} pl
 * @param {{royaltyFree?: boolean}} [opt] royalty_free の stream が関わるか
 */
export function validate(pl, opt = {}) {
  const issues = [];
  const err = (text, node, edge) => issues.push({ level: 'error', node, edge, text });
  const warn = (text, node) => issues.push({ level: 'warn', node, text });
  if (!isPipelineName(pl.name)) err('パイプラインの名前は英数字と . _ - の 1〜64 文字です');
  if (!pl.nodes.some((n) => n.type === 'source')) err('入力（source）がありません');
  if (!pl.nodes.some((n) => n.type === 'sink' || n.type === 'snapshot_on_change')) err('出力（sink）がありません');
  if (hasCycle(pl)) err('輪（循環）があります。DAG にしてください');
  const names = new Map();
  for (const n of pl.nodes) {
    if (!isNodeName(n.name)) err(`部品の名前「${n.name}」は英数字と _ - の 1〜64 文字です`, n.id);
    if (names.has(n.name)) err(`部品の名前「${n.name}」が重なっています`, n.id);
    names.set(n.name, n);
  }
  for (const e of pl.edges) {
    const tn = byId(pl, e.to.node);
    const port = tn && inPorts(tn)[e.to.port];
    const t = outType(pl, e.from);
    if (!port || !compatible(t, port.accept)) err(`つなげない組み合わせ: ${PORT_LABEL[t] || t} → ${tn ? tn.name : '?'}`, undefined, e.id);
    const fn = byId(pl, e.from.node);
    if (fn && fn.type === 'source') {
      const kind = e.from.port === 0 ? 'video' : 'audio';
      if (!fn.p[`${kind}.codec`]) err(`「${fn.name}」の ${kind} を使っていますが、${kind} のコーデックがありません`, fn.id, e.id);
    }
  }
  for (const n of pl.nodes) {
    const k = KINDS[n.type];
    const ports = inPorts(n);
    ports.forEach((port, i) => {
      if (incoming(pl, n.id, i)) return;
      if (n.type === 'sink' && i === 1) return;
      err(`「${n.name}」の入力「${port.name}」がつながっていません`, n.id);
    });
    if (k.named) {
      if (!n.ins.length) err(`「${n.name}」に入力がありません`, n.id);
      const seen = new Set();
      for (const i of n.ins) {
        if (!isNodeName(i.name)) err(`「${n.name}」の入力名「${i.name}」は英数字と _ - です`, n.id);
        if (seen.has(i.name)) err(`「${n.name}」の入力名「${i.name}」が重なっています`, n.id);
        seen.add(i.name);
      }
    }
    // 出力の使い手（サーバーでは UNCONSUMED_OUTPUT の誤り）
    outPorts(n).forEach((o, i) => {
      if (n.type === 'compose' && i === 1) return;
      if (n.type === 'source' && !n.p[`${o.name}.codec`]) return;
      if (!pl.edges.some((e) => e.from.node === n.id && e.from.port === i)) err(`「${n.name}」の出力「${o.name}」がどこにも使われていません`, n.id);
    });
    for (const f of k.fields) {
      if (f.when && !f.when(n.p)) continue;
      const v = n.p[f.path];
      if (f.required && empty(v)) err(`「${n.name}」の「${f.label}」がありません`, n.id);
      if (f.type === 'number' && !empty(v)) {
        const x = Number(v);
        if (Number.isNaN(x) || (f.min !== undefined && x < f.min) || (f.max !== undefined && x > f.max)) err(`「${n.name}」の「${f.label}」が範囲外です`, n.id);
      }
    }
    if (n.type === 'source' && !n.p['video.codec'] && !n.p['audio.codec']) err(`「${n.name}」は映像も音声も使っていません`, n.id);
    if (n.type === 'compose') {
      const scenes = n.p.scenes || {};
      const inNames = new Set(n.ins.map((i) => i.name));
      if (!Object.keys(scenes).length) err(`「${n.name}」にシーンがありません`, n.id);
      if (n.p.program && !scenes[n.p.program]) err(`「${n.name}」のプログラム「${n.p.program}」というシーンがありません`, n.id);
      if (n.p.preview && !scenes[n.p.preview]) err(`「${n.name}」のプレビュー「${n.p.preview}」というシーンがありません`, n.id);
      for (const [sn, sc] of Object.entries(scenes)) {
        (sc.layers || []).forEach((l, li) => {
          const kinds = ['source', 'image', 'color'].filter((x) => !empty(l[x]));
          if (kinds.length !== 1) err(`「${n.name}」のシーン「${sn}」のレイヤー ${li + 1} は source・image・color のどれか 1 つです`, n.id);
          if (l.source && !inNames.has(l.source)) err(`「${n.name}」のシーン「${sn}」が知らない入力「${l.source}」を使っています`, n.id);
          if (!(l.width > 0) || !(l.height > 0)) err(`「${n.name}」のシーン「${sn}」のレイヤー ${li + 1} の大きさがありません`, n.id);
        });
      }
    }
    if (n.type === 'sink') {
      const kinds = [0, 1].map((i) => incoming(pl, n.id, i)).filter(Boolean).map((e) => outType(pl, e.from));
      if (kinds.filter((t) => t === 'ev').length > 1 || kinds.filter((t) => t === 'ea').length > 1) err(`「${n.name}」に入れられるのは映像 1 つと音声 1 つまでです`, n.id);
    }
    if (opt.royaltyFree) {
      const codecs = n.type === 'encode' ? [n.p.codec] : n.type === 'source' ? [n.p['video.codec'], n.p['audio.codec']] : [];
      for (const c of codecs) if (c && ROYALTY_CODECS.has(c)) err(`「${n.name}」: royalty_free の stream が関わるので ${c} は使えません（REQ-007）`, n.id);
    }
  }
  if (pl.nodes.some((n) => n.type === 'encode' && n.p.codec === 'h264') && !opt.royaltyFree) {
    warn('H.264 の符号化にはハードウェアが要ります（無ければノードの検証で CAPACITY_UNAVAILABLE）');
  }
  return issues;
}

// ---------------------------------------------------------------- 変換

function getPath(obj, path) {
  return path.split('.').reduce((o, k) => (o && typeof o === 'object' ? o[k] : undefined), obj);
}

function setPath(obj, path, v) {
  const ks = path.split('.');
  let o = obj;
  for (const k of ks.slice(0, -1)) {
    if (!o[k] || typeof o[k] !== 'object') o[k] = {};
    o = o[k];
  }
  o[ks[ks.length - 1]] = v;
}

function delPath(obj, path) {
  const ks = path.split('.');
  let o = obj;
  for (const k of ks.slice(0, -1)) {
    if (!o || typeof o !== 'object') return;
    o = o[k];
  }
  if (o && typeof o === 'object') delete o[ks[ks.length - 1]];
}

function prune(o) {
  for (const k of Object.keys(o)) {
    if (o[k] && typeof o[k] === 'object' && !Array.isArray(o[k])) {
      prune(o[k]);
      if (!Object.keys(o[k]).length) delete o[k];
    }
  }
  return o;
}

/** 深いマージ（b の値が勝つ。配列は置き換え）。 */
function merge(a, b) {
  const out = { ...a };
  for (const [k, v] of Object.entries(b || {})) {
    out[k] = v && typeof v === 'object' && !Array.isArray(v) && out[k] && typeof out[k] === 'object' && !Array.isArray(out[k]) ? merge(out[k], v) : v;
  }
  return out;
}

const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

function fieldValue(f, v) {
  if (f.type === 'number') return Number(v);
  if (f.type === 'check') return !!v;
  return v;
}

function refOf(pl, from) {
  const n = byId(pl, from.node);
  if (!n) return null;
  const port = outPorts(n)[from.port];
  if (n.type === 'source') return `${n.name}.${port.name}`;
  if (n.type === 'compose') return from.port === 0 ? n.name : `${n.name}.preview`;
  return n.name;
}

/** 編集用のモデル → グラフ JSON（POST /pipelines の graph）。 */
export function toGraph(pl) {
  const options = {};
  for (const f of OPTION_FIELDS) if (!empty(pl.options[f.path])) options[f.path] = fieldValue(f, pl.options[f.path]);
  const nodes = pl.nodes.map((n) => {
    const k = KINDS[n.type];
    let o = clone(n.extra) || {};
    for (const f of k.fields) {
      const v = n.p[f.path];
      if (f.type === 'scene' && empty(v)) continue;
      if (!empty(v) && (!f.when || f.when(n.p))) setPath(o, f.path, fieldValue(f, v));
    }
    if (n.type === 'source') {
      if (!n.p['video.codec']) delete o.video;
      if (!n.p['audio.codec']) delete o.audio;
    }
    if (n.type === 'compose') o.scenes = clone(n.p.scenes) || {};
    const ins = inPorts(n).map((_, i) => {
      const e = incoming(pl, n.id, i);
      return e ? refOf(pl, e.from) : null;
    });
    if (n.type === 'compose') o.inputs = Object.fromEntries(n.ins.map((i, ix) => [i.name, ins[ix] || '']));
    else if (n.type === 'audio_mix') {
      o.inputs = Object.fromEntries(n.ins.map((i, ix) => {
        const m = { from: ins[ix] || '' };
        for (const key of ['gain_db', 'pan', 'delay_ms']) if (!empty(i[key]) && Number(i[key]) !== 0) m[key] = Number(i[key]);
        if (i.mute) m.mute = true;
        if (i.solo) m.solo = true;
        return [i.name, m];
      }));
    } else if (n.type === 'sink') o.inputs = ins.filter(Boolean);
    else if (n.type !== 'source') o.input = ins[0] || '';
    o = prune(o);
    if (n.type === 'compose') o.scenes = o.scenes || {};
    return { type: n.type, id: n.name, ...o };
  });
  const g = { version: GRAPH_VERSION };
  if (Object.keys(options).length) g.options = options;
  g.nodes = nodes;
  return g;
}

/** 部品の位置を自動で決める（上流からの深さで列、同じ列は上から）。 */
function autoLayout(pl) {
  const depth = new Map();
  const d = (id, seen = new Set()) => {
    if (depth.has(id)) return depth.get(id);
    if (seen.has(id)) return 0;
    seen.add(id);
    const ups = pl.edges.filter((e) => e.to.node === id).map((e) => d(e.from.node, seen) + 1);
    const v = ups.length ? Math.max(...ups) : 0;
    depth.set(id, v);
    return v;
  };
  const rows = new Map();
  for (const n of pl.nodes) {
    const c = d(n.id);
    const r = rows.get(c) || 0;
    rows.set(c, r + 1);
    n.x = 30 + c * 220;
    n.y = 30 + r * 110;
  }
}

/**
 * グラフ JSON → 編集用のモデル。
 * @param {object} graph
 * @param {{name?: string, positions?: Record<string, [number, number]>}} [opt]
 */
export function fromGraph(graph, opt = {}) {
  if (!graph || typeof graph !== 'object' || !Array.isArray(graph.nodes)) throw new Error('graph.nodes がありません');
  if (graph.version !== GRAPH_VERSION) throw new Error(`graph.version は ${GRAPH_VERSION} です`);
  const pl = emptyPipeline(opt.name || 'imported');
  for (const f of OPTION_FIELDS) if (graph.options && graph.options[f.path] !== undefined) pl.options[f.path] = graph.options[f.path];
  const pending = [];
  for (const gn of graph.nodes) {
    const k = KINDS[gn.type];
    if (!k) throw new Error(`知らない部品の種類: ${gn.type}`);
    const rest = clone(gn);
    delete rest.type;
    delete rest.id;
    const n = { id: uid('n'), type: gn.type, name: String(gn.id), x: 0, y: 0, p: {}, extra: {} };
    for (const f of k.fields) {
      const v = getPath(gn, f.path);
      if (v !== undefined && (typeof v !== 'object' || v === null)) {
        n.p[f.path] = v;
        delPath(rest, f.path);
      }
    }
    if (gn.type === 'compose') {
      n.p.scenes = clone(gn.scenes) || {};
      delete rest.scenes;
      n.ins = Object.keys(gn.inputs || {}).map((name) => ({ name }));
      Object.entries(gn.inputs || {}).forEach(([, ref], i) => pending.push({ to: n, port: i, ref }));
      delete rest.inputs;
    } else if (gn.type === 'audio_mix') {
      n.ins = Object.entries(gn.inputs || {}).map(([name, m]) => ({ name, gain_db: m.gain_db || 0, mute: !!m.mute, solo: !!m.solo, pan: m.pan || 0, delay_ms: m.delay_ms || 0 }));
      Object.values(gn.inputs || {}).forEach((m, i) => pending.push({ to: n, port: i, ref: m.from }));
      delete rest.inputs;
    } else if (gn.type === 'sink') {
      (gn.inputs || []).slice(0, 2).forEach((ref, i) => pending.push({ to: n, port: i, ref }));
      delete rest.inputs;
    } else if (gn.type !== 'source') {
      pending.push({ to: n, port: 0, ref: gn.input });
      delete rest.input;
    }
    n.extra = prune(rest);
    pl.nodes.push(n);
  }
  for (const { to, port, ref } of pending) {
    if (!ref) continue;
    const [nodeName, portName] = String(ref).split('.');
    const from = pl.nodes.find((m) => m.name === nodeName);
    if (!from) continue;
    const outs = outPorts(from).map((o) => o.name);
    let pi = portName ? outs.indexOf(portName) : 0;
    if (portName === 'out') pi = 0;
    if (pi < 0) continue;
    pl.edges.push({ id: uid('e'), from: { node: from.id, port: pi }, to: { node: to.id, port } });
  }
  const pos = opt.positions || {};
  const missing = pl.nodes.some((n) => !pos[n.name]);
  if (missing) autoLayout(pl);
  for (const n of pl.nodes) if (pos[n.name]) [n.x, n.y] = pos[n.name];
  return pl;
}

/** 位置（部品名 → [x, y]）。グラフには入れられないので画面側が覚える。 */
export function positionsOf(pl) {
  return Object.fromEntries(pl.nodes.map((n) => [n.name, [n.x, n.y]]));
}

/** 例: 2 本の VP9 を PinP に合成して AV1 ＋ Opus で出す（tools/interop/node.sh の J と同じ形）。 */
export function sampleGraph(streams = {}) {
  const { a = '', b = '', out = '' } = streams;
  return {
    version: 1,
    options: { clock: 'realtime' },
    nodes: [
      { type: 'source', id: 'cam_a', stream: a, video: { codec: 'vp9' }, audio: { codec: 'opus' } },
      { type: 'source', id: 'cam_b', stream: b, video: { codec: 'vp9' } },
      { type: 'decode', id: 'dec_a', input: 'cam_a.video' },
      { type: 'decode', id: 'dec_b', input: 'cam_b.video' },
      { type: 'decode', id: 'adec_a', input: 'cam_a.audio' },
      {
        type: 'compose', id: 'prod', inputs: { a: 'dec_a', b: 'dec_b' }, canvas: { width: 640, height: 360, fps_num: 30 },
        scenes: {
          pinp: { layers: [{ source: 'b', x: 0, y: 0, width: 640, height: 360 }, { source: 'a', x: 440, y: 250, width: 192, height: 108, z: 1 }] },
          cam_a: { layers: [{ source: 'a', x: 0, y: 0, width: 640, height: 360 }] },
          cam_b: { layers: [{ source: 'b', x: 0, y: 0, width: 640, height: 360 }] },
        },
        program: 'pinp', preview: 'cam_a', transition: { kind: 'fade', duration_ms: 300 },
      },
      { type: 'audio_mix', id: 'mix', inputs: { mic_a: { from: 'adec_a' } } },
      { type: 'encode', id: 'venc', input: 'prod', codec: 'av1', bitrate_kbps: 800, gop: 30 },
      { type: 'encode', id: 'aenc', input: 'mix', codec: 'opus', bitrate_kbps: 96 },
      { type: 'sink', id: 'out', inputs: ['venc', 'aenc'], stream: out },
    ],
  };
}

export function samplePipeline(streams) {
  const pl = fromGraph(sampleGraph(streams), { name: 'pinp-sample' });
  return pl;
}

/** グラフの合成の部品（スイッチャーの対象）。 */
export function composeNodes(graph) {
  return ((graph && graph.nodes) || []).filter((n) => n.type === 'compose');
}

/**
 * sink ごとに、合成のプログラム・プレビューのどちらから来ているかをたどる。
 * @returns {Array<{sink: string, stream: string, from: 'program'|'preview'|null, compose: string|null}>}
 */
export function sinkOrigins(graph) {
  const nodes = new Map(((graph && graph.nodes) || []).map((n) => [n.id, n]));
  const trace = (ref, seen = new Set()) => {
    if (!ref) return null;
    const [id, port] = String(ref).split('.');
    const n = nodes.get(id);
    if (!n || seen.has(id)) return null;
    seen.add(id);
    if (n.type === 'compose') return { from: port === 'preview' ? 'preview' : 'program', compose: n.id };
    if (n.input) return trace(n.input, seen);
    return null;
  };
  return [...nodes.values()].filter((n) => n.type === 'sink').map((s) => {
    const hit = (s.inputs || []).map((r) => trace(r)).find(Boolean) || { from: null, compose: null };
    return { sink: s.id, stream: s.stream, ...hit };
  });
}
