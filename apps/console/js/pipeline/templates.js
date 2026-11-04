// パイプラインのひな形（はじめての人が最初の 1 本を作るための例）。画面に依存しない。
//
// どれもロイヤリティフリーのコーデック（VP9 の入力 → AV1 ＋ Opus の出力）で、GPU の無いノードでも動く形にしてある。
// stream は空のまま。作るときに roles（入力と出力の部品）ごとに stream を選び、fromGraph で編集用のモデルにする。
// 文言は pipelines.tpl.<id>.*（title・desc）と pipelines.tpl.role.*。

const W = 1280;
const H = 720;
const full = (source) => ({ source, x: 0, y: 0, width: W, height: H });
const corner = (source) => ({ source, x: 896, y: 486, width: 352, height: 198, z: 1, border: { width: 4, color: '#ffffff' }, corner_radius: 12 });
const canvas = { width: W, height: H, fps_num: 30 };

/**
 * ひな形。
 * - id・icon
 * - roles: [{node, kind: 'input'|'output', role}]（role は pipelines.tpl.role.<role>）
 * - options: 作るときに選べる追加（[{id, def}]、文言は pipelines.tpl.opt.<id>）
 * - graph(opts): グラフ JSON（stream は空）
 */
export const TEMPLATES = [
  {
    id: 'pinp', icon: 'layers',
    roles: [{ node: 'main', kind: 'input', role: 'main' }, { node: 'corner', kind: 'input', role: 'corner' }, { node: 'out', kind: 'output', role: 'program' }],
    graph: () => ({
      version: 1, options: { clock: 'realtime' },
      nodes: [
        { type: 'source', id: 'main', stream: '', video: { codec: 'vp9' }, audio: { codec: 'opus' } },
        { type: 'source', id: 'corner', stream: '', video: { codec: 'vp9' } },
        { type: 'decode', id: 'dec_main', input: 'main.video' },
        { type: 'decode', id: 'dec_corner', input: 'corner.video' },
        {
          type: 'compose', id: 'prod', inputs: { main: 'dec_main', corner: 'dec_corner' }, canvas,
          scenes: { pinp: { layers: [full('main'), corner('corner')] }, main: { layers: [full('main')] }, corner: { layers: [full('corner')] } },
          program: 'pinp', preview: 'main', transition: { kind: 'fade', duration_ms: 400 },
        },
        { type: 'encode', id: 'venc', input: 'prod', codec: 'av1', bitrate_kbps: 2500 },
        { type: 'sink', id: 'out', inputs: ['venc', 'main.audio'], stream: '' },
      ],
    }),
  },
  {
    id: 'switcher', icon: 'switcher',
    roles: [{ node: 'cam1', kind: 'input', role: 'camera1' }, { node: 'cam2', kind: 'input', role: 'camera2' }, { node: 'out', kind: 'output', role: 'program' }, { node: 'pvw', kind: 'output', role: 'preview', option: 'preview' }],
    options: [{ id: 'preview', def: false }],
    graph: (o = {}) => ({
      version: 1, options: { clock: 'realtime' },
      nodes: [
        { type: 'source', id: 'cam1', stream: '', video: { codec: 'vp9' }, audio: { codec: 'opus' } },
        { type: 'source', id: 'cam2', stream: '', video: { codec: 'vp9' }, audio: { codec: 'opus' } },
        { type: 'decode', id: 'dec1', input: 'cam1.video' },
        { type: 'decode', id: 'dec2', input: 'cam2.video' },
        { type: 'decode', id: 'adec1', input: 'cam1.audio' },
        { type: 'decode', id: 'adec2', input: 'cam2.audio' },
        {
          type: 'compose', id: 'prod', inputs: { cam1: 'dec1', cam2: 'dec2' }, canvas,
          scenes: {
            cam1: { layers: [full('cam1')] },
            cam2: { layers: [full('cam2')] },
            pinp: { layers: [full('cam1'), corner('cam2')] },
            side: { layers: [{ source: 'cam1', x: 0, y: 180, width: 640, height: 360 }, { source: 'cam2', x: 640, y: 180, width: 640, height: 360 }] },
          },
          program: 'cam1', preview: 'cam2', transition: { kind: 'fade', duration_ms: 500 },
        },
        { type: 'audio_mix', id: 'mix', inputs: { mic1: { from: 'adec1' }, mic2: { from: 'adec2', gain_db: -6 } } },
        { type: 'encode', id: 'venc', input: 'prod', codec: 'av1', bitrate_kbps: 2500 },
        { type: 'encode', id: 'aenc', input: 'mix', codec: 'opus', bitrate_kbps: 128 },
        { type: 'sink', id: 'out', inputs: ['venc', 'aenc'], stream: '' },
        ...(o.preview ? [
          { type: 'encode', id: 'pvenc', input: 'prod.preview', codec: 'av1', bitrate_kbps: 800 },
          { type: 'sink', id: 'pvw', inputs: ['pvenc'], stream: '' },
        ] : []),
      ],
    }),
  },
  {
    id: 'lecture', icon: 'presentation',
    roles: [{ node: 'teacher', kind: 'input', role: 'teacher' }, { node: 'board', kind: 'input', role: 'board' }, { node: 'out', kind: 'output', role: 'program' }],
    graph: () => ({
      version: 1, options: { clock: 'realtime' },
      nodes: [
        { type: 'source', id: 'teacher', stream: '', video: { codec: 'vp9' }, audio: { codec: 'opus' } },
        { type: 'source', id: 'board', stream: '', video: { codec: 'vp9' } },
        { type: 'decode', id: 'dec_teacher', input: 'teacher.video' },
        { type: 'decode', id: 'dec_board', input: 'board.video' },
        { type: 'board', id: 'fix_board', input: 'dec_board', params: { mode: 'whiteboard', auto_detect: true, redetect_frames: 300, output_width: W, output_height: H, enhance: true } },
        { type: 'snapshot_on_change', id: 'notes', input: 'fix_board', params: { mode: 'whiteboard', enabled: true } },
        {
          type: 'compose', id: 'prod', inputs: { board: 'fix_board', teacher: 'dec_teacher' }, canvas,
          scenes: { lecture: { layers: [full('board'), corner('teacher')] }, board: { layers: [full('board')] }, teacher: { layers: [full('teacher')] } },
          program: 'lecture', preview: 'teacher', transition: { kind: 'fade', duration_ms: 400 },
        },
        { type: 'encode', id: 'venc', input: 'prod', codec: 'av1', bitrate_kbps: 2000 },
        { type: 'sink', id: 'out', inputs: ['venc', 'teacher.audio'], stream: '' },
      ],
    }),
  },
  {
    id: 'audio', icon: 'sliders',
    roles: [{ node: 'mic1', kind: 'input', role: 'mic1' }, { node: 'mic2', kind: 'input', role: 'mic2' }, { node: 'out', kind: 'output', role: 'mix' }],
    graph: () => ({
      version: 1, options: { clock: 'realtime' },
      nodes: [
        { type: 'source', id: 'mic1', stream: '', audio: { codec: 'opus' } },
        { type: 'source', id: 'mic2', stream: '', audio: { codec: 'opus' } },
        { type: 'decode', id: 'dec1', input: 'mic1.audio' },
        { type: 'decode', id: 'dec2', input: 'mic2.audio' },
        { type: 'audio_mix', id: 'mix', inputs: { mic1: { from: 'dec1' }, mic2: { from: 'dec2' } }, limiter: true },
        { type: 'encode', id: 'aenc', input: 'mix', codec: 'opus', bitrate_kbps: 128 },
        { type: 'sink', id: 'out', inputs: ['aenc'], stream: '' },
      ],
    }),
  },
  {
    id: 'ladder', icon: 'network',
    roles: [{ node: 'src', kind: 'input', role: 'source' }, { node: 'hd', kind: 'output', role: 'hd' }, { node: 'sd', kind: 'output', role: 'sd' }],
    graph: () => ({
      version: 1, options: { clock: 'realtime' },
      nodes: [
        { type: 'source', id: 'src', stream: '', video: { codec: 'vp9' }, audio: { codec: 'opus' } },
        { type: 'decode', id: 'dec', input: 'src.video' },
        { type: 'scale', id: 'to720', input: 'dec', width: 1280, height: 720 },
        { type: 'scale', id: 'to360', input: 'dec', width: 640, height: 360 },
        { type: 'encode', id: 'enc720', input: 'to720', codec: 'av1', bitrate_kbps: 2500 },
        { type: 'encode', id: 'enc360', input: 'to360', codec: 'av1', bitrate_kbps: 700 },
        { type: 'sink', id: 'hd', inputs: ['enc720', 'src.audio'], stream: '' },
        { type: 'sink', id: 'sd', inputs: ['enc360', 'src.audio'], stream: '' },
      ],
    }),
  },
  {
    id: 'transcode', icon: 'shrink',
    roles: [{ node: 'src', kind: 'input', role: 'source' }, { node: 'out', kind: 'output', role: 'output' }],
    graph: () => ({
      version: 1, options: { clock: 'realtime' },
      nodes: [
        { type: 'source', id: 'src', stream: '', video: { codec: 'vp9' }, audio: { codec: 'opus' } },
        { type: 'decode', id: 'dec', input: 'src.video' },
        { type: 'encode', id: 'venc', input: 'dec', codec: 'av1', bitrate_kbps: 1500 },
        { type: 'sink', id: 'out', inputs: ['venc', 'src.audio'], stream: '' },
      ],
    }),
  },
  {
    id: 'blank', icon: 'plus', roles: [],
    graph: () => ({ version: 1, nodes: [] }),
  },
];

export const templateById = (id) => TEMPLATES.find((x) => x.id === id) || null;

/** ひな形の数（入力・出力・部品）。 */
export function templateStats(tpl, opts = {}) {
  const g = tpl.graph(opts);
  return {
    inputs: g.nodes.filter((n) => n.type === 'source').length,
    outputs: g.nodes.filter((n) => n.type === 'sink').length,
    nodes: g.nodes.length,
    compose: g.nodes.some((n) => n.type === 'compose'),
    codecs: [...new Set(g.nodes.filter((n) => n.type === 'encode').map((n) => n.codec))],
  };
}

/**
 * ひな形に stream を入れたグラフ。streams: {部品の id: stream の id}
 */
export function fillTemplate(tpl, streams = {}, opts = {}) {
  const g = tpl.graph(opts);
  for (const n of g.nodes) if ((n.type === 'source' || n.type === 'sink') && streams[n.id]) n.stream = streams[n.id];
  return g;
}
