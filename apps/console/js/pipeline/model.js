// パイプライン定義（senca-pipeline のグラフ JSON、`version: 1`）と編集用のモデル。画面に依存しない（node でも読める）。
//
// グラフの形は crates/senca-pipeline/src/model.rs が正（知らない項目は拒否される）。編集用のモデルは
// 部品（位置・平らにした設定）と線（出力ポート → 入力ポート）を持ち、toGraph / fromGraph で行き来する。
//
// - 部品の種類: source・decode・board・scale・crop・compose・audio_mix・encode・snapshot_on_change・sink
// - 入力の参照は `"部品"` か `"部品.ポート"`（source は video・audio、compose は program・preview、ほかは out）
// - 場所（どのノードで動かすか）は持たない（REQ-005）。画面の位置はグラフに入れられないので、画面側が別に覚える。
// - 取り込んだグラフの、この画面で編集しない項目（compose の images、board の corners など）はそのまま残す。
// - 編集の道具: つなげるかの下見（checkConnect）・自動の配置（autoLayout）・元に戻す用の写し（serialize / restore）・
//   写しと貼り付け（copyNodes / pasteNodes）。

import { t as tr } from '../i18n/index.js';

export const GRAPH_VERSION = 1;

/** 鍵の文を、読むたびに今の言語で引く（言語を切り替えても作り直さなくてよいように）。 */
const L = (key) => ({ key, toString: () => tr(key) });
const withLabel = (o, key) => Object.defineProperty(o, 'label', { get: () => tr(key), enumerable: true, configurable: true });
/** [値, 鍵] の選択肢を [値, 文] にする（鍵でないものはそのまま）。 */
const resolveOptions = (opts) => opts.map((o) => (Array.isArray(o) && typeof o[1] === 'string' && o[1].startsWith('pipelines.') ? [o[0], tr(o[1])] : o));

/** ポートの型。ev/ea = 符号化済みの映像・音声、v/a = 生のフレーム。raw/enc は上流が決まっていないとき。 */
export const PORT_LABEL = {};
for (const k of ['ev', 'ea', 'v', 'a', 'raw', 'enc']) Object.defineProperty(PORT_LABEL, k, { get: () => tr(`pipelines.port.${k}`), enumerable: true });

/** ポートの型の見た目: media = video・audio・any（色）、encoded = 符号化済みか（形: 菱形か丸）。 */
export function portLook(type) {
  const media = type === 'ev' || type === 'v' ? 'video' : type === 'ea' || type === 'a' ? 'audio' : 'any';
  return { media, encoded: type === 'ev' || type === 'ea' || type === 'enc' };
}

const VIDEO_CODECS = ['av1', 'vp9', 'vp8', 'h264', 'h265'];
const codecOpts = (list) => list.map((c) => [c, `pipelines.codec.${c}`]);
const AUDIO_CODECS = ['opus', 'aac', 'pcma', 'pcmu'];
const ENCODE_CODECS = ['av1', 'vp9', 'vp8', 'h264', 'h265', 'opus'];
const FILTERS = ['bilinear', 'nearest', 'bicubic', 'lanczos3', 'area'];
const TRANSITION_KEYS = ['cut', 'fade', 'wipe_left', 'wipe_right'];
/** トランジションの種類 [値, 今の言語の名前]。 */
export const transitions = () => TRANSITION_KEYS.map((k) => [k, tr(`pipelines.transition.${k}`)]);
/** ロイヤリティのあるコーデック（royalty_free の stream が関わると使えない。REQ-007）。 */
export const ROYALTY_CODECS = new Set(['h264', 'h265', 'aac']);
/** ハードウェア（GPU）が無いと扱えないコーデック（無ければノードの検証で CAPACITY_UNAVAILABLE）。 */
export const GPU_CODECS = new Set(['h264', 'h265']);

const num = (path, key, extra = {}) => withLabel({ path, type: 'number', ...extra }, key);
const sel = (path, key, options, extra = {}) => {
  const o = withLabel({ path, type: 'select', ...extra }, key);
  return Object.defineProperty(o, 'options', { get: () => resolveOptions(typeof options === 'function' ? options() : options), enumerable: true });
};
const chk = (path, key, def, extra = {}) => withLabel({ path, type: 'check', def, ...extra }, key);
/** 説明の文（検査器で欄の下に出す）。 */
const help = (f, key) => Object.defineProperty(f, 'help', { get: () => tr(key), enumerable: true });
/** 詳しい設定（検査器で「詳しい設定」の中に畳む）。 */
const adv = (f) => Object.assign(f, { advanced: true });

/**
 * 部品の種類。
 * - cat: 分類（色の名前。css の --k-<cat>）・icon: アイコン・desc: 説明の鍵
 * - in: 入力ポート（固定）。名前付きの入力（compose・audio_mix）は部品ごとの `ins` から作る。
 * - out: 出力ポートの名前（型は outType で上流から決める）。
 * - fields: 検査器に出す設定（path はグラフの中の場所）。
 */
export const KINDS = {
  source: {
    cat: 'input', icon: 'streams', in: [], out: ['video', 'audio'],
    fields: [
      help(withLabel({ path: 'stream', type: 'stream', required: true }, 'pipelines.f.source.stream'), 'pipelines.h.source.stream'),
      help(sel('video.codec', 'pipelines.f.source.video.codec', [['', 'pipelines.notUsed'], ...codecOpts(VIDEO_CODECS)]), 'pipelines.h.source.codec'),
      adv(num('video.track', 'pipelines.f.source.video.track', { min: 0, when: (p) => !!p['video.codec'] })),
      adv(num('video.width', 'pipelines.f.source.video.width', { min: 16, when: (p) => !!p['video.codec'] })),
      adv(num('video.height', 'pipelines.f.source.video.height', { min: 16, when: (p) => !!p['video.codec'] })),
      adv(num('video.fps', 'pipelines.f.source.video.fps', { min: 1, step: 'any', when: (p) => !!p['video.codec'] })),
      sel('audio.codec', 'pipelines.f.source.audio.codec', [['', 'pipelines.notUsed'], ...codecOpts(AUDIO_CODECS)]),
      adv(num('audio.track', 'pipelines.f.source.audio.track', { min: 0, when: (p) => !!p['audio.codec'] })),
      adv(num('audio.sample_rate', 'pipelines.f.source.audio.sample_rate', { min: 8000, when: (p) => !!p['audio.codec'] })),
      adv(num('audio.channels', 'pipelines.f.source.audio.channels', { min: 1, max: 8, when: (p) => !!p['audio.codec'] })),
    ],
    defaults: { 'video.codec': 'vp9' },
  },
  decode: {
    cat: 'decode', icon: 'expand', in: [['input', ['ev', 'ea', 'enc']]], out: ['out'],
    fields: [
      help(adv(num('max_width', 'pipelines.f.decode.max_width', { min: 16 })), 'pipelines.h.decode.max'),
      adv(num('max_height', 'pipelines.f.decode.max_height', { min: 16 })),
      help(adv(num('threads', 'pipelines.f.decode.threads', { min: 0, def: 0 })), 'pipelines.h.threads'),
    ],
  },
  board: {
    cat: 'video', icon: 'presentation', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [
      sel('params.mode', 'pipelines.f.board.params.mode', [['whiteboard', 'pipelines.opt.whiteboard'], ['blackboard', 'pipelines.opt.blackboard']], { def: 'whiteboard' }),
      help(chk('params.auto_detect', 'pipelines.f.board.params.auto_detect', false), 'pipelines.h.board.auto_detect'),
      adv(num('params.redetect_frames', 'pipelines.f.board.params.redetect_frames', { min: 0, def: 0 })),
      num('params.output_width', 'pipelines.f.board.params.output_width', { min: 16, def: 1280 }),
      num('params.output_height', 'pipelines.f.board.params.output_height', { min: 16, def: 720 }),
      help(chk('params.enhance', 'pipelines.f.board.params.enhance', true), 'pipelines.h.board.enhance'),
      adv(num('params.stroke_gain', 'pipelines.f.board.params.stroke_gain', { step: 'any' })),
      adv(num('params.chroma_gain', 'pipelines.f.board.params.chroma_gain', { step: 'any' })),
      chk('params.invert_to_white', 'pipelines.f.board.params.invert_to_white', false),
      adv(chk('params.clahe', 'pipelines.f.board.params.clahe', false)),
    ],
  },
  scale: {
    cat: 'video', icon: 'scaling', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [
      num('width', 'pipelines.f.scale.width', { min: 16, required: true, def: 1280 }),
      num('height', 'pipelines.f.scale.height', { min: 16, required: true, def: 720 }),
      help(adv(sel('filter', 'pipelines.f.scale.filter', FILTERS, { def: 'bilinear' })), 'pipelines.h.scale.filter'),
    ],
  },
  crop: {
    cat: 'video', icon: 'crop', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [
      help(num('x', 'pipelines.f.crop.x', { min: 0, required: true, def: 0 }), 'pipelines.h.crop'),
      num('y', 'pipelines.f.crop.y', { min: 0, required: true, def: 0 }),
      num('width', 'pipelines.f.crop.width', { min: 2, required: true, def: 640 }),
      num('height', 'pipelines.f.crop.height', { min: 2, required: true, def: 360 }),
    ],
  },
  compose: {
    cat: 'compose', icon: 'layers', named: true, out: ['program', 'preview'],
    accept: ['v', 'raw'],
    fields: [
      num('canvas.width', 'pipelines.f.compose.canvas.width', { min: 16, required: true, def: 1280 }),
      num('canvas.height', 'pipelines.f.compose.canvas.height', { min: 16, required: true, def: 720 }),
      help(num('canvas.fps_num', 'pipelines.f.compose.canvas.fps_num', { min: 1, required: true, def: 30 }), 'pipelines.h.compose.fps'),
      adv(num('canvas.fps_den', 'pipelines.f.compose.canvas.fps_den', { min: 1, def: 1 })),
      withLabel({ path: 'canvas.background', type: 'color', def: '#000000' }, 'pipelines.f.compose.canvas.background'),
      help(withLabel({ path: 'program', type: 'scene', required: true }, 'pipelines.f.compose.program'), 'pipelines.h.compose.program'),
      help(withLabel({ path: 'preview', type: 'scene' }, 'pipelines.f.compose.preview'), 'pipelines.h.compose.preview'),
      help(sel('transition.kind', 'pipelines.f.compose.transition.kind', transitions, { def: 'cut' }), 'pipelines.h.compose.transition'),
      num('transition.duration_ms', 'pipelines.f.compose.transition.duration_ms', { min: 0, def: 0 }),
      help(adv(num('missing.hold_ms', 'pipelines.f.compose.missing.hold_ms', { min: 0, def: 2000 })), 'pipelines.h.compose.missing'),
      adv(withLabel({ path: 'missing.slate_color', type: 'color', def: '#000000' }, 'pipelines.f.compose.missing.slate_color')),
      adv(num('threads', 'pipelines.f.compose.threads', { min: 0, def: 0 })),
    ],
  },
  audio_mix: {
    cat: 'audio', icon: 'sliders', named: true, out: ['out'], accept: ['a', 'raw'],
    fields: [
      num('master_gain_db', 'pipelines.f.audio_mix.master_gain_db', { step: 0.5, def: 0 }),
      help(chk('limiter', 'pipelines.f.audio_mix.limiter', true), 'pipelines.h.audio_mix.limiter'),
    ],
  },
  encode: {
    cat: 'encode', icon: 'shrink', in: [['input', ['v', 'a', 'raw']]], out: ['out'],
    fields: [
      help(sel('codec', 'pipelines.f.encode.codec', codecOpts(ENCODE_CODECS), { required: true, def: 'av1' }), 'pipelines.h.encode.codec'),
      help(num('bitrate_kbps', 'pipelines.f.encode.bitrate_kbps', { min: 1, required: true, def: 2000 }), 'pipelines.h.encode.bitrate'),
      help(num('gop', 'pipelines.f.encode.gop', { min: 1 }), 'pipelines.h.encode.gop'),
      help(chk('low_latency', 'pipelines.f.encode.low_latency', true), 'pipelines.h.encode.low_latency'),
      adv(sel('rate_control', 'pipelines.f.encode.rate_control', ['cbr', 'vbr'], { def: 'cbr' })),
      adv(num('width', 'pipelines.f.encode.width', { min: 16 })),
      adv(num('height', 'pipelines.f.encode.height', { min: 16 })),
      adv(num('frame_ms', 'pipelines.f.encode.frame_ms', { min: 2.5, step: 'any', when: (p) => p.codec === 'opus' })),
      adv(num('threads', 'pipelines.f.encode.threads', { min: 0, def: 0 })),
    ],
  },
  snapshot_on_change: {
    cat: 'output', icon: 'camera', in: [['input', ['v', 'raw']]], out: [],
    fields: [
      sel('params.mode', 'pipelines.f.snapshot_on_change.params.mode', [['whiteboard', 'pipelines.opt.whiteboard'], ['blackboard', 'pipelines.opt.blackboard']], { def: 'whiteboard' }),
      help(chk('params.enabled', 'pipelines.f.snapshot_on_change.params.enabled', true), 'pipelines.h.snapshot.enabled'),
      adv(num('params.analysis_fps', 'pipelines.f.snapshot_on_change.params.analysis_fps', { min: 0, step: 'any', def: 2 })),
      adv(num('params.stable_frames', 'pipelines.f.snapshot_on_change.params.stable_frames', { min: 1, def: 3 })),
      help(adv(num('params.change_fraction', 'pipelines.f.snapshot_on_change.params.change_fraction', { min: 0, step: 'any', def: 0.004 })), 'pipelines.h.snapshot.change'),
      adv(num('params.jpeg_quality', 'pipelines.f.snapshot_on_change.params.jpeg_quality', { min: 1, max: 100, def: 85 })),
    ],
  },
  sink: {
    cat: 'output', icon: 'live', in: [[L('pipelines.port.in1'), ['ev', 'ea', 'enc']], [L('pipelines.port.in2'), ['ev', 'ea', 'enc']]], out: [],
    fields: [help(withLabel({ path: 'stream', type: 'stream', required: true }, 'pipelines.f.sink.stream'), 'pipelines.h.sink.stream')],
  },
};

for (const [type, k] of Object.entries(KINDS)) {
  withLabel(k, `pipelines.kind.${type}`);
  Object.defineProperty(k, 'desc', { get: () => tr(`pipelines.kd.${type}`), enumerable: true });
}

/** 部品の置き場の分け方（左の一覧の見出し）。 */
export const PALETTE_GROUPS = [
  ['in', ['source']],
  ['process', ['decode', 'board', 'scale', 'crop']],
  ['mix', ['compose', 'audio_mix']],
  ['out', ['encode', 'sink', 'snapshot_on_change']],
];

export const OPTION_FIELDS = [
  help(sel('clock', 'pipelines.f.options.clock', [['realtime', 'pipelines.opt.realtime'], ['input', 'pipelines.opt.input']], { def: 'realtime' }), 'pipelines.h.options.clock'),
  sel('overflow', 'pipelines.f.options.overflow', [['drop', 'pipelines.opt.drop'], ['block', 'pipelines.opt.block']], { def: 'drop' }),
  help(num('latency_ms', 'pipelines.f.options.latency_ms', { min: 0, def: 100 }), 'pipelines.h.options.latency'),
  num('missing_timeout_ms', 'pipelines.f.options.missing_timeout_ms', { min: 1, def: 2000 }),
  num('block_timeout_ms', 'pipelines.f.options.block_timeout_ms', { min: 1, def: 2000 }),
  num('input_queue_aus', 'pipelines.f.options.input_queue_aus', { min: 1, def: 256 }),
  num('frame_queue', 'pipelines.f.options.frame_queue', { min: 1, def: 8 }),
  num('status_interval_ms', 'pipelines.f.options.status_interval_ms', { min: 0, def: 100 }),
  num('commit_timeout_ms', 'pipelines.f.options.commit_timeout_ms', { min: 1, def: 10000 }),
];

// ---------------------------------------------------------------- 部品の形（画面の座標）

export const NODE_W = 188;
export const NODE_HEAD = 48;
const ROW = 22;
export const GRID = 20;

/** 部品の高さ（ポートの数で伸びる）。 */
export function nodeHeight(n) {
  const k = Math.max(inPorts(n).length, outPorts(n).length, 1);
  return NODE_HEAD + 4 + k * ROW + 8;
}

/** ポートの中心の、部品の左上からの位置。 */
export function portOffset(n, dir, i) {
  return { x: dir === 'out' ? NODE_W : 0, y: NODE_HEAD + 4 + i * ROW + ROW / 2 };
}

/** 入力ポート: [{name, accept}] */
export function inPorts(n) {
  const k = KINDS[n.type];
  if (k.named) return (n.ins || []).map((i) => ({ name: i.name, accept: k.accept }));
  return k.in.map(([name, accept]) => ({ name: String(name), accept }));
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
export function freshName(pl, type, base) {
  const taken = new Set(pl.nodes.map((n) => n.name));
  if (base && !taken.has(base)) return base;
  if (!base) {
    for (let i = 1; ; i += 1) {
      const name = `${SHORT[type] || type}${i}`;
      if (!taken.has(name)) return name;
    }
  }
  const stem = base.replace(/_\d+$/, '').slice(0, 58);
  for (let i = 2; ; i += 1) {
    const name = `${stem}_${i}`;
    if (!taken.has(name)) return name;
  }
}

const snap = (v) => Math.round(v / GRID) * GRID;

export function addNode(pl, type, x, y) {
  const k = KINDS[type];
  const p = { ...(k.defaults || {}) };
  for (const f of k.fields) if (f.def !== undefined && p[f.path] === undefined) p[f.path] = f.def;
  const n = { id: uid('n'), type, name: freshName(pl, type), x: snap(x), y: snap(y), p, extra: {} };
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

/** 名前付きの入力を足す（compose・audio_mix）。名前を返す。 */
export function addNamedInput(n, base = 'in') {
  let k = n.ins.length + 1;
  while (n.ins.some((x) => x.name === `${base}${k}`)) k += 1;
  const name = `${base}${k}`;
  n.ins.push(n.type === 'audio_mix' ? { name, gain_db: 0, mute: false, solo: false, pan: 0, delay_ms: 0 } : { name });
  return name;
}

const byId = (pl, id) => pl.nodes.find((m) => m.id === id);
const incoming = (pl, id, port) => pl.edges.find((e) => e.to.node === id && e.to.port === port);
export const nodeById = byId;
export const edgeInto = incoming;

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

/** 受け口の説明（「映像（生）」など、最初の 1 つ）。 */
const acceptLabel = (accept) => accept.map((x) => PORT_LABEL[x]).filter(Boolean)[0] || '';

/**
 * つなげるかの下見（モデルは変えない）。{ok, reason?, code?}
 * code: same・noInput・incompatible・taken・cycle（taken は置き換えればつなげる）
 */
export function checkConnect(pl, from, to) {
  if (from.node === to.node) return { ok: false, code: 'same', reason: tr('pipelines.v.sameNode') };
  const tn = byId(pl, to.node);
  if (!tn) return { ok: false, code: 'noInput', reason: tr('pipelines.v.noNode') };
  const port = inPorts(tn)[to.port];
  if (!port) return { ok: false, code: 'noInput', reason: tr('pipelines.v.noInput') };
  const t = outType(pl, from);
  if (!compatible(t, port.accept)) {
    const hint = (t === 'ev' || t === 'ea') && port.accept.includes('v') ? tr('pipelines.v.needDecode')
      : (t === 'v' || t === 'a') && port.accept.includes('ev') ? tr('pipelines.v.needEncode')
        : t === 'v' && port.accept.includes('a') && !port.accept.includes('v') ? tr('pipelines.v.needAudio')
          : t === 'a' && port.accept.includes('v') && !port.accept.includes('a') ? tr('pipelines.v.needVideo') : '';
    return { ok: false, code: 'incompatible', reason: tr('pipelines.v.incompatible', { from: PORT_LABEL[t] || t || '?', to: acceptLabel(port.accept), hint }) };
  }
  const taken = incoming(pl, to.node, to.port);
  pl.edges.push({ id: '__probe', from: { ...from }, to: { ...to } });
  const cyc = hasCycle(pl);
  pl.edges.pop();
  if (cyc) return { ok: false, code: 'cycle', reason: tr('pipelines.v.cycleEdge') };
  if (taken) return { ok: false, code: 'taken', reason: tr('pipelines.v.taken'), replace: taken };
  return { ok: true };
}

/** つなぐ。replace: true なら、入力に既にある線を置き換える。 */
export function connect(pl, from, to, { replace = false } = {}) {
  const c = checkConnect(pl, from, to);
  if (!c.ok && !(replace && c.code === 'taken')) return c;
  if (c.code === 'taken') pl.edges = pl.edges.filter((e) => e !== c.replace);
  const e = { id: uid('e'), from: { ...from }, to: { ...to } };
  pl.edges.push(e);
  return { ok: true, edge: e };
}

/** 出力 from につなげる入力の一覧: [{node, port, name, taken}] */
export function connectTargets(pl, from) {
  const out = [];
  for (const m of pl.nodes) {
    inPorts(m).forEach((port, i) => {
      const c = checkConnect(pl, from, { node: m.id, port: i });
      if (c.ok || c.code === 'taken') out.push({ node: m.id, port: i, name: port.name, taken: c.code === 'taken' });
    });
  }
  return out;
}

/** 入力 to につなげる出力の一覧: [{node, port, name, type}] */
export function connectSources(pl, to) {
  const out = [];
  for (const m of pl.nodes) {
    outPorts(m).forEach((o, i) => {
      const from = { node: m.id, port: i };
      const c = checkConnect(pl, from, to);
      if (c.ok || c.code === 'taken') out.push({ node: m.id, port: i, name: o.name, type: outType(pl, from) });
    });
  }
  return out;
}

/** 型 type の線を受けられる部品の種類（線を何もない所へ落としたときの候補）。 */
export function kindsAccepting(type) {
  return Object.entries(KINDS).filter(([, k]) => (k.named ? compatible(type, k.accept) : k.in.some(([, acc]) => compatible(type, acc)))).map(([t]) => t);
}

/** 型 type を出せる部品の種類（入力から逆に線を引いたときの候補）。 */
export function kindsProducing(accept) {
  const can = { source: ['ev', 'ea'], decode: ['v', 'a'], board: ['v'], scale: ['v'], crop: ['v'], compose: ['v'], audio_mix: ['a'], encode: ['ev', 'ea'] };
  return Object.entries(can).filter(([, ts]) => ts.some((t) => compatible(t, accept))).map(([t]) => t);
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
 * issues: [{level: 'error'|'warn', node?, edge?, text, code?}]
 * @param {object} pl
 * @param {{royaltyFree?: boolean}} [opt] royalty_free の stream が関わるか
 */
export function validate(pl, opt = {}) {
  const issues = [];
  const err = (text, node, edge, code) => issues.push({ level: 'error', node, edge, text, code });
  const warn = (text, node, code) => issues.push({ level: 'warn', node, text, code });
  if (!isPipelineName(pl.name)) err(tr('pipelines.v.pipelineName'));
  if (!pl.nodes.some((n) => n.type === 'source')) err(tr('pipelines.v.noSource'), undefined, undefined, 'noSource');
  if (!pl.nodes.some((n) => n.type === 'sink' || n.type === 'snapshot_on_change')) err(tr('pipelines.v.noSink'), undefined, undefined, 'noSink');
  if (hasCycle(pl)) err(tr('pipelines.v.cycle'));
  const names = new Map();
  for (const n of pl.nodes) {
    if (!isNodeName(n.name)) err(tr('pipelines.v.nodeName', { node: n.name }), n.id);
    if (names.has(n.name)) err(tr('pipelines.v.nodeDup', { node: n.name }), n.id);
    names.set(n.name, n);
  }
  for (const e of pl.edges) {
    const tn = byId(pl, e.to.node);
    const port = tn && inPorts(tn)[e.to.port];
    const t = outType(pl, e.from);
    if (!port || !compatible(t, port.accept)) err(tr('pipelines.v.badEdge', { type: PORT_LABEL[t] || t, node: tn ? tn.name : '?' }), tn && tn.id, e.id);
    const fn = byId(pl, e.from.node);
    if (fn && fn.type === 'source') {
      const kind = e.from.port === 0 ? 'video' : 'audio';
      if (!fn.p[`${kind}.codec`]) err(tr('pipelines.v.noCodec', { node: fn.name, kind: tr(`pipelines.media.${kind}`) }), fn.id, e.id);
    }
  }
  for (const n of pl.nodes) {
    const k = KINDS[n.type];
    const ports = inPorts(n);
    ports.forEach((port, i) => {
      if (incoming(pl, n.id, i)) return;
      if (n.type === 'sink' && i === 1) return;
      err(tr('pipelines.v.unconnected', { node: n.name, port: port.name }), n.id, undefined, 'unconnected');
    });
    if (k.named) {
      if (!n.ins.length) err(tr('pipelines.v.noIns', { node: n.name }), n.id);
      const seen = new Set();
      for (const i of n.ins) {
        if (!isNodeName(i.name)) err(tr('pipelines.v.inName', { node: n.name, input: i.name }), n.id);
        if (seen.has(i.name)) err(tr('pipelines.v.inDup', { node: n.name, input: i.name }), n.id);
        seen.add(i.name);
      }
    }
    // 出力の使い手（サーバーでは UNCONSUMED_OUTPUT の誤り）
    outPorts(n).forEach((o, i) => {
      if (n.type === 'compose' && i === 1) return;
      if (n.type === 'source' && !n.p[`${o.name}.codec`]) return;
      if (!pl.edges.some((e) => e.from.node === n.id && e.from.port === i)) err(tr('pipelines.v.unused', { node: n.name, port: o.name }), n.id, undefined, 'UNCONSUMED_OUTPUT');
    });
    for (const f of k.fields) {
      if (f.when && !f.when(n.p)) continue;
      const v = n.p[f.path];
      if (f.required && empty(v)) err(tr('pipelines.v.missingField', { node: n.name, field: f.label }), n.id, undefined, 'missing');
      if (f.type === 'number' && !empty(v)) {
        const x = Number(v);
        if (Number.isNaN(x) || (f.min !== undefined && x < f.min) || (f.max !== undefined && x > f.max)) err(tr('pipelines.v.range', { node: n.name, field: f.label }), n.id);
      }
    }
    if (n.type === 'source' && !n.p['video.codec'] && !n.p['audio.codec']) err(tr('pipelines.v.noTracks', { node: n.name }), n.id);
    if (n.type === 'compose') {
      const scenes = n.p.scenes || {};
      const inNames = new Set(n.ins.map((i) => i.name));
      if (!Object.keys(scenes).length) err(tr('pipelines.v.noScenes', { node: n.name }), n.id);
      if (n.p.program && !scenes[n.p.program]) err(tr('pipelines.v.noProgram', { node: n.name, scene: n.p.program }), n.id);
      if (n.p.preview && !scenes[n.p.preview]) err(tr('pipelines.v.noPreview', { node: n.name, scene: n.p.preview }), n.id);
      for (const [sn, sc] of Object.entries(scenes)) {
        (sc.layers || []).forEach((l, li) => {
          const kinds = ['source', 'image', 'color'].filter((x) => !empty(l[x]));
          if (kinds.length !== 1) err(tr('pipelines.v.layerKind', { node: n.name, scene: sn, n: li + 1 }), n.id);
          if (l.source && !inNames.has(l.source)) err(tr('pipelines.v.layerSource', { node: n.name, scene: sn, input: l.source }), n.id);
          if (!(l.width > 0) || !(l.height > 0)) err(tr('pipelines.v.layerSize', { node: n.name, scene: sn, n: li + 1 }), n.id);
        });
      }
    }
    if (n.type === 'sink') {
      const kinds = [0, 1].map((i) => incoming(pl, n.id, i)).filter(Boolean).map((e) => outType(pl, e.from));
      if (kinds.filter((t) => t === 'ev').length > 1 || kinds.filter((t) => t === 'ea').length > 1) err(tr('pipelines.v.sinkTracks', { node: n.name }), n.id);
    }
    if (n.type === 'encode' && n.p.codec === 'opus') {
      const e = incoming(pl, n.id, 0);
      if (e && outType(pl, e.from) === 'v') err(tr('pipelines.v.opusVideo', { node: n.name }), n.id);
    } else if (n.type === 'encode' && n.p.codec) {
      const e = incoming(pl, n.id, 0);
      if (e && outType(pl, e.from) === 'a') err(tr('pipelines.v.videoCodecAudio', { node: n.name, codec: n.p.codec }), n.id);
    }
    if (opt.royaltyFree) {
      const codecs = n.type === 'encode' ? [n.p.codec] : n.type === 'source' ? [n.p['video.codec'], n.p['audio.codec']] : [];
      for (const c of codecs) if (c && ROYALTY_CODECS.has(c)) err(tr('pipelines.v.royalty', { node: n.name, codec: c }), n.id, undefined, 'POLICY_REJECTED');
    }
  }
  // GPU が要るコーデック（符号化と、source の映像を復号するとき）
  for (const n of pl.nodes) {
    let codec = null;
    if (n.type === 'encode' && GPU_CODECS.has(n.p.codec)) codec = n.p.codec;
    if (n.type === 'decode') {
      const e = incoming(pl, n.id, 0);
      const up = e && byId(pl, e.from.node);
      if (up && up.type === 'source' && e.from.port === 0 && GPU_CODECS.has(up.p['video.codec'])) codec = up.p['video.codec'];
    }
    if (codec && !(opt.royaltyFree && ROYALTY_CODECS.has(codec))) warn(tr('pipelines.v.gpu', { node: n.name, codec: codec.toUpperCase().replace('H26', 'H.26') }), n.id, 'CAPACITY_UNAVAILABLE');
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

const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

function fieldValue(f, v) {
  if (f.type === 'number') return Number(v);
  if (f.type === 'check') return !!v;
  return v;
}

/** 出力ポートの参照の文字列（`cam.video`・`prod`・`prod.preview`・`dec`）。 */
export function refOf(pl, from) {
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

/** 実行の順（上流から。輪があれば残りを後ろに付ける）。部品の id の配列。 */
export function topoOrder(pl) {
  const indeg = new Map(pl.nodes.map((n) => [n.id, 0]));
  for (const e of pl.edges) if (indeg.has(e.to.node)) indeg.set(e.to.node, indeg.get(e.to.node) + 1);
  const q = pl.nodes.filter((n) => !indeg.get(n.id)).map((n) => n.id);
  const out = [];
  while (q.length) {
    const id = q.shift();
    out.push(id);
    for (const e of pl.edges.filter((x) => x.from.node === id)) {
      indeg.set(e.to.node, indeg.get(e.to.node) - 1);
      if (indeg.get(e.to.node) === 0) q.push(e.to.node);
    }
  }
  for (const n of pl.nodes) if (!out.includes(n.id)) out.push(n.id);
  return out;
}

/**
 * 部品の位置を自動で決める: 上流からの深さで列に分け、列の中は上流の位置の平均（重心）で並べて線の交差を減らす。
 * ids を渡すとその部品だけを動かす（ほかは今の位置のまま）。
 */
export function autoLayout(pl, { x0 = 40, y0 = 40, colGap = 72, rowGap = 28 } = {}) {
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
  for (const n of pl.nodes) d(n.id);
  // 終端（sink・snapshot）は一番右の列にそろえる
  const maxD = Math.max(0, ...depth.values());
  for (const n of pl.nodes) if ((n.type === 'sink' || n.type === 'snapshot_on_change') && !pl.edges.some((e) => e.from.node === n.id)) depth.set(n.id, Math.max(depth.get(n.id), maxD));
  const cols = new Map();
  for (const n of pl.nodes) {
    const c = depth.get(n.id);
    if (!cols.has(c)) cols.set(c, []);
    cols.get(c).push(n);
  }
  const order = [...cols.keys()].sort((a, b) => a - b);
  const centerY = new Map();
  for (const c of order) {
    const col = cols.get(c);
    const key = (n) => {
      const ups = pl.edges.filter((e) => e.to.node === n.id).map((e) => {
        const u = byId(pl, e.from.node);
        return (centerY.get(e.from.node) ?? 0) + (u ? portOffset(u, 'out', e.from.port).y : 0);
      });
      return ups.length ? ups.reduce((a, b) => a + b, 0) / ups.length : pl.nodes.indexOf(n) * 1000;
    };
    const keyed = col.map((n) => [key(n), n]).sort((a, b) => a[0] - b[0]);
    let y = y0;
    for (const [, n] of keyed) {
      n.x = x0 + c * (NODE_W + colGap);
      n.y = snap(y);
      centerY.set(n.id, n.y);
      y = n.y + nodeHeight(n) + rowGap;
    }
  }
  // 列ごとに縦の真ん中をそろえる
  const heights = order.map((c) => { const col = cols.get(c); const last = col.reduce((m, n) => Math.max(m, n.y + nodeHeight(n)), 0); return last - y0; });
  const tallest = Math.max(0, ...heights);
  order.forEach((c, i) => {
    const off = snap((tallest - heights[i]) / 2);
    if (off > 0) for (const n of cols.get(c)) n.y += off;
  });
}

/**
 * グラフ JSON → 編集用のモデル。
 * @param {object} graph
 * @param {{name?: string, positions?: Record<string, [number, number]>}} [opt]
 */
export function fromGraph(graph, opt = {}) {
  if (!graph || typeof graph !== 'object' || !Array.isArray(graph.nodes)) throw new Error(tr('pipelines.v.noNodes'));
  if (graph.version !== GRAPH_VERSION) throw new Error(tr('pipelines.v.version', { v: GRAPH_VERSION }));
  const pl = emptyPipeline(opt.name || 'imported');
  for (const f of OPTION_FIELDS) if (graph.options && graph.options[f.path] !== undefined) pl.options[f.path] = graph.options[f.path];
  const pending = [];
  for (const gn of graph.nodes) {
    const k = KINDS[gn.type];
    if (!k) throw new Error(tr('pipelines.v.unknownType', { type: gn.type }));
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

// ---------------------------------------------------------------- 元に戻す・写しと貼り付け

/** 編集の状態の写し（元に戻す・やり直すに使う）。 */
export function serialize(pl) {
  return JSON.stringify({ name: pl.name, options: pl.options, nodes: pl.nodes, edges: pl.edges });
}

export function restore(pl, s) {
  const v = JSON.parse(s);
  pl.name = v.name;
  pl.options = v.options;
  pl.nodes = v.nodes;
  pl.edges = v.edges;
}

/** 選んだ部品（と、その間の線）の写し。 */
export function copyNodes(pl, ids) {
  const set = new Set(ids);
  const nodes = pl.nodes.filter((n) => set.has(n.id));
  const index = new Map(nodes.map((n, i) => [n.id, i]));
  return {
    kind: 'senca.pipeline.clip.v1',
    nodes: nodes.map((n) => clone({ type: n.type, name: n.name, x: n.x, y: n.y, p: n.p, extra: n.extra, ins: n.ins })),
    edges: pl.edges.filter((e) => set.has(e.from.node) && set.has(e.to.node)).map((e) => ({ from: { i: index.get(e.from.node), port: e.from.port }, to: { i: index.get(e.to.node), port: e.to.port } })),
  };
}

/** 写しを貼り付ける（名前が重なれば新しい名前にする）。新しい部品の id の配列を返す。 */
export function pasteNodes(pl, clip, { dx = 40, dy = 40 } = {}) {
  if (!clip || clip.kind !== 'senca.pipeline.clip.v1' || !Array.isArray(clip.nodes)) return [];
  const made = [];
  for (const c of clip.nodes) {
    if (!KINDS[c.type]) { made.push(null); continue; }
    const name = pl.nodes.some((m) => m.name === c.name) ? freshName(pl, c.type, c.name) : c.name;
    const n = { id: uid('n'), type: c.type, name, x: snap(c.x + dx), y: snap(c.y + dy), p: clone(c.p) || {}, extra: clone(c.extra) || {} };
    if (c.ins) n.ins = clone(c.ins);
    pl.nodes.push(n);
    made.push(n);
  }
  for (const e of clip.edges || []) {
    const a = made[e.from.i];
    const b = made[e.to.i];
    if (a && b) pl.edges.push({ id: uid('e'), from: { node: a.id, port: e.from.port }, to: { node: b.id, port: e.to.port } });
  }
  return made.filter(Boolean).map((n) => n.id);
}

// ---------------------------------------------------------------- 入力の stream と配信中のトラック

/**
 * 配信中の stream のトラック（GET /streams の live.tracks）を source の設定に入れる。
 * 変えた項目の一覧（['video.codec', …]）を返す。
 */
export function applyLiveTracks(n, tracks) {
  if (!n || n.type !== 'source' || !Array.isArray(tracks)) return [];
  const changed = [];
  const set = (k, v) => { if (v !== undefined && v !== null && n.p[k] !== v) { n.p[k] = v; changed.push(k); } };
  const video = tracks.find((x) => VIDEO_CODECS.includes(x.codec));
  const audio = tracks.find((x) => AUDIO_CODECS.includes(x.codec));
  if (video) {
    set('video.codec', video.codec);
    if (video.width) set('video.width', video.width);
    if (video.height) set('video.height', video.height);
  }
  if (audio) set('audio.codec', audio.codec);
  else if (n.p['audio.codec'] && tracks.length) { n.p['audio.codec'] = ''; changed.push('audio.codec'); }
  return changed;
}

/** トラックの短い説明（「VP9 640×360 · Opus」）。 */
export function describeTracks(tracks) {
  if (!Array.isArray(tracks) || !tracks.length) return '';
  return tracks.map((x) => `${String(x.codec || '?').toUpperCase().replace('H26', 'H.26')}${x.width ? ` ${x.width}×${x.height}` : ''}`).join(' · ');
}

// ---------------------------------------------------------------- 例とスイッチャー

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

/**
 * 合成の入力の名前 → 元の stream の id（上流の source までたどる。サムネイルに使う）。
 * @returns {Record<string, string>}
 */
export function composeInputStreams(graph, composeId) {
  const nodes = new Map(((graph && graph.nodes) || []).map((n) => [n.id, n]));
  const c = nodes.get(composeId);
  if (!c || c.type !== 'compose') return {};
  const trace = (ref, seen = new Set()) => {
    if (!ref) return null;
    const id = String(ref).split('.')[0];
    const n = nodes.get(id);
    if (!n || seen.has(id)) return null;
    seen.add(id);
    if (n.type === 'source') return n.stream || null;
    if (n.input) return trace(n.input, seen);
    return null;
  };
  return Object.fromEntries(Object.entries(c.inputs || {}).map(([name, ref]) => [name, trace(ref)]).filter(([, s]) => s));
}

/** パイプラインの入力・出力の stream（一覧の「入力 → 出力」）。 */
export function graphStreams(graph) {
  const ns = (graph && graph.nodes) || [];
  return {
    inputs: [...new Set(ns.filter((n) => n.type === 'source').map((n) => n.stream).filter(Boolean))],
    outputs: [...new Set(ns.filter((n) => n.type === 'sink').map((n) => n.stream).filter(Boolean))],
  };
}
