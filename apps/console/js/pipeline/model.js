// パイプライン定義（senca-pipeline のグラフ JSON、`version: 1`）と編集用のモデル。画面に依存しない（node でも読める）。
//
// グラフの形は crates/senca-pipeline/src/model.rs が正（知らない項目は拒否される）。編集用のモデルは
// 部品（位置・平らにした設定）と線（出力ポート → 入力ポート）を持ち、toGraph / fromGraph で行き来する。
//
// - 部品の種類: source・decode・board・scale・crop・compose・audio_mix・encode・snapshot_on_change・sink
// - 入力の参照は `"部品"` か `"部品.ポート"`（source は video・audio、compose は program・preview、ほかは out）
// - 場所（どのノードで動かすか）は持たない（REQ-005）。画面の位置はグラフに入れられないので、画面側が別に覚える。
// - 取り込んだグラフの、この画面で編集しない項目（compose の images、board の corners など）はそのまま残す。

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

const VIDEO_CODECS = ['av1', 'vp9', 'vp8', 'h264', 'h265'];
const AUDIO_CODECS = ['opus', 'aac', 'pcma', 'pcmu'];
const ENCODE_CODECS = ['av1', 'vp9', 'vp8', 'h264', 'h265', 'opus'];
const FILTERS = ['bilinear', 'nearest', 'bicubic', 'lanczos3', 'area'];
const TRANSITION_KEYS = ['cut', 'fade', 'wipe_left', 'wipe_right'];
/** トランジションの種類 [値, 今の言語の名前]。 */
export const transitions = () => TRANSITION_KEYS.map((k) => [k, tr(`pipelines.transition.${k}`)]);
/** ロイヤリティのあるコーデック（royalty_free の stream が関わると使えない。REQ-007）。 */
export const ROYALTY_CODECS = new Set(['h264', 'h265', 'aac']);

const num = (path, key, extra = {}) => withLabel({ path, type: 'number', ...extra }, key);
const sel = (path, key, options, extra = {}) => {
  const o = withLabel({ path, type: 'select', ...extra }, key);
  return Object.defineProperty(o, 'options', { get: () => resolveOptions(typeof options === 'function' ? options() : options), enumerable: true });
};
const chk = (path, key, def) => withLabel({ path, type: 'check', def }, key);

/**
 * 部品の種類。
 * - in: 入力ポート（固定）。名前付きの入力（compose・audio_mix）は部品ごとの `ins` から作る。
 * - out: 出力ポートの名前（型は outType で上流から決める）。
 * - fields: 検査器に出す設定（path はグラフの中の場所）。
 */
export const KINDS = {
  source: {
    color: '#2f80ed', in: [], out: ['video', 'audio'],
    fields: [
      withLabel({ path: 'stream', type: 'stream', required: true }, 'pipelines.f.source.stream'),
      sel('video.codec', 'pipelines.f.source.video.codec', ['', ...VIDEO_CODECS]),
      num('video.track', 'pipelines.f.source.video.track', { min: 0, when: (p) => !!p['video.codec'] }),
      num('video.width', 'pipelines.f.source.video.width', { min: 16, when: (p) => !!p['video.codec'] }),
      num('video.height', 'pipelines.f.source.video.height', { min: 16, when: (p) => !!p['video.codec'] }),
      num('video.fps', 'pipelines.f.source.video.fps', { min: 1, step: 'any', when: (p) => !!p['video.codec'] }),
      sel('audio.codec', 'pipelines.f.source.audio.codec', ['', ...AUDIO_CODECS]),
      num('audio.track', 'pipelines.f.source.audio.track', { min: 0, when: (p) => !!p['audio.codec'] }),
      num('audio.sample_rate', 'pipelines.f.source.audio.sample_rate', { min: 8000, when: (p) => !!p['audio.codec'] }),
      num('audio.channels', 'pipelines.f.source.audio.channels', { min: 1, max: 8, when: (p) => !!p['audio.codec'] }),
    ],
    defaults: { 'video.codec': 'vp9' },
  },
  decode: {
    color: '#8e44ad', in: [['input', ['ev', 'ea', 'enc']]], out: ['out'],
    fields: [num('max_width', 'pipelines.f.decode.max_width', { min: 16 }), num('max_height', 'pipelines.f.decode.max_height', { min: 16 }), num('threads', 'pipelines.f.decode.threads', { min: 0, def: 0 })],
  },
  board: {
    color: '#16a085', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [
      sel('params.mode', 'pipelines.f.board.params.mode', [['whiteboard', 'pipelines.opt.whiteboard'], ['blackboard', 'pipelines.opt.blackboard']], { def: 'whiteboard' }),
      chk('params.auto_detect', 'pipelines.f.board.params.auto_detect', false),
      num('params.redetect_frames', 'pipelines.f.board.params.redetect_frames', { min: 0, def: 0 }),
      num('params.output_width', 'pipelines.f.board.params.output_width', { min: 16, def: 1280 }),
      num('params.output_height', 'pipelines.f.board.params.output_height', { min: 16, def: 720 }),
      chk('params.enhance', 'pipelines.f.board.params.enhance', true),
      num('params.stroke_gain', 'pipelines.f.board.params.stroke_gain', { step: 'any' }),
      num('params.chroma_gain', 'pipelines.f.board.params.chroma_gain', { step: 'any' }),
      chk('params.invert_to_white', 'pipelines.f.board.params.invert_to_white', false),
      chk('params.clahe', 'pipelines.f.board.params.clahe', false),
    ],
  },
  scale: {
    color: '#16a085', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [num('width', 'pipelines.f.scale.width', { min: 16, required: true, def: 1280 }), num('height', 'pipelines.f.scale.height', { min: 16, required: true, def: 720 }), sel('filter', 'pipelines.f.scale.filter', FILTERS, { def: 'bilinear' })],
  },
  crop: {
    color: '#16a085', in: [['input', ['v', 'raw']]], out: ['out'],
    fields: [num('x', 'pipelines.f.crop.x', { min: 0, required: true, def: 0 }), num('y', 'pipelines.f.crop.y', { min: 0, required: true, def: 0 }), num('width', 'pipelines.f.crop.width', { min: 2, required: true, def: 640 }), num('height', 'pipelines.f.crop.height', { min: 2, required: true, def: 360 })],
  },
  compose: {
    color: '#d35400', named: true, out: ['program', 'preview'],
    accept: ['v', 'raw'],
    fields: [
      num('canvas.width', 'pipelines.f.compose.canvas.width', { min: 16, required: true, def: 1280 }),
      num('canvas.height', 'pipelines.f.compose.canvas.height', { min: 16, required: true, def: 720 }),
      num('canvas.fps_num', 'pipelines.f.compose.canvas.fps_num', { min: 1, required: true, def: 30 }),
      num('canvas.fps_den', 'pipelines.f.compose.canvas.fps_den', { min: 1, def: 1 }),
      withLabel({ path: 'canvas.background', type: 'color', def: '#000000' }, 'pipelines.f.compose.canvas.background'),
      withLabel({ path: 'program', type: 'scene', required: true }, 'pipelines.f.compose.program'),
      withLabel({ path: 'preview', type: 'scene' }, 'pipelines.f.compose.preview'),
      sel('transition.kind', 'pipelines.f.compose.transition.kind', transitions, { def: 'cut' }),
      num('transition.duration_ms', 'pipelines.f.compose.transition.duration_ms', { min: 0, def: 0 }),
      num('missing.hold_ms', 'pipelines.f.compose.missing.hold_ms', { min: 0, def: 2000 }),
      withLabel({ path: 'missing.slate_color', type: 'color', def: '#000000' }, 'pipelines.f.compose.missing.slate_color'),
      num('threads', 'pipelines.f.compose.threads', { min: 0, def: 0 }),
    ],
  },
  audio_mix: {
    color: '#d35400', named: true, out: ['out'], accept: ['a', 'raw'],
    fields: [num('master_gain_db', 'pipelines.f.audio_mix.master_gain_db', { step: 0.5, def: 0 }), chk('limiter', 'pipelines.f.audio_mix.limiter', true)],
  },
  encode: {
    color: '#c0392b', in: [['input', ['v', 'a', 'raw']]], out: ['out'],
    fields: [
      sel('codec', 'pipelines.f.encode.codec', ENCODE_CODECS, { required: true, def: 'av1' }),
      num('bitrate_kbps', 'pipelines.f.encode.bitrate_kbps', { min: 1, required: true, def: 2000 }),
      num('gop', 'pipelines.f.encode.gop', { min: 1 }),
      chk('low_latency', 'pipelines.f.encode.low_latency', true),
      sel('rate_control', 'pipelines.f.encode.rate_control', ['cbr', 'vbr'], { def: 'cbr' }),
      num('width', 'pipelines.f.encode.width', { min: 16 }),
      num('height', 'pipelines.f.encode.height', { min: 16 }),
      num('frame_ms', 'pipelines.f.encode.frame_ms', { min: 2.5, step: 'any', when: (p) => p.codec === 'opus' }),
      num('threads', 'pipelines.f.encode.threads', { min: 0, def: 0 }),
    ],
  },
  snapshot_on_change: {
    color: '#7f8c8d', in: [['input', ['v', 'raw']]], out: [],
    fields: [
      sel('params.mode', 'pipelines.f.snapshot_on_change.params.mode', [['whiteboard', 'pipelines.opt.whiteboard'], ['blackboard', 'pipelines.opt.blackboard']], { def: 'whiteboard' }),
      chk('params.enabled', 'pipelines.f.snapshot_on_change.params.enabled', true),
      num('params.analysis_fps', 'pipelines.f.snapshot_on_change.params.analysis_fps', { min: 0, step: 'any', def: 2 }),
      num('params.stable_frames', 'pipelines.f.snapshot_on_change.params.stable_frames', { min: 1, def: 3 }),
      num('params.change_fraction', 'pipelines.f.snapshot_on_change.params.change_fraction', { min: 0, step: 'any', def: 0.004 }),
      num('params.jpeg_quality', 'pipelines.f.snapshot_on_change.params.jpeg_quality', { min: 1, max: 100, def: 85 }),
    ],
  },
  sink: {
    color: '#27ae60', in: [[L('pipelines.port.in1'), ['ev', 'ea', 'enc']], [L('pipelines.port.in2'), ['ev', 'ea', 'enc']]], out: [],
    fields: [withLabel({ path: 'stream', type: 'stream', required: true }, 'pipelines.f.sink.stream')],
  },
};

for (const [type, k] of Object.entries(KINDS)) withLabel(k, `pipelines.kind.${type}`);

export const OPTION_FIELDS = [
  sel('clock', 'pipelines.f.options.clock', [['realtime', 'pipelines.opt.realtime'], ['input', 'pipelines.opt.input']], { def: 'realtime' }),
  sel('overflow', 'pipelines.f.options.overflow', [['drop', 'pipelines.opt.drop'], ['block', 'pipelines.opt.block']], { def: 'drop' }),
  num('latency_ms', 'pipelines.f.options.latency_ms', { min: 0, def: 100 }),
  num('missing_timeout_ms', 'pipelines.f.options.missing_timeout_ms', { min: 1, def: 2000 }),
  num('block_timeout_ms', 'pipelines.f.options.block_timeout_ms', { min: 1, def: 2000 }),
  num('input_queue_aus', 'pipelines.f.options.input_queue_aus', { min: 1, def: 256 }),
  num('frame_queue', 'pipelines.f.options.frame_queue', { min: 1, def: 8 }),
  num('status_interval_ms', 'pipelines.f.options.status_interval_ms', { min: 0, def: 100 }),
  num('commit_timeout_ms', 'pipelines.f.options.commit_timeout_ms', { min: 1, def: 10000 }),
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
  if (from.node === to.node) return { ok: false, reason: tr('pipelines.v.sameNode') };
  const tn = byId(pl, to.node);
  if (!tn) return { ok: false, reason: tr('pipelines.v.noNode') };
  const port = inPorts(tn)[to.port];
  if (!port) return { ok: false, reason: tr('pipelines.v.noInput') };
  const t = outType(pl, from);
  if (!compatible(t, port.accept)) {
    const hint = (t === 'ev' || t === 'ea') && port.accept.includes('v') ? tr('pipelines.v.needDecode')
      : (t === 'v' || t === 'a') && port.accept.includes('ev') ? tr('pipelines.v.needEncode') : '';
    return { ok: false, reason: tr('pipelines.v.incompatible', { from: PORT_LABEL[t] || t, to: port.accept.map((x) => PORT_LABEL[x]).filter(Boolean)[0] || '', hint }) };
  }
  if (incoming(pl, to.node, to.port)) return { ok: false, reason: tr('pipelines.v.taken') };
  const e = { id: uid('e'), from: { ...from }, to: { ...to } };
  pl.edges.push(e);
  if (hasCycle(pl)) {
    pl.edges.pop();
    return { ok: false, reason: tr('pipelines.v.cycleEdge') };
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
  if (!isPipelineName(pl.name)) err(tr('pipelines.v.pipelineName'));
  if (!pl.nodes.some((n) => n.type === 'source')) err(tr('pipelines.v.noSource'));
  if (!pl.nodes.some((n) => n.type === 'sink' || n.type === 'snapshot_on_change')) err(tr('pipelines.v.noSink'));
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
    if (!port || !compatible(t, port.accept)) err(tr('pipelines.v.badEdge', { type: PORT_LABEL[t] || t, node: tn ? tn.name : '?' }), undefined, e.id);
    const fn = byId(pl, e.from.node);
    if (fn && fn.type === 'source') {
      const kind = e.from.port === 0 ? 'video' : 'audio';
      if (!fn.p[`${kind}.codec`]) err(tr('pipelines.v.noCodec', { node: fn.name, kind }), fn.id, e.id);
    }
  }
  for (const n of pl.nodes) {
    const k = KINDS[n.type];
    const ports = inPorts(n);
    ports.forEach((port, i) => {
      if (incoming(pl, n.id, i)) return;
      if (n.type === 'sink' && i === 1) return;
      err(tr('pipelines.v.unconnected', { node: n.name, port: port.name }), n.id);
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
      if (!pl.edges.some((e) => e.from.node === n.id && e.from.port === i)) err(tr('pipelines.v.unused', { node: n.name, port: o.name }), n.id);
    });
    for (const f of k.fields) {
      if (f.when && !f.when(n.p)) continue;
      const v = n.p[f.path];
      if (f.required && empty(v)) err(tr('pipelines.v.missingField', { node: n.name, field: f.label }), n.id);
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
    if (opt.royaltyFree) {
      const codecs = n.type === 'encode' ? [n.p.codec] : n.type === 'source' ? [n.p['video.codec'], n.p['audio.codec']] : [];
      for (const c of codecs) if (c && ROYALTY_CODECS.has(c)) err(tr('pipelines.v.royalty', { node: n.name, codec: c }), n.id);
    }
  }
  if (pl.nodes.some((n) => n.type === 'encode' && n.p.codec === 'h264') && !opt.royaltyFree) {
    warn(tr('pipelines.v.h264hw'));
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
