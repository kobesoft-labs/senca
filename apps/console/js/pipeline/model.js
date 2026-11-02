// パイプライン定義（DAG）のモデルと検査。画面に依存しない（node --test でも読める）。
//
// 定義は「何をするか」だけを持ち、「どこで動かすか」は持たない（REQ-005: ロジックと計算資源の分離）。
// 各部品は入力・出力のポートの種類（av = 多重化済み、video、audio、data、any）と必要な資源を宣言する。

export const PORT_LABEL = { av: '映像＋音声', video: '映像', audio: '音声', data: 'データ', any: '何でも' };

/** 部品の種類。params は検査器（inspector）に出す設定。 */
export const KINDS = {
  input: {
    label: '入力', color: '#2f80ed', in: [], out: ['av'],
    types: [['rtmp', 'RTMP（push）'], ['srt', 'SRT'], ['rtsp', 'RTSP（pull）'], ['webrtc', 'WebRTC（WHIP）'], ['hls_pull', 'HLS（pull）'], ['decklink', 'DeckLink'], ['paas', 'PaaS 連携']],
    params: [
      { key: 'stream', label: 'stream（push の受け口）', type: 'stream', when: (t) => ['rtmp', 'srt', 'webrtc'].includes(t) },
      { key: 'url', label: 'URL（pull 先）', type: 'text', when: (t) => ['rtsp', 'hls_pull', 'paas'].includes(t), required: true },
      { key: 'device', label: '装置（ボードの入力番号）', type: 'text', when: (t) => t === 'decklink', required: true },
    ],
  },
  demux: { label: '分離', color: '#8e44ad', in: ['av'], out: ['video', 'audio', 'data'], types: [['demux', 'トラックに分ける']], params: [] },
  mux: { label: '多重化', color: '#8e44ad', in: ['video', 'audio'], out: ['av'], types: [['mux', '時刻を揃えて 1 本に']], params: [] },
  switch: {
    label: '選択・切替', color: '#16a085', in: ['av', 'av'], out: ['av'],
    types: [['failover', 'バックアップへ自動切替'], ['manual', '手動切替（キーフレーム境界）']],
    params: [{ key: 'timeout_ms', label: '切替までの無信号（ミリ秒）', type: 'number', min: 100, def: 2000, when: (t) => t === 'failover' }],
  },
  compose: {
    label: '映像の合成', color: '#d35400', in: ['video', 'video'], out: ['video'],
    types: [['pip', 'PinP'], ['side', '並べる'], ['scene', 'シーン（スイッチャー）']],
    params: [
      { key: 'width', label: '幅', type: 'number', min: 16, def: 1920 },
      { key: 'height', label: '高さ', type: 'number', min: 16, def: 1080 },
      { key: 'fps', label: 'フレームレート', type: 'number', min: 1, def: 30 },
    ],
  },
  mix: {
    label: '音声ミキサー', color: '#d35400', in: ['audio', 'audio'], out: ['audio'], types: [['mix', 'ミキシング']],
    params: [{ key: 'sample_rate', label: 'サンプルレート', type: 'select', options: ['48000', '44100'], def: '48000' }],
  },
  transcode: {
    label: '変換', color: '#c0392b', in: ['any'], out: ['any'],
    types: [['video', '映像のトランスコード'], ['audio', '音声のトランスコード'], ['resize', 'リサイズ'], ['fps', 'フレームレート変換']],
    params: [
      { key: 'codec', label: 'コーデック', type: 'select', options: ['av1', 'vp9', 'vp8', 'h264', 'h265', 'opus', 'aac'], when: (t) => t === 'video' || t === 'audio', required: true },
      { key: 'bitrate_kbps', label: 'ビットレート（kbps）', type: 'number', min: 1, when: (t) => t === 'video' || t === 'audio' },
      { key: 'width', label: '幅', type: 'number', min: 16, when: (t) => t === 'resize' || t === 'video' },
      { key: 'height', label: '高さ', type: 'number', min: 16, when: (t) => t === 'resize' || t === 'video' },
      { key: 'fps', label: 'フレームレート', type: 'number', min: 1, when: (t) => t === 'fps' },
    ],
  },
  external: {
    label: '外部処理', color: '#7f8c8d', in: ['any'], out: ['any'], external: true,
    types: [['container', '隔離したコンテナ']],
    params: [
      { key: 'image', label: 'コンテナのイメージ', type: 'text', required: true },
      { key: 'exchange', label: '受け渡し', type: 'select', options: ['raw_shm', 'encoded_socket'], def: 'raw_shm' },
      { key: 'on_failure', label: '落ちた・遅れたとき', type: 'select', options: ['passthrough', 'black', 'stop'], def: 'passthrough' },
      { key: 'deadline_ms', label: 'フレームの期限（ミリ秒）', type: 'number', min: 1, def: 200 },
    ],
  },
  output: {
    label: '出力', color: '#27ae60', in: ['av'], out: [],
    types: [['hls', 'HLS'], ['recording', '録画'], ['webrtc', 'WebRTC（WHEP）'], ['srt', 'SRT 送出'], ['rtmp', 'RTMP 送出'], ['moq', 'MoQ'], ['paas', 'PaaS 連携']],
    params: [
      { key: 'url', label: '送り先 URL', type: 'text', when: (t) => ['srt', 'rtmp', 'paas'].includes(t), required: true },
      { key: 'container', label: 'コンテナ', type: 'select', options: ['fmp4', 'ts'], def: 'fmp4', when: (t) => t === 'hls' },
      { key: 'retention_days', label: '保持日数', type: 'number', min: 1, when: (t) => t === 'recording' },
    ],
  },
};

export const RESOURCES = [['cpu', 'CPU'], ['gpu', 'GPU（任意）'], ['gpu_nvidia', 'GPU（NVIDIA）'], ['gpu_intel', 'GPU（Intel）'], ['board', 'キャプチャボード']];

export const NODE_W = 172;
export const NODE_H = 60;

let seq = 0;
export function newId(prefix) {
  seq += 1;
  return `${prefix}${Date.now().toString(36)}${seq.toString(36)}`;
}

export function emptyPipeline(name = '新しいパイプライン') {
  return { id: newId('pl_'), remoteId: null, etag: null, name, redundancy: 'none', nodes: [], edges: [], updated_at: new Date().toISOString() };
}

export function addNode(pl, kind, x, y) {
  const k = KINDS[kind];
  const params = {};
  for (const p of k.params) if (p.def !== undefined) params[p.key] = p.def;
  const n = {
    id: newId('n'), kind, type: k.types[0][0], label: `${k.label} ${pl.nodes.filter((m) => m.kind === kind).length + 1}`,
    x: Math.round(x / 10) * 10, y: Math.round(y / 10) * 10, params, resources: kind === 'transcode' || kind === 'compose' ? 'gpu' : 'cpu',
  };
  pl.nodes.push(n);
  return n;
}

export function removeNode(pl, id) {
  pl.nodes = pl.nodes.filter((n) => n.id !== id);
  pl.edges = pl.edges.filter((e) => e.from.node !== id && e.to.node !== id);
}

export function portType(pl, ref, dir) {
  const n = pl.nodes.find((m) => m.id === ref.node);
  if (!n) return null;
  return KINDS[n.kind][dir][ref.port] || null;
}

/** 出力と入力の種類がつながるか。 */
export function compatible(outType, inType) {
  if (!outType || !inType) return false;
  return outType === inType || outType === 'any' || inType === 'any';
}

export function connect(pl, from, to) {
  if (from.node === to.node) return { ok: false, reason: '同じ部品にはつなげません' };
  const ot = portType(pl, from, 'out');
  const it = portType(pl, to, 'in');
  if (!compatible(ot, it)) return { ok: false, reason: `${PORT_LABEL[ot] || ot} は ${PORT_LABEL[it] || it} の入力につなげません${ot === 'av' ? '（分離を挟んでください）' : ''}` };
  if (pl.edges.some((e) => e.to.node === to.node && e.to.port === to.port)) return { ok: false, reason: 'この入力にはもうつながっています' };
  const e = { id: newId('e'), from: { ...from }, to: { ...to } };
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

const RF_BAD = new Set(['h264', 'h265', 'aac']);

/**
 * 検査（保存前・反映前）。issues: [{level: 'error'|'warn', node?, edge?, text}]
 * @param {object} pl
 * @param {{codecProfile?: string}} [opt]
 */
export function validate(pl, opt = {}) {
  const issues = [];
  if (!pl.name || !pl.name.trim()) issues.push({ level: 'error', text: 'パイプラインの名前がありません' });
  if (!pl.nodes.some((n) => n.kind === 'input')) issues.push({ level: 'error', text: '入力がありません' });
  if (!pl.nodes.some((n) => n.kind === 'output')) issues.push({ level: 'error', text: '出力がありません' });
  if (hasCycle(pl)) issues.push({ level: 'error', text: '輪（循環）があります。DAG にしてください' });
  for (const e of pl.edges) {
    const ot = portType(pl, e.from, 'out');
    const it = portType(pl, e.to, 'in');
    if (!compatible(ot, it)) issues.push({ level: 'error', edge: e.id, text: `つなげない組み合わせ: ${PORT_LABEL[ot] || ot} → ${PORT_LABEL[it] || it}` });
  }
  for (const n of pl.nodes) {
    const k = KINDS[n.kind];
    k.in.forEach((_t, i) => {
      if (!pl.edges.some((e) => e.to.node === n.id && e.to.port === i)) {
        const optional = (n.kind === 'switch' && i === 1) || ((n.kind === 'compose' || n.kind === 'mix') && i > 0) || (n.kind === 'mux' && i === 1);
        issues.push({ level: optional ? 'warn' : 'error', node: n.id, text: `「${n.label}」の入力 ${i + 1}（${PORT_LABEL[k.in[i]]}）がつながっていません` });
      }
    });
    if (k.out.length && !pl.edges.some((e) => e.from.node === n.id)) {
      issues.push({ level: 'warn', node: n.id, text: `「${n.label}」の出力がどこにもつながっていません` });
    }
    for (const p of k.params) {
      if (p.when && !p.when(n.type)) continue;
      const v = n.params[p.key];
      if (p.required && (v === undefined || v === null || v === '')) issues.push({ level: 'error', node: n.id, text: `「${n.label}」の「${p.label}」がありません` });
      if (p.type === 'number' && v !== undefined && v !== '' && v !== null && (Number.isNaN(Number(v)) || (p.min !== undefined && Number(v) < p.min))) {
        issues.push({ level: 'error', node: n.id, text: `「${n.label}」の「${p.label}」が範囲外です` });
      }
    }
    if (n.kind === 'transcode' && opt.codecProfile === 'royalty_free' && RF_BAD.has(n.params.codec)) {
      issues.push({ level: 'error', node: n.id, text: `「${n.label}」: royalty_free の方針では ${n.params.codec} を使えません（REQ-007）` });
    }
    if (n.kind === 'external') {
      issues.push({ level: 'warn', node: n.id, text: `「${n.label}」は利用者の拡張（外部処理）です。SENCA の中核ではなく、隔離したコンテナで動きます` });
    }
  }
  if (pl.redundancy === '1+1') issues.push({ level: 'warn', text: '二重化（1+1）: 2 ノードで同時に動かすので、資源は約 2 倍になります' });
  return issues;
}

/** API に送る定義（位置は ui に分ける。場所の指定は持たない）。 */
export function toDefinition(pl) {
  return {
    name: pl.name,
    redundancy: pl.redundancy,
    nodes: pl.nodes.map((n) => ({ id: n.id, kind: n.kind, type: n.type, label: n.label, params: n.params, resources: n.resources })),
    edges: pl.edges.map((e) => ({ from: `${e.from.node}.out${e.from.port}`, to: `${e.to.node}.in${e.to.port}` })),
    ui: { positions: Object.fromEntries(pl.nodes.map((n) => [n.id, [n.x, n.y]])) },
  };
}

/** toDefinition の逆（読み込み・取り込み）。 */
export function fromDefinition(def, base = emptyPipeline()) {
  const pos = (def.ui && def.ui.positions) || {};
  const pl = { ...base, name: def.name || base.name, redundancy: def.redundancy || 'none' };
  pl.nodes = (def.nodes || []).filter((n) => KINDS[n.kind]).map((n, i) => ({
    id: String(n.id), kind: n.kind, type: n.type || KINDS[n.kind].types[0][0], label: n.label || n.id,
    params: n.params || {}, resources: n.resources || 'cpu',
    x: pos[n.id] ? pos[n.id][0] : 40 + (i % 4) * 220, y: pos[n.id] ? pos[n.id][1] : 40 + Math.floor(i / 4) * 120,
  }));
  const parse = (s, dir) => {
    const m = new RegExp(`^(.+)\\.${dir}(\\d+)$`).exec(String(s));
    return m ? { node: m[1], port: Number(m[2]) } : null;
  };
  pl.edges = (def.edges || []).map((e) => ({ id: newId('e'), from: parse(e.from, 'out'), to: parse(e.to, 'in') }))
    .filter((e) => e.from && e.to && pl.nodes.some((n) => n.id === e.from.node) && pl.nodes.some((n) => n.id === e.to.node));
  return pl;
}

/** AC-P10 の例: 入力 2 つ → 分離 → 合成・ミキサー → 変換 → 多重化 → HLS と録画。 */
export function samplePipeline() {
  const pl = emptyPipeline('例: 2 カメラの合成 → HLS と録画');
  const a = addNode(pl, 'input', 30, 40); a.label = 'カメラ A';
  const b = addNode(pl, 'input', 30, 240); b.label = 'カメラ B'; b.type = 'rtsp'; b.params.url = 'rtsp://camera-b.example/stream1';
  const da = addNode(pl, 'demux', 250, 40); da.label = '分離 A';
  const db = addNode(pl, 'demux', 250, 240); db.label = '分離 B';
  const c = addNode(pl, 'compose', 480, 60); c.label = 'PinP 合成';
  const m = addNode(pl, 'mix', 480, 260); m.label = '音声ミキサー';
  const t = addNode(pl, 'transcode', 700, 60); t.label = 'AV1 変換'; t.params.codec = 'av1'; t.params.bitrate_kbps = 3000;
  const x = addNode(pl, 'mux', 920, 160); x.label = '多重化';
  const o1 = addNode(pl, 'output', 1140, 80); o1.label = 'HLS';
  const o2 = addNode(pl, 'output', 1140, 260); o2.label = '録画'; o2.type = 'recording';
  const link = (f, fp, to, tp) => connect(pl, { node: f.id, port: fp }, { node: to.id, port: tp });
  link(a, 0, da, 0); link(b, 0, db, 0);
  link(da, 0, c, 0); link(db, 0, c, 1);
  link(da, 1, m, 0); link(db, 1, m, 1);
  link(c, 0, t, 0); link(t, 0, x, 0); link(m, 0, x, 1);
  link(x, 0, o1, 0); link(x, 0, o2, 0);
  return pl;
}
