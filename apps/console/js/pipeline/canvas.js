// パイプラインのグラフを描いて操作する面（SVG）。編集の画面（editor.js）から使う。
//
// - 表示: 部品のカード（分類の色・アイコン・名前・要約・ポートの名前）、線（映像は青・音声は緑。符号化済みのポートは菱形、生は丸）
// - 動かす: ホイール・2 本指 = 動かす、Ctrl/⌘＋ホイール・つまむ = 拡大縮小、Space＋ドラッグ・中ボタン = 動かす、
//   背景のドラッグ = 囲んで選ぶ（タッチでは動かす）、部品のドラッグ = 移動（格子に吸着。Alt で吸着しない）
// - つなぐ: 出力の点から入力の点（か部品）へ。つなげる点だけが光り、つなげない点に重ねると理由を出す。
//   つながった入力の点を引くと線を外して付け替えられる。何も無い所に落とすと、つなげる部品の候補を出す。
// - 小さな地図（右下）と、拡大縮小・全体を表示・自動の配置のボタン（左下）。
//
// モデル（js/pipeline/model.js の編集用のモデル）は呼ぶ側が持つ。変えたら hooks.onChange(what) を呼ぶ。

import { h, s, replace } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';
import { tooltip } from '../ui/overlay.js';
import {
  KINDS, NODE_W, NODE_HEAD, GRID, nodeHeight, portOffset, inPorts, outPorts, outType, portLook, checkConnect, connect,
  kindsAccepting, kindsProducing, topoOrder, edgeInto, nodeById,
} from './model.js';

const MIN_K = 0.25;
const MAX_K = 2;
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
const snapV = (v) => Math.round(v / GRID) * GRID;

function edgeD(a, b) {
  const dx = b.x >= a.x ? Math.max(40, (b.x - a.x) / 2) : Math.max(80, (a.x - b.x) / 2);
  return `M${a.x},${a.y} C${a.x + dx},${a.y} ${b.x - dx},${b.y} ${b.x},${b.y}`;
}

const trunc = (str, n) => (str.length > n ? `${str.slice(0, n - 1)}…` : str);

/**
 * @param {{
 *   model: () => object,
 *   summary: (n: object) => string,
 *   marks: () => {nodes: Map<string, 'error'|'warn'>, edges: Set<string>},
 *   runtime: () => (Map<string, string>|null),
 *   onSelect: () => void,
 *   onChange: (what: string, opt?: object) => void,
 *   onAddRequest: (type: string, at: {x:number,y:number}, link?: object) => void,
 *   onOpenNode: (id: string) => void,
 *   onEmptyAction: (what: string) => void,
 *   onAutoLayout: () => void,
 * }} hooks
 */
export function createCanvas(hooks) {
  const view = { x: 40, y: 40, k: 1 };
  const sel = { nodes: new Set(), edge: null };
  let wire = null; // {from:{node,port}|null, to:{node,port}|null, a, b, type, accept, detached}
  let nodeEls = new Map();
  let edgeEls = new Map();
  let spaceDown = false;
  let running = false;
  let quietFocus = false;

  const pl = () => hooks.model();

  // ---------------------------------------------------------------- 要素
  const defs = s('defs', null,
    s('pattern', { id: 'pl-dots', width: GRID, height: GRID, patternUnits: 'userSpaceOnUse' }, s('circle', { cx: 1, cy: 1, r: 1, class: 'pl-dot' })),
    // 部品の名前が枠からはみ出さないように（部品ごとの座標で効く）
    s('clipPath', { id: 'pl-clip-head', clipPathUnits: 'userSpaceOnUse' }, s('rect', { x: 0, y: 0, width: NODE_W - 10, height: NODE_HEAD })));
  const bg = s('rect', { class: 'pl-bg', x: -50000, y: -50000, width: 100000, height: 100000, fill: 'url(#pl-dots)' });
  const edgesG = s('g', { class: 'pl-edges' });
  const nodesG = s('g', { class: 'pl-nodes' });
  const overlayG = s('g', { class: 'pl-overlay' });
  const vp = s('g', { class: 'pl-viewport' }, bg, edgesG, nodesG, overlayG);
  const helpId = `pl-kbd-help-${Math.random().toString(36).slice(2, 7)}`;
  const svg = s('svg', { class: 'pl-svg', role: 'application', 'aria-roledescription': t('pipelines.canvas.role'), 'aria-label': t('pipelines.graph'), 'aria-describedby': helpId, tabindex: '-1' }, defs, vp);
  const marquee = h('div', { class: 'pl-marquee', hidden: true });
  const hint = h('div', { class: 'pl-wire-hint', role: 'status', 'aria-live': 'polite', hidden: true });
  const kbdHelp = h('p', { id: helpId, class: 'visually-hidden' }, t('pipelines.canvas.keys'));
  const zoomLabel = h('span', { class: 'num' }, '100%');
  const mini = s('svg', { class: 'pl-mini-svg', 'aria-hidden': 'true' });
  const miniBox = h('div', { class: 'pl-minimap', title: t('pipelines.canvas.minimap') }, mini);
  const emptyBox = h('div', { class: 'pl-canvas-empty', hidden: true });
  const menuHost = h('div', { class: 'pl-menu-host' });

  const ctl = (name, label, fn, kbd) => {
    const b = h('button', { type: 'button', class: 'btn ghost icon-only sm', 'aria-label': label, onClick: fn }, icon(name));
    tooltip(b, label, { kbd });
    return b;
  };
  const zoomReset = h('button', { type: 'button', class: 'btn ghost sm pl-zoom-pct', 'aria-label': t('pipelines.canvas.zoomReset'), onClick: () => zoomTo(1) }, zoomLabel);
  tooltip(zoomReset, t('pipelines.canvas.zoomReset'), { kbd: '1' });
  const controls = h('div', { class: 'pl-controls', role: 'toolbar', 'aria-label': t('pipelines.canvas.view') },
    ctl('zoom-out', t('pipelines.canvas.zoomOut'), () => zoomBy(1 / 1.2), '−'),
    zoomReset,
    ctl('zoom-in', t('pipelines.canvas.zoomIn'), () => zoomBy(1.2), '+'),
    ctl('fit', t('pipelines.canvas.fit'), () => fit(), '0'),
    h('span', { class: 'pl-controls-sep', 'aria-hidden': 'true' }),
    ctl('network', t('pipelines.canvas.autoLayout'), () => hooks.onAutoLayout(), 'L'),
    ctl('map', t('pipelines.canvas.toggleMinimap'), () => { el.classList.toggle('no-minimap'); drawMini(); }));
  const legend = h('div', { class: 'pl-legend', 'aria-hidden': 'true' },
    h('span', null, h('i', { class: 'lg-port enc' }), t('pipelines.legend.enc')),
    h('span', null, h('i', { class: 'lg-port' }), t('pipelines.legend.dec')),
    h('span', null, h('i', { class: 'lg-line video' }), t('pipelines.media.videoCap')),
    h('span', null, h('i', { class: 'lg-line audio' }), t('pipelines.media.audioCap')));
  const el = h('div', { class: 'pl-canvas', 'data-testid': 'pipeline-canvas' }, svg, marquee, hint, emptyBox, controls, legend, miniBox, menuHost, kbdHelp);

  // ---------------------------------------------------------------- 座標
  const applyView = () => {
    vp.setAttribute('transform', `translate(${view.x} ${view.y}) scale(${view.k})`);
    zoomLabel.textContent = `${Math.round(view.k * 100)}%`;
    el.style.setProperty('--pl-k', String(view.k));
    // 小さく表示しているときは、名前だけを大きく出す（要約とポートの名前は隠す）
    svg.classList.toggle('lod-low', view.k < 0.72);
    drawMiniView();
  };
  const toWorld = (cx, cy) => {
    const r = svg.getBoundingClientRect();
    return { x: (cx - r.left - view.x) / view.k, y: (cy - r.top - view.y) / view.k };
  };
  const zoomAt = (k, cx, cy) => {
    const nk = clamp(k, MIN_K, MAX_K);
    const r = svg.getBoundingClientRect();
    const px = cx - r.left;
    const py = cy - r.top;
    view.x = px - ((px - view.x) / view.k) * nk;
    view.y = py - ((py - view.y) / view.k) * nk;
    view.k = nk;
    applyView();
  };
  const center = () => { const r = svg.getBoundingClientRect(); return { cx: r.left + r.width / 2, cy: r.top + r.height / 2 }; };
  function zoomBy(f) { const { cx, cy } = center(); zoomAt(view.k * f, cx, cy); }
  function zoomTo(k) { const { cx, cy } = center(); zoomAt(k, cx, cy); }

  const bounds = (ids) => {
    const ns = pl().nodes.filter((n) => !ids || ids.includes(n.id));
    if (!ns.length) return null;
    const x1 = Math.min(...ns.map((n) => n.x));
    const y1 = Math.min(...ns.map((n) => n.y));
    const x2 = Math.max(...ns.map((n) => n.x + NODE_W));
    const y2 = Math.max(...ns.map((n) => n.y + nodeHeight(n)));
    return { x1, y1, x2, y2 };
  };

  /** 全体（か ids の部品）が見えるように拡大縮小する。 */
  function fit(ids, { maxK = 1.1 } = {}) {
    const b = bounds(ids);
    const r = svg.getBoundingClientRect();
    if (!b || !r.width) { view.x = 40; view.y = 40; view.k = 1; applyView(); return; }
    const pad = r.width < 500 ? 16 : 48;
    const fitK = clamp(Math.min((r.width - pad * 2) / (b.x2 - b.x1), (r.height - pad * 2) / (b.y2 - b.y1), maxK), MIN_K, MAX_K);
    // 狭い面では全体を入れると読めないので、読める大きさで左（入力の側）から見せる
    const k = r.width < 480 && !ids ? Math.max(fitK, 0.5) : fitK;
    view.k = k;
    view.x = k > fitK ? pad - b.x1 * k : (r.width - (b.x2 - b.x1) * k) / 2 - b.x1 * k;
    view.y = (r.height - (b.y2 - b.y1) * k) / 2 - b.y1 * k;
    applyView();
  }

  /** 部品が見えるところまで動かす（キーボードで選んだとき）。 */
  function reveal(id) {
    const n = nodeById(pl(), id);
    if (!n) return;
    const r = svg.getBoundingClientRect();
    const x1 = n.x * view.k + view.x;
    const y1 = n.y * view.k + view.y;
    const x2 = (n.x + NODE_W) * view.k + view.x;
    const y2 = (n.y + nodeHeight(n)) * view.k + view.y;
    const m = 24;
    if (x1 < m) view.x += m - x1; else if (x2 > r.width - m) view.x -= x2 - (r.width - m);
    if (y1 < m) view.y += m - y1; else if (y2 > r.height - m) view.y -= y2 - (r.height - m);
    applyView();
  }

  /** 見えている場所の真ん中（新しい部品を置く場所）。重なれば少しずらす。 */
  function freeSpot() {
    const r = svg.getBoundingClientRect();
    let p = toWorld(r.left + r.width / 2 - (NODE_W * view.k) / 2, r.top + r.height / 3);
    for (let i = 0; i < 12 && pl().nodes.some((n) => Math.abs(n.x - p.x) < NODE_W * 0.6 && Math.abs(n.y - p.y) < 60); i += 1) p = { x: p.x + 40, y: p.y + 40 };
    return { x: snapV(p.x), y: snapV(p.y) };
  }

  // ---------------------------------------------------------------- 描画
  const portPos = (n, dir, i) => { const o = portOffset(n, dir, i); return { x: n.x + o.x, y: n.y + o.y }; };

  const portShape = (type, connected) => {
    const look = portLook(type);
    const cls = ['pl-port-shape', look.media, connected && 'on'];
    return look.encoded
      ? s('rect', { class: cls, x: -5.5, y: -5.5, width: 11, height: 11, rx: 2, transform: 'rotate(45)' })
      : s('circle', { class: cls, r: 6 });
  };

  const nodeLabel = (n, mark) => {
    const p = pl();
    const ins = inPorts(n).map((port, i) => {
      const e = edgeInto(p, n.id, i);
      const up = e && nodeById(p, e.from.node);
      return up ? t('pipelines.canvas.ariaIn', { port: port.name, from: up.name }) : t('pipelines.canvas.ariaInFree', { port: port.name });
    });
    const outs = p.edges.filter((e) => e.from.node === n.id).map((e) => (nodeById(p, e.to.node) || {}).name).filter(Boolean);
    return [
      t('pipelines.canvas.ariaNode', { kind: KINDS[n.type].label, node: n.name }),
      ...ins,
      outs.length ? t('pipelines.canvas.ariaOut', { to: outs.join(', ') }) : null,
      mark === 'error' ? t('pipelines.canvas.ariaError') : mark === 'warn' ? t('pipelines.canvas.ariaWarn') : null,
    ].filter(Boolean).join('. ');
  };

  const drawNode = (n, marks, rt) => {
    const k = KINDS[n.type];
    const hh = nodeHeight(n);
    const mark = marks.nodes.get(n.id);
    const isSel = sel.nodes.has(n.id);
    const live = rt && rt.get(n.name);
    const g = s('g', {
      class: ['pl-node', `k-${k.cat}`, isSel && 'selected', mark && `mark-${mark}`],
      transform: `translate(${n.x},${n.y})`, tabindex: '0', role: 'button', 'aria-pressed': String(isSel),
      'aria-label': nodeLabel(n, mark), 'data-node': n.name, 'data-id': n.id,
    },
    s('rect', { class: 'shadow', x: 0, y: 2, width: NODE_W, height: hh, rx: 12 }),
    s('rect', { class: 'body', width: NODE_W, height: hh, rx: 12 }),
    s('path', { class: 'head', d: `M0 12 A12 12 0 0 1 12 0 H${NODE_W - 12} A12 12 0 0 1 ${NODE_W} 12 V${NODE_HEAD} H0 Z` }),
    s('line', { class: 'head-line', x1: 0, x2: NODE_W, y1: NODE_HEAD, y2: NODE_HEAD }),
    s('rect', { class: 'chip', x: 11, y: 10, width: 28, height: 28, rx: 8 }),
    (() => { const ic = icon(k.icon); ic.setAttribute('x', '17'); ic.setAttribute('y', '16'); ic.setAttribute('width', '16'); ic.setAttribute('height', '16'); ic.setAttribute('class', 'icon chip-icon'); return ic; })(),
    s('text', { class: 'title', x: 48, y: 22, 'clip-path': 'url(#pl-clip-head)' }, trunc(n.name, 22)),
    s('text', { class: 'sub', x: 48, y: 38, 'clip-path': 'url(#pl-clip-head)' }, trunc(hooks.summary(n) || k.label, 26)),
    s('rect', { class: 'ring', x: -3, y: -3, width: NODE_W + 6, height: hh + 6, rx: 15 }));
    if (mark) {
      g.appendChild(s('g', { class: `badge-mark ${mark}`, transform: `translate(${NODE_W - 14},14)` },
        s('circle', { r: 8 }), s('text', { y: 4, 'text-anchor': 'middle' }, '!')));
    } else if (live) {
      g.appendChild(s('circle', { class: `live-dot ${live}`, cx: NODE_W - 14, cy: 14, r: 5 }, s('title', null, t(`pipelines.live.${live}`))));
    }
    const p = pl();
    inPorts(n).forEach((port, i) => {
      const o = portOffset(n, 'in', i);
      const e = edgeInto(p, n.id, i);
      const ty = e ? outType(p, e.from) : (port.accept.includes('ev') || port.accept.includes('ea') ? 'enc' : port.accept.includes('a') && !port.accept.includes('v') ? 'a' : port.accept.includes('v') && !port.accept.includes('a') ? 'v' : 'raw');
      const pg = s('g', { class: 'pl-port in', transform: `translate(${o.x},${o.y})`, 'data-port': `in:${n.id}:${i}` },
        s('circle', { class: 'hit', r: 12 }), portShape(ty, !!e), s('circle', { class: 'halo', r: 11 }));
      g.appendChild(pg);
      g.appendChild(s('text', { class: 'port-label', x: 14, y: o.y + 4 }, trunc(port.name, 14)));
    });
    outPorts(n).forEach((port, i) => {
      const o = portOffset(n, 'out', i);
      const ty = outType(p, { node: n.id, port: i });
      const used = p.edges.some((e) => e.from.node === n.id && e.from.port === i);
      const pg = s('g', { class: 'pl-port out', transform: `translate(${o.x},${o.y})`, 'data-port': `out:${n.id}:${i}` },
        s('circle', { class: 'hit', r: 12 }), portShape(ty, used), s('circle', { class: 'halo', r: 11 }));
      g.appendChild(pg);
      g.appendChild(s('text', { class: 'port-label out', x: NODE_W - 14, y: o.y + 4, 'text-anchor': 'end' }, port.name));
    });
    bindNode(g, n);
    return g;
  };

  const drawEdge = (e, marks) => {
    const p = pl();
    const a = nodeById(p, e.from.node);
    const b = nodeById(p, e.to.node);
    if (!a || !b) return null;
    const d = edgeD(portPos(a, 'out', e.from.port), portPos(b, 'in', e.to.port));
    const look = portLook(outType(p, e.from));
    const isSel = sel.edge === e.id;
    const hit = s('path', { d, class: 'pl-edge-hit', 'data-edge': e.id });
    const path = s('path', { d, class: ['pl-edge', look.media, look.encoded && 'enc', isSel && 'selected', marks.edges.has(e.id) && 'invalid'], 'data-edge': e.id });
    const pick = (ev) => {
      if (ev.button !== 0) return;
      ev.stopPropagation();
      sel.nodes.clear();
      sel.edge = e.id;
      draw();
      hooks.onSelect();
    };
    hit.addEventListener('pointerdown', pick);
    path.addEventListener('pointerdown', pick);
    edgeEls.set(e.id, { hit, path, e });
    return [hit, path];
  };

  function draw() {
    const p = pl();
    const active = document.activeElement;
    const refocus = active && svg.contains(active) && active.getAttribute('data-id');
    const marks = hooks.marks();
    const rt = hooks.runtime();
    nodeEls = new Map();
    edgeEls = new Map();
    for (const id of [...sel.nodes]) if (!nodeById(p, id)) sel.nodes.delete(id);
    if (sel.edge && !p.edges.some((e) => e.id === sel.edge)) sel.edge = null;
    replace(edgesG, p.edges.map((e) => drawEdge(e, marks)));
    const order = topoOrder(p);
    const nodes = order.map((id) => nodeById(p, id)).filter(Boolean);
    // 選んだ部品は上に描く（重なったときに見える）。Tab の順は流れの順のまま（後ろで並べ替える）
    replace(nodesG, nodes.map((n) => { const g = drawNode(n, marks, rt); nodeEls.set(n.id, g); return g; }));
    replace(overlayG);
    if (sel.edge) drawEdgeTools();
    svg.classList.toggle('is-running', running);
    emptyBox.hidden = p.nodes.length > 0;
    if (!emptyBox.hidden && !emptyBox.firstChild) buildEmpty();
    drawMini();
    if (refocus) { const g = nodeEls.get(refocus); if (g) { quietFocus = true; g.focus({ preventScroll: true }); quietFocus = false; } }
  }

  /** 選んだ線の真ん中に「外す」のボタン。 */
  const drawEdgeTools = () => {
    const ee = edgeEls.get(sel.edge);
    if (!ee) return;
    const len = ee.path.getTotalLength ? ee.path.getTotalLength() : 0;
    const m = len ? ee.path.getPointAtLength(len / 2) : null;
    if (!m) return;
    const btn = s('g', { class: 'pl-edge-del', transform: `translate(${m.x},${m.y})`, role: 'button', tabindex: '0', 'aria-label': t('pipelines.deleteEdge') },
      s('circle', { r: 11 }), s('path', { d: 'M-4,-4 L4,4 M4,-4 L-4,4' }));
    const del = (ev) => { ev.stopPropagation(); ev.preventDefault(); removeEdge(sel.edge); };
    btn.addEventListener('pointerdown', del);
    btn.addEventListener('keydown', (ev) => { if (ev.key === 'Enter' || ev.key === ' ') del(ev); });
    overlayG.appendChild(btn);
  };

  const removeEdge = (id) => {
    const p = pl();
    p.edges = p.edges.filter((x) => x.id !== id);
    sel.edge = null;
    hooks.onChange('disconnect');
  };

  /** ドラッグ中は動いた部品と、その線だけを描き直す。 */
  const moveDraw = (ids) => {
    const p = pl();
    for (const id of ids) {
      const n = nodeById(p, id);
      const g = nodeEls.get(id);
      if (n && g) g.setAttribute('transform', `translate(${n.x},${n.y})`);
    }
    for (const { hit, path, e } of edgeEls.values()) {
      if (!ids.has(e.from.node) && !ids.has(e.to.node)) continue;
      const a = nodeById(p, e.from.node);
      const b = nodeById(p, e.to.node);
      const d = edgeD(portPos(a, 'out', e.from.port), portPos(b, 'in', e.to.port));
      hit.setAttribute('d', d);
      path.setAttribute('d', d);
    }
    replace(overlayG);
    drawMini();
  };

  // ---------------------------------------------------------------- 小さな地図
  let miniScale = 1;
  let miniOrigin = { x: 0, y: 0 };
  const miniView = s('rect', { class: 'pl-mini-view' });
  function drawMini() {
    if (el.classList.contains('no-minimap')) return;
    const p = pl();
    const b = bounds() || { x1: 0, y1: 0, x2: 400, y2: 300 };
    const r = svg.getBoundingClientRect();
    const vx1 = -view.x / view.k;
    const vy1 = -view.y / view.k;
    const vx2 = vx1 + (r.width || 1) / view.k;
    const vy2 = vy1 + (r.height || 1) / view.k;
    const x1 = Math.min(b.x1, vx1) - 40;
    const y1 = Math.min(b.y1, vy1) - 40;
    const x2 = Math.max(b.x2, vx2) + 40;
    const y2 = Math.max(b.y2, vy2) + 40;
    const W = 168;
    const H = 108;
    miniScale = Math.min(W / (x2 - x1), H / (y2 - y1));
    // 全体が見えているときは地図は要らない
    miniBox.classList.toggle('fits', b.x1 >= vx1 && b.y1 >= vy1 && b.x2 <= vx2 && b.y2 <= vy2);
    miniOrigin = { x: x1, y: y1 };
    mini.setAttribute('viewBox', `0 0 ${W} ${H}`);
    replace(mini, ...p.nodes.map((n) => s('rect', {
      class: ['pl-mini-node', `k-${KINDS[n.type].cat}`, sel.nodes.has(n.id) && 'selected'],
      x: (n.x - x1) * miniScale, y: (n.y - y1) * miniScale, width: NODE_W * miniScale, height: nodeHeight(n) * miniScale, rx: 2,
    })), miniView);
    drawMiniView();
  }
  function drawMiniView() {
    const r = svg.getBoundingClientRect();
    const b = bounds();
    if (b) {
      const vx1 = -view.x / view.k;
      const vy1 = -view.y / view.k;
      miniBox.classList.toggle('fits', b.x1 >= vx1 && b.y1 >= vy1 && b.x2 <= vx1 + r.width / view.k && b.y2 <= vy1 + r.height / view.k);
    }
    miniView.setAttribute('x', String((-view.x / view.k - miniOrigin.x) * miniScale));
    miniView.setAttribute('y', String((-view.y / view.k - miniOrigin.y) * miniScale));
    miniView.setAttribute('width', String(((r.width || 1) / view.k) * miniScale));
    miniView.setAttribute('height', String(((r.height || 1) / view.k) * miniScale));
  }
  miniBox.addEventListener('pointerdown', (ev) => {
    ev.preventDefault();
    const go = (e2) => {
      const mr = mini.getBoundingClientRect();
      const wx = (e2.clientX - mr.left) / miniScale + miniOrigin.x;
      const wy = (e2.clientY - mr.top) / miniScale + miniOrigin.y;
      const r = svg.getBoundingClientRect();
      view.x = r.width / 2 - wx * view.k;
      view.y = r.height / 2 - wy * view.k;
      applyView();
    };
    go(ev);
    const up = () => { window.removeEventListener('pointermove', go); window.removeEventListener('pointerup', up); drawMini(); };
    window.addEventListener('pointermove', go);
    window.addEventListener('pointerup', up);
  });

  // ---------------------------------------------------------------- 何も無いとき
  const buildEmpty = () => {
    replace(emptyBox, h('div', { class: 'pl-canvas-empty-inner' },
      h('div', { class: 'es-icon' }, icon('pipelines')),
      h('h3', null, t('pipelines.canvas.emptyTitle')),
      h('p', null, t('pipelines.canvas.emptyBody')),
      h('div', { class: 'es-actions' },
        h('button', { type: 'button', class: 'btn', onClick: () => hooks.onEmptyAction('source'), 'data-testid': 'canvas-add-source' }, icon('streams'), t('pipelines.canvas.addSource')),
        h('button', { type: 'button', class: 'btn', onClick: () => hooks.onEmptyAction('templates') }, icon('sparkles'), t('pipelines.canvas.useTemplate')))));
  };

  // ---------------------------------------------------------------- 部品の操作
  function bindNode(g, n) {
    g.addEventListener('pointerdown', (ev) => {
      if (ev.target.closest('.pl-port')) return;
      if (ev.button !== 0 || spaceDown) return;
      ev.stopPropagation();
      closeMenu();
      const additive = ev.shiftKey || ev.metaKey || ev.ctrlKey;
      if (additive) {
        if (sel.nodes.has(n.id)) sel.nodes.delete(n.id); else sel.nodes.add(n.id);
      } else if (!sel.nodes.has(n.id)) {
        sel.nodes.clear();
        sel.nodes.add(n.id);
      }
      sel.edge = null;
      const ids = new Set(sel.nodes);
      const p = pl();
      const start = toWorld(ev.clientX, ev.clientY);
      const orig = new Map([...ids].map((id) => { const m = nodeById(p, id); return [id, m ? { x: m.x, y: m.y } : null]; }));
      let moved = false;
      g.setPointerCapture && g.setPointerCapture(ev.pointerId);
      const move = (e2) => {
        const q = toWorld(e2.clientX, e2.clientY);
        const dx = q.x - start.x;
        const dy = q.y - start.y;
        if (!moved && Math.hypot(dx, dy) * view.k < 4) return;
        if (!moved) { moved = true; el.classList.add('dragging'); }
        for (const [id, o] of orig) {
          const m = nodeById(p, id);
          if (!m || !o) continue;
          m.x = e2.altKey ? Math.round(o.x + dx) : snapV(o.x + dx);
          m.y = e2.altKey ? Math.round(o.y + dy) : snapV(o.y + dy);
        }
        moveDraw(ids);
        edgeScroll(e2);
      };
      const up = () => {
        g.removeEventListener('pointermove', move);
        g.removeEventListener('pointerup', up);
        g.removeEventListener('pointercancel', up);
        stopEdgeScroll();
        el.classList.remove('dragging');
        if (moved) hooks.onChange('move', { structural: false });
        else { draw(); hooks.onSelect(); }
        const again = svg.querySelector(`[data-id="${CSS.escape(n.id)}"]`);
        if (again) again.focus({ preventScroll: true });
      };
      g.addEventListener('pointermove', move);
      g.addEventListener('pointerup', up);
      g.addEventListener('pointercancel', up);
    });
    g.addEventListener('dblclick', (ev) => { if (!ev.target.closest('.pl-port')) hooks.onOpenNode(n.id); });
    g.addEventListener('contextmenu', (ev) => {
      ev.preventDefault();
      if (!sel.nodes.has(n.id)) { sel.nodes.clear(); sel.nodes.add(n.id); sel.edge = null; draw(); hooks.onSelect(); }
      const r = el.getBoundingClientRect();
      openMenu(ev.clientX - r.left, ev.clientY - r.top, hooks.nodeMenu ? hooks.nodeMenu(n.id) : []);
    });
    g.addEventListener('focus', () => {
      if (quietFocus) return;
      if (!sel.nodes.has(n.id) || sel.nodes.size !== 1) {
        sel.nodes.clear();
        sel.nodes.add(n.id);
        sel.edge = null;
        for (const [id, ge] of nodeEls) { ge.classList.toggle('selected', id === n.id); ge.setAttribute('aria-pressed', String(id === n.id)); }
        replace(overlayG);
        hooks.onSelect();
      }
      if (g.matches(':focus-visible')) reveal(n.id);
    });
    g.addEventListener('keydown', (ev) => {
      const step = ev.shiftKey ? GRID * 5 : GRID;
      const moves = { ArrowLeft: [-step, 0], ArrowRight: [step, 0], ArrowUp: [0, -step], ArrowDown: [0, step] };
      if (moves[ev.key] && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
        ev.preventDefault();
        const p = pl();
        const ids = sel.nodes.size ? sel.nodes : new Set([n.id]);
        for (const id of ids) { const m = nodeById(p, id); if (m) { m.x += moves[ev.key][0]; m.y += moves[ev.key][1]; } }
        moveDraw(ids);
        hooks.onChange('move', { structural: false, coalesce: `kbd-move:${[...ids].join(',')}` });
        reveal(n.id);
      } else if (ev.key === 'Enter' || ev.key === 'F2') {
        ev.preventDefault();
        hooks.onOpenNode(n.id);
      } else if (ev.key === 'ContextMenu' || (ev.shiftKey && ev.key === 'F10')) {
        ev.preventDefault();
        const r = el.getBoundingClientRect();
        const gr = g.getBoundingClientRect();
        openMenu(gr.left - r.left + 20, gr.top - r.top + 20, hooks.nodeMenu ? hooks.nodeMenu(n.id) : [], true);
      }
    });
  }

  // 画面の端に近いドラッグは表示を動かす
  let edgeTimer = null;
  let lastEv = null;
  const edgeScroll = (ev) => {
    lastEv = ev;
    if (edgeTimer) return;
    edgeTimer = setInterval(() => {
      if (!lastEv) return;
      const r = svg.getBoundingClientRect();
      const m = 28;
      let dx = 0;
      let dy = 0;
      if (lastEv.clientX < r.left + m) dx = 12; else if (lastEv.clientX > r.right - m) dx = -12;
      if (lastEv.clientY < r.top + m) dy = 12; else if (lastEv.clientY > r.bottom - m) dy = -12;
      if (dx || dy) { view.x += dx; view.y += dy; applyView(); }
    }, 30);
  };
  const stopEdgeScroll = () => { clearInterval(edgeTimer); edgeTimer = null; lastEv = null; };

  // ---------------------------------------------------------------- 線を引く
  const portAt = (cx, cy) => {
    const hit = document.elementFromPoint(cx, cy);
    if (!hit || !svg.contains(hit)) return null;
    const pe = hit.closest('[data-port]');
    if (pe) { const [dir, node, i] = pe.getAttribute('data-port').split(':'); return { dir, node, port: Number(i), el: pe }; }
    const ne = hit.closest('[data-id]');
    if (ne) return { dir: 'node', node: ne.getAttribute('data-id'), el: ne };
    return null;
  };

  /** 線の先を、部品に重ねたら最初につなげるポートに寄せる。 */
  const resolveTarget = (at) => {
    if (!at || !wire) return null;
    const p = pl();
    if (wire.from) {
      if (at.dir === 'in') return { node: at.node, port: at.port };
      if (at.dir === 'node') {
        const n = nodeById(p, at.node);
        if (!n) return null;
        const ports = inPorts(n).map((_, i) => ({ node: n.id, port: i }));
        return ports.find((q) => checkConnect(p, wire.from, q).ok) || ports.find((q) => checkConnect(p, wire.from, q).code === 'taken') || ports[0] || null;
      }
      return null;
    }
    if (at.dir === 'out') return { node: at.node, port: at.port };
    if (at.dir === 'node') {
      const n = nodeById(p, at.node);
      if (!n) return null;
      const ports = outPorts(n).map((_, i) => ({ node: n.id, port: i }));
      return ports.find((q) => checkConnect(p, q, wire.to).ok) || ports[0] || null;
    }
    return null;
  };

  const markPorts = () => {
    const p = pl();
    for (const pe of svg.querySelectorAll('[data-port]')) {
      const [dir, node, i] = pe.getAttribute('data-port').split(':');
      let c = null;
      if (wire.from && dir === 'in') c = checkConnect(p, wire.from, { node, port: Number(i) });
      else if (wire.to && dir === 'out') c = checkConnect(p, { node, port: Number(i) }, wire.to);
      pe.classList.toggle('can', !!c && (c.ok || c.code === 'taken'));
      pe.classList.toggle('cannot', !!c && !(c.ok || c.code === 'taken'));
    }
    svg.classList.add('wiring');
  };
  const clearPortMarks = () => {
    for (const pe of svg.querySelectorAll('[data-port]')) pe.classList.remove('can', 'cannot', 'target');
    svg.classList.remove('wiring');
  };

  const showHint = (text, ok, cx, cy) => {
    if (!text) { hint.hidden = true; return; }
    const r = el.getBoundingClientRect();
    hint.hidden = false;
    hint.textContent = text;
    hint.classList.toggle('ok', ok);
    hint.classList.toggle('no', !ok);
    hint.style.left = `${Math.min(cx - r.left + 16, r.width - 260)}px`;
    hint.style.top = `${cy - r.top + 18}px`;
  };

  const wireLine = s('path', { class: 'pl-wire' });

  const startWire = (ev, dir, nodeId, port) => {
    const p = pl();
    let detached = null;
    if (dir === 'out') {
      wire = { from: { node: nodeId, port }, to: null };
    } else {
      const e = edgeInto(p, nodeId, port);
      if (e) {
        // つながった入力を引くと、線を外して付け替える
        detached = e;
        p.edges = p.edges.filter((x) => x !== e);
        wire = { from: { ...e.from }, to: null, detached };
      } else {
        wire = { from: null, to: { node: nodeId, port } };
      }
    }
    const anchor = wire.from ? portPos(nodeById(p, wire.from.node), 'out', wire.from.port) : portPos(nodeById(p, wire.to.node), 'in', wire.to.port);
    wire.a = anchor;
    const ty = wire.from ? outType(p, wire.from) : null;
    wireLine.setAttribute('class', ['pl-wire', ty ? portLook(ty).media : 'any'].join(' '));
    if (detached) draw();
    overlayG.appendChild(wireLine);
    markPorts();
    const move = (e2) => {
      const q = toWorld(e2.clientX, e2.clientY);
      const at = portAt(e2.clientX, e2.clientY);
      const tgt = resolveTarget(at);
      for (const pe of svg.querySelectorAll('.target')) pe.classList.remove('target');
      let end = q;
      if (tgt) {
        const tn = nodeById(pl(), tgt.node);
        const c = wire.from ? checkConnect(pl(), wire.from, tgt) : checkConnect(pl(), tgt, wire.to);
        const okish = c.ok || c.code === 'taken';
        const pe = svg.querySelector(`[data-port="${wire.from ? 'in' : 'out'}:${tgt.node}:${tgt.port}"]`);
        if (pe) pe.classList.add('target');
        if (okish && tn) end = portPos(tn, wire.from ? 'in' : 'out', tgt.port);
        const portName = wire.from ? inPorts(tn)[tgt.port].name : outPorts(tn)[tgt.port].name;
        showHint(okish ? (c.code === 'taken' ? t('pipelines.canvas.hintReplace', { node: tn.name, port: portName }) : t('pipelines.canvas.hintConnect', { node: tn.name, port: portName })) : c.reason, okish, e2.clientX, e2.clientY);
      } else {
        showHint(t('pipelines.canvas.hintDrop'), true, e2.clientX, e2.clientY);
      }
      wireLine.setAttribute('d', wire.from ? edgeD(wire.a, end) : edgeD(end, wire.a));
      edgeScroll(e2);
    };
    const up = (e2) => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
      window.removeEventListener('pointercancel', up);
      stopEdgeScroll();
      clearPortMarks();
      wireLine.remove();
      showHint(null);
      const w = wire;
      const at = portAt(e2.clientX, e2.clientY);
      const tgt = e2.type === 'pointercancel' ? null : resolveTarget(at);
      wire = null;
      const pp = pl();
      if (tgt) {
        const from = w.from || tgt;
        const to = w.from ? tgt : w.to;
        if (w.detached && from.node === w.detached.from.node && from.port === w.detached.from.port && to.node === w.detached.to.node && to.port === w.detached.to.port) {
          pp.edges.push(w.detached);
          draw();
          return;
        }
        const r = connect(pp, from, to, { replace: true });
        if (!r.ok) {
          if (w.detached) pp.edges.push(w.detached);
          hooks.onReject && hooks.onReject(r.reason);
          draw();
          return;
        }
        sel.nodes.clear();
        sel.edge = r.edge.id;
        hooks.onChange('connect');
        return;
      }
      if (w.detached) { hooks.onChange('disconnect'); return; }
      // 何も無い所に落とした: つなげる部品の候補
      const q = toWorld(e2.clientX, e2.clientY);
      const r = el.getBoundingClientRect();
      const ty = w.from ? outType(pp, w.from) : null;
      const accept = w.to ? inPorts(nodeById(pp, w.to.node))[w.to.port].accept : null;
      const kinds = w.from ? kindsAccepting(ty) : kindsProducing(accept);
      if (!kinds.length) { draw(); return; }
      openMenu(e2.clientX - r.left, e2.clientY - r.top, [
        { heading: w.from ? t('pipelines.canvas.addConnected') : t('pipelines.canvas.addFeeding') },
        ...kinds.map((type) => ({
          label: KINDS[type].label, icon: KINDS[type].icon, cat: KINDS[type].cat,
          onSelect: () => hooks.onAddRequest(type, { x: snapV(w.from ? q.x : q.x - NODE_W), y: snapV(q.y - NODE_HEAD) }, w.from ? { from: w.from } : { to: w.to }),
        })),
      ]);
      draw();
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
    window.addEventListener('pointercancel', up);
    move(ev);
  };

  svg.addEventListener('pointerdown', (ev) => {
    const pe = ev.target.closest && ev.target.closest('[data-port]');
    if (!pe || ev.button !== 0) return;
    ev.stopPropagation();
    ev.preventDefault();
    closeMenu();
    const [dir, node, i] = pe.getAttribute('data-port').split(':');
    startWire(ev, dir, node, Number(i));
  }, true);

  // ---------------------------------------------------------------- 背景: 動かす・囲んで選ぶ・つまむ
  const pointers = new Map();
  let pinch = null;
  svg.addEventListener('pointerdown', (ev) => {
    if (ev.target.closest('.pl-node, [data-port], .pl-edge, .pl-edge-hit, .pl-edge-del')) return;
    closeMenu();
    pointers.set(ev.pointerId, { x: ev.clientX, y: ev.clientY });
    svg.setPointerCapture(ev.pointerId);
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      pinch = { d: Math.hypot(a.x - b.x, a.y - b.y), k: view.k, mx: (a.x + b.x) / 2, my: (a.y + b.y) / 2, vx: view.x, vy: view.y };
      marquee.hidden = true;
      return;
    }
    const pan = ev.pointerType === 'touch' || ev.button === 1 || spaceDown;
    const start = { x: ev.clientX, y: ev.clientY, vx: view.x, vy: view.y };
    const additive = ev.shiftKey || ev.metaKey || ev.ctrlKey;
    const before = new Set(sel.nodes);
    let moved = false;
    const r0 = el.getBoundingClientRect();
    const move = (e2) => {
      if (!pointers.has(e2.pointerId)) return;
      pointers.set(e2.pointerId, { x: e2.clientX, y: e2.clientY });
      if (pinch && pointers.size === 2) {
        const [a, b] = [...pointers.values()];
        const d = Math.hypot(a.x - b.x, a.y - b.y);
        const mx = (a.x + b.x) / 2;
        const my = (a.y + b.y) / 2;
        view.x = pinch.vx + (mx - pinch.mx);
        view.y = pinch.vy + (my - pinch.my);
        applyView();
        zoomAt(pinch.k * (d / pinch.d), mx, my);
        return;
      }
      const dx = e2.clientX - start.x;
      const dy = e2.clientY - start.y;
      if (!moved && Math.hypot(dx, dy) < 4) return;
      moved = true;
      if (pan) {
        el.classList.add('panning');
        view.x = start.vx + dx;
        view.y = start.vy + dy;
        applyView();
        return;
      }
      const x1 = Math.min(start.x, e2.clientX);
      const y1 = Math.min(start.y, e2.clientY);
      const x2 = Math.max(start.x, e2.clientX);
      const y2 = Math.max(start.y, e2.clientY);
      marquee.hidden = false;
      Object.assign(marquee.style, { left: `${x1 - r0.left}px`, top: `${y1 - r0.top}px`, width: `${x2 - x1}px`, height: `${y2 - y1}px` });
      const a = toWorld(x1, y1);
      const b = toWorld(x2, y2);
      sel.nodes = new Set(additive ? before : []);
      for (const n of pl().nodes) {
        if (n.x < b.x && n.x + NODE_W > a.x && n.y < b.y && n.y + nodeHeight(n) > a.y) sel.nodes.add(n.id);
      }
      for (const [id, g] of nodeEls) g.classList.toggle('selected', sel.nodes.has(id));
    };
    const up = (e2) => {
      pointers.delete(e2.pointerId);
      if (pointers.size < 2) pinch = null;
      if (pointers.size) return;
      svg.removeEventListener('pointermove', move);
      svg.removeEventListener('pointerup', up);
      svg.removeEventListener('pointercancel', up);
      el.classList.remove('panning');
      marquee.hidden = true;
      if (!moved && !pan) {
        sel.nodes.clear();
        sel.edge = null;
      }
      sel.edge = moved && !pan ? null : sel.edge;
      draw();
      hooks.onSelect();
    };
    svg.addEventListener('pointermove', move);
    svg.addEventListener('pointerup', up);
    svg.addEventListener('pointercancel', up);
  });

  svg.addEventListener('wheel', (ev) => {
    ev.preventDefault();
    closeMenu();
    if (ev.ctrlKey || ev.metaKey) {
      zoomAt(view.k * Math.exp(-ev.deltaY * (ev.deltaMode === 1 ? 0.05 : 0.0025)), ev.clientX, ev.clientY);
    } else {
      const f = ev.deltaMode === 1 ? 16 : 1;
      view.x -= (ev.shiftKey && !ev.deltaX ? ev.deltaY : ev.deltaX) * f;
      view.y -= (ev.shiftKey && !ev.deltaX ? 0 : ev.deltaY) * f;
      applyView();
    }
  }, { passive: false });

  const onKeyDown = (ev) => { if (ev.key === ' ' && !ev.repeat && el.contains(document.activeElement) && document.activeElement.tagName !== 'BUTTON') { spaceDown = true; el.classList.add('space'); } };
  const onKeyUp = (ev) => { if (ev.key === ' ') { spaceDown = false; el.classList.remove('space'); } };
  window.addEventListener('keydown', onKeyDown);
  window.addEventListener('keyup', onKeyUp);

  // 左の一覧からドラッグして置く
  el.addEventListener('dragover', (ev) => { if (ev.dataTransfer && [...ev.dataTransfer.types].includes('application/x-senca-kind')) { ev.preventDefault(); ev.dataTransfer.dropEffect = 'copy'; } });
  el.addEventListener('drop', (ev) => {
    const type = ev.dataTransfer && ev.dataTransfer.getData('application/x-senca-kind');
    if (!type || !KINDS[type]) return;
    ev.preventDefault();
    const q = toWorld(ev.clientX, ev.clientY);
    hooks.onAddRequest(type, { x: snapV(q.x - NODE_W / 2), y: snapV(q.y - NODE_HEAD / 2) });
  });

  // ---------------------------------------------------------------- その場のメニュー（右クリック・線を落としたとき）
  let menu = null;
  function closeMenu() {
    if (!menu) return;
    menu.remove();
    menu = null;
  }
  function openMenu(x, y, items, focusFirst = true) {
    closeMenu();
    const entries = [];
    const box = h('div', { class: 'menu pl-ctx', role: 'menu', 'aria-label': t('pipelines.canvas.menu') });
    for (const it of items) {
      if (!it) continue;
      if (it.separator) { box.appendChild(h('div', { class: 'menu-sep', role: 'separator' })); continue; }
      if (it.heading) { box.appendChild(h('div', { class: 'menu-label', role: 'presentation' }, it.heading)); continue; }
      const b = h('button', { type: 'button', role: 'menuitem', tabindex: '-1', class: [it.danger && 'danger', it.cat && `k-${it.cat}`], onClick: () => { closeMenu(); it.onSelect(); } },
        it.icon ? icon(it.icon) : null, h('span', null, it.label), it.kbd ? h('kbd', null, it.kbd) : null);
      entries.push(b);
      box.appendChild(b);
    }
    box.addEventListener('keydown', (e) => {
      const i = entries.indexOf(document.activeElement);
      if (e.key === 'ArrowDown') { e.preventDefault(); entries[(i + 1) % entries.length].focus(); }
      else if (e.key === 'ArrowUp') { e.preventDefault(); entries[(i - 1 + entries.length) % entries.length].focus(); }
      else if (e.key === 'Escape' || e.key === 'Tab') { e.preventDefault(); e.stopPropagation(); closeMenu(); svg.focus(); }
    });
    menuHost.appendChild(box);
    const r = el.getBoundingClientRect();
    box.style.left = `${Math.max(8, Math.min(x, r.width - box.offsetWidth - 8))}px`;
    box.style.top = `${Math.max(8, Math.min(y, r.height - box.offsetHeight - 8))}px`;
    menu = box;
    if (focusFirst && entries[0]) entries[0].focus({ preventScroll: true });
    const away = (e) => { if (menu && !menu.contains(e.target)) { closeMenu(); document.removeEventListener('pointerdown', away, true); } };
    setTimeout(() => document.addEventListener('pointerdown', away, true), 0);
  }
  svg.addEventListener('contextmenu', (ev) => {
    if (ev.target.closest('.pl-node')) return;
    ev.preventDefault();
    const r = el.getBoundingClientRect();
    if (hooks.canvasMenu) openMenu(ev.clientX - r.left, ev.clientY - r.top, hooks.canvasMenu(toWorld(ev.clientX, ev.clientY)));
  });

  const ro = typeof ResizeObserver === 'function' ? new ResizeObserver(() => drawMini()) : null;
  if (ro) ro.observe(el);

  applyView();

  return {
    el,
    sel,
    draw,
    fit,
    reveal,
    zoomBy,
    zoomTo,
    freeSpot,
    openMenu,
    closeMenu,
    toWorld,
    get view() { return { ...view }; },
    setView(v) { Object.assign(view, v); applyView(); },
    setRunning(v) { running = !!v; svg.classList.toggle('is-running', running); },
    focusNode(id) {
      const g = svg.querySelector(`[data-id="${CSS.escape(id)}"]`);
      if (g) g.focus({ preventScroll: true });
      reveal(id);
    },
    select(ids, { edge = null } = {}) {
      sel.nodes = new Set(ids);
      sel.edge = edge;
      draw();
    },
    destroy() {
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('keyup', onKeyUp);
      if (ro) ro.disconnect();
      stopEdgeScroll();
    },
  };
}
