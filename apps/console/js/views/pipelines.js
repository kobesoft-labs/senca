// パイプラインの編集（REQ-005）: SVG 上で部品を置いて線でつなぐ。右の検査器で設定する。
//
// グラフは senca-pipeline の JSON（js/pipeline/model.js が行き来する）。ノードの API（js/api/pipelines.js）で
// 保存（POST・PATCH ＋ If-Match）・検証（POST /pipelines/{id}/validate、計画と問題の一覧）・開始/停止
// （PATCH desired_state）・削除をする。保存前の下書きと部品の位置はこのブラウザ（localStorage）に置く
// （グラフは知らない項目を拒否するので、位置はグラフに入れられない）。
//
// 操作: 左の部品を押すと置く。部品はドラッグで動かす。右の丸（出力）から左の丸（入力）へドラッグでつなぐ。
// キーボード: Tab で部品を選び、矢印で動かす（Shift で大きく）、Delete で消す。つなぐのは検査器からもできる。

import { h, s, replace, select, debounce, table, badge, kv } from '../lib/dom.js';
import { modal, confirmDialog, toast, reportError } from '../lib/ui.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { button, iconButton, pageHeader, panel, alert, formField, helpHint } from '../ui/index.js';
import { pipelines as api, NotAvailableError, issuesOf } from '../api/pipelines.js';
import { streams, listAll } from '../api/resources.js';
import { fmtTime } from '../lib/format.js';
import {
  KINDS, PORT_LABEL, OPTION_FIELDS, NODE_W, transitions, nodeHeight, inPorts, outPorts, outType, compatible, emptyPipeline, addNode, removeNode,
  removeNamedInput, connect, validate, toGraph, fromGraph, positionsOf, samplePipeline, referencedStreams, isNodeName,
} from '../pipeline/model.js';

const DRAFTS = 'senca.console.pipelines.v2';
const LAYOUT = 'senca.console.pipeline-layout.v1';
const SELECTED = 'senca.console.pipelines.selected';

const readJson = (k, d) => {
  try { const v = JSON.parse(localStorage.getItem(k) || 'null'); return v ?? d; } catch { return d; }
};
const writeJson = (k, v) => {
  try { localStorage.setItem(k, JSON.stringify(v)); } catch { /* 容量切れなど */ }
};

function portPos(n, dir, i) {
  const k = dir === 'in' ? inPorts(n).length : outPorts(n).length;
  const hh = nodeHeight(n);
  const step = hh / (k + 1);
  return { x: n.x + (dir === 'out' ? NODE_W : 0), y: n.y + step * (i + 1) };
}

function edgePath(a, b) {
  const dx = Math.max(40, Math.abs(b.x - a.x) / 2);
  return `M${a.x},${a.y} C${a.x + dx},${a.y} ${b.x - dx},${b.y} ${b.x},${b.y}`;
}

/** パイプラインの観測した状態。data-state に生の値（running など）を入れる。 */
export const observedBadge = (st) => {
  const kind = { running: 'ok', starting: 'info', stopping: 'warn', error: 'danger', failed: 'danger', stopped: 'outline' }[st] ?? '';
  const known = ['running', 'starting', 'stopping', 'stopped', 'error', 'failed'].includes(st);
  const b = h('span', { class: ['badge', 'has-dot', kind].filter(Boolean).join(' '), 'data-state': st || '', title: st || '' }, known ? t(`status.${st}`) : (st || '—'));
  return b;
};

/** 計画（senca-pipeline の Plan）の表。 */
export function planView(plan) {
  if (!plan) return h('p', { class: 'muted' }, t('pipelines.plan.none'));
  const tot = plan.totals || {};
  return h('div', null,
    kv([
      [t('pipelines.plan.order'), (plan.order || []).join(' → ')],
      [t('pipelines.plan.totals'), t('pipelines.plan.totalsValue', { px: Math.round((tot.pixels_per_sec || 0) / 1e6), cpu: tot.cpu_codec_sessions ?? 0, gpu: tot.gpu_codec_sessions ?? 0, threads: tot.threads ?? 0, ms: Math.round(tot.max_latency_ms || 0) })],
    ]),
    table([
      { label: t('pipelines.plan.node'), render: (n) => h('code', null, n.id) },
      { label: t('pipelines.plan.type'), render: (n) => n.type },
      { label: t('pipelines.plan.mode'), render: (n) => badge(n.mode, { passthrough: 'ok', transcode: 'warn', process: '', terminal: '' }[n.mode] || '') },
      { label: t('pipelines.plan.codec'), render: (n) => (n.codec ? [h('code', null, n.codec), n.hardware === true ? ' (HW)' : n.hardware === false ? ' (SW)' : '', n.provider ? ` ${n.provider}` : ''] : '—') },
      { label: t('pipelines.plan.outputs'), render: (n) => (n.outputs || []).join(', ') || '—' },
      { label: t('pipelines.plan.latency'), class: 'num', render: (n) => `${Math.round(n.added_latency_ms || 0)} ms` },
      { label: t('pipelines.plan.notes'), render: (n) => (n.notes || []).join(' / ') },
    ], plan.nodes || [], { caption: t('pipelines.plan.caption') }),
    h('h3', null, t('pipelines.plan.sinks')),
    table([
      { label: 'sink', render: (x) => h('code', null, x.sink) },
      { label: 'stream', render: (x) => h('a', { href: `#/streams/${encodeURIComponent(x.stream)}` }, h('code', null, x.stream)) },
      { label: t('pipelines.plan.track'), render: (x) => `${x.track} (${x.kind})` },
      { label: t('pipelines.plan.codec'), render: (x) => h('code', null, x.codec) },
      { label: t('pipelines.plan.mode'), render: (x) => x.mode },
      { label: t('pipelines.plan.from'), render: (x) => x.from },
      { label: t('pipelines.plan.latency'), class: 'num', render: (x) => `${Math.round(x.latency_ms || 0)} ms` },
    ], (plan.sinks || []).flatMap((sk) => sk.tracks.map((tk) => ({ sink: sk.id, stream: sk.stream, ...tk }))), { empty: t('pipelines.plan.noSinks') }));
}

export async function render(root) {
  let drafts = readJson(DRAFTS, []);
  const layout = readJson(LAYOUT, {});
  /** entries: [{pl, remote}]（remote は GET の結果、下書きは null） */
  let entries = [];
  let cur = null;
  let sel = null; // {type:'node'|'edge', id}
  let localIssues = [];
  let remoteIssues = [];
  let plan = null;
  let available = null;
  let streamList = [];
  let focusNode = null;
  let pollTimer = null;
  let dead = false;

  const banner = h('div');
  const listSel = h('select', { id: 'pl-pick', 'aria-label': t('pipelines.pick') });
  const svg = s('svg', { role: 'application', 'aria-label': t('pipelines.graph'), viewBox: '0 0 1400 700', width: 1400, height: 700 });
  const inspector = h('section', { class: 'panel pl-inspector', 'aria-label': t('pipelines.inspector') });
  const issuesBox = h('div');
  const runtimeBox = h('div');
  const planBox = h('div');
  const toolbarState = h('span', { class: 'row' });
  const pl = () => cur.pl;

  const rf = () => referencedStreams(pl()).some((id) => (streamList.find((x) => x.id === id) || {}).codec_profile === 'royalty_free');

  // ---------------------------------------------------------------- 保存（下書き・位置）
  const persistNow = () => {
    if (!cur) return;
    const p = pl();
    if (p.remoteId) {
      layout[p.remoteId] = positionsOf(p);
      writeJson(LAYOUT, layout);
    } else {
      const d = { key: p.key, name: p.name, graph: toGraph(p), positions: positionsOf(p), updated_at: new Date().toISOString() };
      drafts = [d, ...drafts.filter((x) => x.key !== p.key)];
      writeJson(DRAFTS, drafts);
    }
  };
  const persist = debounce(persistNow, 300);

  const changed = ({ structural = true } = {}) => {
    if (structural) pl().dirty = true;
    localIssues = validate(pl(), { royaltyFree: rf() });
    persist();
    draw();
    renderIssues();
    renderToolbar();
  };

  const label = (e) => `${e.pl.name}${e.pl.remoteId ? '' : t('pipelines.draftSuffix')}${e.pl.dirty && e.pl.remoteId ? ' *' : ''}`;
  const renderList = () => {
    replace(listSel, entries.map((e) => h('option', { value: e.pl.key, selected: e === cur }, label(e))));
  };

  const renderBanner = () => {
    replace(banner, available === false
      ? alert('warn', t('pipelines.offlineBody'), { title: t('pipelines.offlineTitle'), role: 'status' })
      : null);
  };

  const offline = (e) => {
    if (e instanceof NotAvailableError) {
      available = false;
      renderBanner();
      toast(t('pipelines.offlineToast'), 'warn', 7000);
      return true;
    }
    return false;
  };

  // ---------------------------------------------------------------- 描画
  const toSvg = (ev) => {
    const pt = svg.createSVGPoint();
    pt.x = ev.clientX;
    pt.y = ev.clientY;
    const m = svg.getScreenCTM();
    return m ? pt.matrixTransform(m.inverse()) : { x: ev.clientX, y: ev.clientY };
  };

  let wire = null;
  const nodeByName = (name) => pl().nodes.find((n) => n.name === name);

  const draw = () => {
    const p = pl();
    const W = Math.max(1400, ...p.nodes.map((n) => n.x + NODE_W + 40));
    const H = Math.max(700, ...p.nodes.map((n) => n.y + nodeHeight(n) + 40));
    svg.setAttribute('viewBox', `0 0 ${W} ${H}`);
    svg.setAttribute('width', W);
    svg.setAttribute('height', H);
    const bad = new Set([...localIssues.filter((i) => i.level === 'error' && i.node).map((i) => i.node),
      ...remoteIssues.map((i) => i.node && nodeByName(i.node)).filter(Boolean).map((n) => n.id)]);
    const badEdges = new Set(localIssues.filter((i) => i.edge).map((i) => i.edge));
    const grid = s('g', { class: 'pl-grid', 'aria-hidden': 'true' });
    for (let x = 0; x <= W; x += 40) grid.appendChild(s('line', { x1: x, y1: 0, x2: x, y2: H }));
    for (let y = 0; y <= H; y += 40) grid.appendChild(s('line', { x1: 0, y1: y, x2: W, y2: y }));
    const bg = s('rect', { x: 0, y: 0, width: W, height: H, fill: 'transparent' });
    bg.addEventListener('pointerdown', () => { sel = null; draw(); renderInspector(); });
    const edges = s('g');
    for (const e of p.edges) {
      const a = p.nodes.find((n) => n.id === e.from.node);
      const b = p.nodes.find((n) => n.id === e.to.node);
      if (!a || !b) continue;
      const d = edgePath(portPos(a, 'out', e.from.port), portPos(b, 'in', e.to.port));
      const isSel = sel && sel.type === 'edge' && sel.id === e.id;
      const hit = s('path', { d, class: 'pl-edge-hit' });
      const path = s('path', { d, class: ['pl-edge', isSel && 'selected', badEdges.has(e.id) && 'invalid'] },
        s('title', null, `${a.name}.${outPorts(a)[e.from.port].name} → ${b.name}（${PORT_LABEL[outType(p, e.from)] || '?'}）`));
      const pick = (ev) => { ev.stopPropagation(); sel = { type: 'edge', id: e.id }; draw(); renderInspector(); };
      hit.addEventListener('pointerdown', pick);
      path.addEventListener('pointerdown', pick);
      edges.append(hit, path);
    }
    const nodes = s('g');
    for (const n of p.nodes) {
      const k = KINDS[n.type];
      const nh = nodeHeight(n);
      const isSel = sel && sel.type === 'node' && sel.id === n.id;
      const sub = n.type === 'source' || n.type === 'sink'
        ? ((streamList.find((x) => x.id === n.p.stream) || {}).name || n.p.stream || t('pipelines.noStream'))
        : n.type === 'encode' ? `${n.p.codec || '?'} ${n.p.bitrate_kbps || ''}k` : k.label;
      const g = s('g', {
        class: ['pl-node', isSel && 'selected', bad.has(n.id) && 'invalid'], transform: `translate(${n.x},${n.y})`, tabindex: '0',
        role: 'button', 'aria-label': t('pipelines.nodeAria', { kind: k.label, node: n.name }), 'data-node': n.name,
      },
      s('rect', { class: 'body', width: NODE_W, height: nh, rx: 8 }),
      s('rect', { class: 'stripe', width: 6, height: nh, rx: 3, fill: k.color }),
      s('text', { x: 14, y: 22 }, n.name.length > 20 ? `${n.name.slice(0, 19)}…` : n.name),
      s('text', { x: 14, y: 40, class: 'kind' }, `${n.type}`.slice(0, 24)),
      s('text', { x: 14, y: 54, class: 'kind' }, String(sub).slice(0, 24)));
      g.addEventListener('pointerdown', (ev) => {
        if (ev.target.classList.contains('pl-port')) return;
        ev.stopPropagation();
        ev.preventDefault();
        sel = { type: 'node', id: n.id };
        const start = toSvg(ev);
        const ox = n.x;
        const oy = n.y;
        let moved = false;
        const move = (e2) => {
          const q = toSvg(e2);
          const nx = Math.max(0, Math.round((ox + q.x - start.x) / 10) * 10);
          const ny = Math.max(0, Math.round((oy + q.y - start.y) / 10) * 10);
          if (nx !== n.x || ny !== n.y) { n.x = nx; n.y = ny; moved = true; draw(); }
        };
        const up = () => {
          window.removeEventListener('pointermove', move);
          window.removeEventListener('pointerup', up);
          if (moved) persist();
          renderInspector();
          focusNode = n.name;
          draw();
        };
        window.addEventListener('pointermove', move);
        window.addEventListener('pointerup', up);
        draw();
      });
      g.addEventListener('keydown', (ev) => {
        const stepPx = ev.shiftKey ? 50 : 10;
        const moves = { ArrowLeft: [-stepPx, 0], ArrowRight: [stepPx, 0], ArrowUp: [0, -stepPx], ArrowDown: [0, stepPx] };
        if (moves[ev.key]) {
          ev.preventDefault();
          n.x = Math.max(0, n.x + moves[ev.key][0]);
          n.y = Math.max(0, n.y + moves[ev.key][1]);
          sel = { type: 'node', id: n.id };
          focusNode = n.name;
          persist();
          draw();
        } else if (ev.key === 'Delete' || ev.key === 'Backspace') {
          ev.preventDefault();
          removeNode(p, n.id);
          sel = null;
          changed();
          renderInspector();
          svg.focus();
        } else if (ev.key === 'Enter' || ev.key === ' ') {
          ev.preventDefault();
          sel = { type: 'node', id: n.id };
          focusNode = n.name;
          draw();
          renderInspector();
        }
      });
      g.addEventListener('focus', () => {
        if (!sel || sel.id !== n.id) { sel = { type: 'node', id: n.id }; focusNode = n.name; renderInspector(); draw(); }
      });
      inPorts(n).forEach((port, i) => {
        const q = portPos({ ...n, x: 0, y: 0 }, 'in', i);
        const c = s('circle', { class: ['pl-port', wire && compatible(wire.type, port.accept) && 'hot'], cx: q.x, cy: q.y, r: 7, 'data-port-in': `${n.id}:${i}` },
          s('title', null, t('pipelines.portIn', { port: port.name })));
        g.appendChild(c);
      });
      outPorts(n).forEach((o, i) => {
        const q = portPos({ ...n, x: 0, y: 0 }, 'out', i);
        const ty = outType(p, { node: n.id, port: i });
        const c = s('circle', { class: 'pl-port', cx: q.x, cy: q.y, r: 7 }, s('title', null, t('pipelines.portOut', { port: o.name, type: PORT_LABEL[ty] || ty })));
        c.addEventListener('pointerdown', (ev) => {
          ev.stopPropagation();
          ev.preventDefault();
          const from = portPos(n, 'out', i);
          wire = { from: { node: n.id, port: i }, type: ty, a: from, b: from };
          const move = (e2) => { wire.b = toSvg(e2); draw(); };
          const up = (e2) => {
            window.removeEventListener('pointermove', move);
            window.removeEventListener('pointerup', up);
            const target = document.elementFromPoint(e2.clientX, e2.clientY);
            const port = target && target.closest && target.closest('[data-port-in]');
            const w = wire;
            wire = null;
            if (port) {
              const [nid, pi] = port.getAttribute('data-port-in').split(':');
              const r = connect(p, w.from, { node: nid, port: Number(pi) });
              if (!r.ok) toast(r.reason, 'warn');
              else sel = { type: 'edge', id: r.edge.id };
              changed();
              renderInspector();
            } else draw();
          };
          window.addEventListener('pointermove', move);
          window.addEventListener('pointerup', up);
        });
        g.appendChild(c);
      });
      nodes.appendChild(g);
    }
    const children = [grid, bg, edges, nodes];
    if (wire) children.push(s('path', { class: 'pl-wire', d: edgePath(wire.a, wire.b) }));
    replace(svg, ...children);
    if (focusNode) {
      const el = svg.querySelector(`[data-node="${CSS.escape(focusNode)}"]`);
      if (el && document.activeElement !== el) el.focus({ preventScroll: true });
      focusNode = null;
    }
  };

  // ---------------------------------------------------------------- 検査器
  const fieldInput = (f, getV, setV, n) => {
    const v = getV();
    let input;
    if (f.type === 'select') input = select(f.options.map((o) => (Array.isArray(o) ? o : [o, o === '' ? t('pipelines.unused') : o])), v ?? f.def ?? '');
    else if (f.type === 'stream') input = select([['', t('pipelines.choose')], ...streamList.map((x) => [x.id, `${x.name} (${x.codec_profile})`]), ...(v && !streamList.some((x) => x.id === v) ? [[v, v]] : [])], v || '');
    else if (f.type === 'scene') input = select([['', t('pipelines.none')], ...Object.keys((n && n.p.scenes) || {}).map((x) => [x, x])], v || '');
    else if (f.type === 'check') {
      input = h('input', { type: 'checkbox', checked: v ?? f.def ?? false });
      input.addEventListener('change', () => setV(input.checked));
      return h('label', { class: 'check' }, input, f.label);
    } else if (f.type === 'color') input = h('input', { type: 'color', value: v || f.def || '#000000' });
    else input = h('input', { type: f.type === 'number' ? 'number' : 'text', value: v ?? '', min: f.min, max: f.max, step: f.step });
    input.addEventListener('change', () => {
      const x = input.value;
      setV(f.type === 'number' ? (x === '' ? null : Number(x)) : x);
    });
    return formField({ label: f.label, input, required: !!f.required });
  };

  const namedInputsEditor = (n) => {
    const isMix = n.type === 'audio_mix';
    const rows = n.ins.map((inp, i) => {
      const name = h('input', { type: 'text', value: inp.name, 'aria-label': t('pipelines.inputNameAria', { n: i + 1 }), style: { width: '110px' } });
      name.addEventListener('change', () => {
        const old = inp.name;
        if (!isNodeName(name.value)) { toast(t('pipelines.inputNameInvalid'), 'warn'); name.value = old; return; }
        inp.name = name.value;
        if (n.type === 'compose') {
          for (const sc of Object.values(n.p.scenes || {})) for (const l of sc.layers || []) if (l.source === old) l.source = inp.name;
        }
        changed();
        renderInspector();
      });
      const up = pl().edges.find((e) => e.to.node === n.id && e.to.port === i);
      const upName = up ? (() => { const m = pl().nodes.find((x) => x.id === up.from.node); return m ? `${m.name}.${outPorts(m)[up.from.port].name}` : '?'; })() : t('pipelines.unconnected');
      const extras = [];
      if (isMix) {
        const numIn = (key, labelText, step = 0.5) => {
          const x = h('input', { type: 'number', step, value: inp[key] ?? 0, style: { width: '64px' }, 'aria-label': t('pipelines.mixFieldAria', { input: inp.name, field: labelText }) });
          x.addEventListener('change', () => { inp[key] = Number(x.value) || 0; changed(); });
          return h('label', { class: 'small' }, labelText, x);
        };
        const ck = (key, labelText) => {
          const x = h('input', { type: 'checkbox', checked: !!inp[key] });
          x.addEventListener('change', () => { inp[key] = x.checked; changed(); });
          return h('label', { class: 'check small' }, x, labelText);
        };
        extras.push(numIn('gain_db', 'dB'), numIn('pan', 'pan', 0.1), numIn('delay_ms', t('pipelines.delayMs'), 1), ck('mute', t('production.mute')), ck('solo', t('production.solo')));
      }
      return h('div', { class: 'inline', style: { flexWrap: 'wrap', gap: '4px', borderBottom: '1px solid var(--border)', paddingBottom: '4px' } },
        name, h('span', { class: 'small muted' }, `← ${upName}`), ...extras,
        h('button', { type: 'button', class: 'small ghost icon-only', 'aria-label': t('pipelines.removeInput', { input: inp.name }), onClick: () => { removeNamedInput(pl(), n, i); changed(); renderInspector(); } }, icon('close')));
    });
    const add = h('button', { type: 'button', class: 'small', onClick: () => {
      let k = n.ins.length + 1;
      while (n.ins.some((x) => x.name === `in${k}`)) k += 1;
      n.ins.push(isMix ? { name: `in${k}`, gain_db: 0, mute: false, solo: false, pan: 0, delay_ms: 0 } : { name: `in${k}` });
      changed();
      renderInspector();
    } }, icon('plus'), t('pipelines.addInput'));
    return h('fieldset', null, h('legend', null, isMix ? t('pipelines.mixInputs') : t('pipelines.composeInputs')), ...rows, add);
  };

  const renderInspector = () => {
    const p = pl();
    if (sel && sel.type === 'node') {
      const n = p.nodes.find((m) => m.id === sel.id);
      if (!n) { sel = null; renderInspector(); return; }
      const k = KINDS[n.type];
      const name = h('input', { type: 'text', value: n.name, maxlength: 64, 'aria-label': t('pipelines.nodeNameAria') });
      name.addEventListener('change', () => {
        if (!isNodeName(name.value) || p.nodes.some((m) => m !== n && m.name === name.value)) { toast(t('pipelines.nodeNameInvalid'), 'warn'); name.value = n.name; return; }
        n.name = name.value;
        changed();
      });
      const fields = k.fields.filter((f) => !f.when || f.when(n.p)).map((f) => fieldInput(f, () => n.p[f.path], (v) => {
        n.p[f.path] = v;
        changed();
        if (f.path.endsWith('.codec') || f.type === 'stream') renderInspector();
      }, n));
      const targets = [];
      outPorts(n).forEach((o, oi) => {
        const ty = outType(p, { node: n.id, port: oi });
        for (const m of p.nodes) {
          if (m.id === n.id) continue;
          inPorts(m).forEach((port, ii) => {
            if (compatible(ty, port.accept) && !p.edges.some((e) => e.to.node === m.id && e.to.port === ii)) {
              targets.push([`${oi}|${m.id}|${ii}`, `${o.name} → ${m.name} (${port.name})`]);
            }
          });
        }
      });
      const target = select(targets.length ? targets : [['', t('pipelines.noTargets')]], '');
      target.id = `tgt-${n.id}`;
      const linkBtn = h('button', { class: 'small', disabled: !targets.length, onClick: () => {
        const [oi, mid, ii] = target.value.split('|');
        const r = connect(p, { node: n.id, port: Number(oi) }, { node: mid, port: Number(ii) });
        if (!r.ok) toast(r.reason, 'warn');
        changed();
        renderInspector();
      } }, icon('link'), t('pipelines.connect'));
      const extraKeys = Object.keys(n.extra || {});
      replace(inspector,
        h('h2', null, h('span', { class: 'dot', style: { background: k.color } }), k.label),
        h('div', { class: 'form' },
          formField({ label: t('pipelines.nodeName'), input: name }),
          ...fields,
          k.named ? namedInputsEditor(n) : null,
          n.type === 'compose' ? h('div', { class: 'inline' }, h('button', { type: 'button', onClick: () => editScenes(n) }, icon('edit'), t('pipelines.editScenes')),
            h('span', { class: 'small muted' }, t('pipelines.sceneCount', { n: Object.keys(n.p.scenes || {}).length }))) : null,
          extraKeys.length ? h('div', { class: 'small muted' }, t('pipelines.extraKeys', { keys: extraKeys.join(', ') })) : null,
          outPorts(n).length ? h('div', { class: 'field' }, h('label', { for: target.id }, t('pipelines.connectKeyboard')), h('div', { class: 'inline' }, target, linkBtn)) : null,
          h('div', { class: 'small muted' }, t('pipelines.ports', { ins: inPorts(n).map((x) => x.name).join(', ') || t('pipelines.portsNone'), outs: outPorts(n).map((o, i) => `${o.name} (${PORT_LABEL[outType(p, { node: n.id, port: i })] || '?'})`).join(', ') || t('pipelines.portsNone') })),
          h('div', { class: 'actions' }, h('button', { class: 'danger', onClick: () => { removeNode(p, n.id); sel = null; changed(); renderInspector(); } }, icon('trash'), t('pipelines.deleteNode')))));
      return;
    }
    if (sel && sel.type === 'edge') {
      const e = p.edges.find((x) => x.id === sel.id);
      if (!e) { sel = null; renderInspector(); return; }
      const a = p.nodes.find((n) => n.id === e.from.node);
      const b = p.nodes.find((n) => n.id === e.to.node);
      replace(inspector, h('h2', null, icon('link'), t('pipelines.edge')),
        h('p', null, `${a.name}.${outPorts(a)[e.from.port].name} → ${b.name} (${inPorts(b)[e.to.port].name})`),
        h('p', { class: 'muted small' }, t('pipelines.edgeType', { type: PORT_LABEL[outType(p, e.from)] || '?' })),
        h('button', { class: 'danger', onClick: () => { p.edges = p.edges.filter((x) => x.id !== e.id); sel = null; changed(); renderInspector(); } }, icon('trash'), t('pipelines.deleteEdge')));
      return;
    }
    const name = h('input', { type: 'text', value: p.name, maxlength: 64, 'aria-label': t('pipelines.nameAria') });
    name.addEventListener('change', () => { p.name = name.value; changed(); renderList(); });
    const opts = OPTION_FIELDS.map((f) => fieldInput(f, () => p.options[f.path], (v) => { p.options[f.path] = v; changed(); }));
    replace(inspector, h('h2', null, icon('pipelines'), t('nav.pipelines')),
      h('div', { class: 'form' },
        formField({ label: t('pipelines.name'), input: name, hint: t('pipelines.nameHint') }),
        h('details', null, h('summary', null, t('pipelines.options')), h('div', { class: 'form' }, ...opts)),
        h('div', { class: 'small muted' }, t('pipelines.counts', { nodes: p.nodes.length, edges: p.edges.length })),
        p.remoteId ? h('div', { class: 'small' }, 'ID: ', h('code', null, p.remoteId)) : h('div', { class: 'small muted' }, t('pipelines.notSaved')),
        h('p', { class: 'small muted' }, t('pipelines.selectHint'))));
  };

  // ---------------------------------------------------------------- シーンの編集（合成）
  const editScenes = async (n) => {
    const scenes = JSON.parse(JSON.stringify(n.p.scenes || {}));
    const inputNames = n.ins.map((i) => i.name);
    const box = h('div', { class: 'form' });
    const cw = Number(n.p['canvas.width']) || 1280;
    const ch = Number(n.p['canvas.height']) || 720;
    const draw2 = () => {
      replace(box, Object.entries(scenes).map(([sn, sc]) => {
        const sname = h('input', { type: 'text', value: sn, 'aria-label': t('pipelines.sceneName'), style: { width: '160px' } });
        sname.addEventListener('change', () => {
          if (!sname.value || scenes[sname.value]) { sname.value = sn; return; }
          const ordered = Object.fromEntries(Object.entries(scenes).map(([k, v]) => (k === sn ? [sname.value, v] : [k, v])));
          for (const k of Object.keys(scenes)) delete scenes[k];
          Object.assign(scenes, ordered);
          draw2();
        });
        const layers = (sc.layers || []).map((l, li) => {
          const content = select([...inputNames.map((x) => [`source:${x}`, t('pipelines.layerInput', { name: x })]), ['color', t('pipelines.layerColor')], ...(l.image ? [[`image:${l.image}`, t('pipelines.layerImage', { name: l.image })]] : [])],
            l.source ? `source:${l.source}` : l.image ? `image:${l.image}` : 'color', { 'aria-label': t('pipelines.layerContent', { scene: sn, n: li + 1 }) });
          const color = h('input', { type: 'color', value: l.color || '#000000', 'aria-label': t('pipelines.layerColor'), hidden: !!(l.source || l.image) });
          content.addEventListener('change', () => {
            delete l.source; delete l.image; delete l.color;
            if (content.value.startsWith('source:')) l.source = content.value.slice(7);
            else if (content.value.startsWith('image:')) l.image = content.value.slice(6);
            else l.color = color.value;
            color.hidden = content.value !== 'color';
          });
          color.addEventListener('input', () => { if (!l.source && !l.image) l.color = color.value; });
          const nm = (key, lbl, def, step = 1) => {
            const x = h('input', { type: 'number', value: l[key] ?? def, step, style: { width: '68px' }, 'aria-label': t('pipelines.layerField', { scene: sn, n: li + 1, field: lbl }) });
            x.addEventListener('change', () => { l[key] = Number(x.value); });
            return h('label', { class: 'small' }, lbl, x);
          };
          const vis = h('input', { type: 'checkbox', checked: l.visible !== false });
          vis.addEventListener('change', () => { l.visible = vis.checked; });
          const fit = select(['contain', 'cover', 'stretch'], l.fit || 'contain', { 'aria-label': t('pipelines.fit') });
          fit.addEventListener('change', () => { l.fit = fit.value; });
          return h('div', { class: 'inline', style: { flexWrap: 'wrap', gap: '4px' } }, content, color,
            nm('x', 'x', 0), nm('y', 'y', 0), nm('width', t('pipelines.width'), cw), nm('height', t('pipelines.height'), ch), nm('z', 'z', 0), nm('opacity', t('pipelines.opacity'), 1, 0.05), fit,
            h('label', { class: 'check small' }, vis, t('pipelines.visible')),
            h('button', { type: 'button', class: 'small ghost icon-only', 'aria-label': t('pipelines.removeLayer', { n: li + 1 }), onClick: () => { sc.layers.splice(li, 1); draw2(); } }, icon('close')));
        });
        return h('fieldset', null, h('legend', null, t('pipelines.sceneLegend', { scene: sn })),
          h('div', { class: 'inline' }, sname, h('button', { type: 'button', class: 'small danger', onClick: () => { delete scenes[sn]; draw2(); } }, icon('trash'), t('pipelines.deleteScene'))),
          ...layers,
          h('button', { type: 'button', class: 'small', onClick: () => {
            sc.layers = sc.layers || [];
            sc.layers.push(inputNames[0] ? { source: inputNames[0], x: 0, y: 0, width: cw, height: ch, z: sc.layers.length } : { color: '#333333', x: 0, y: 0, width: cw, height: ch });
            draw2();
          } }, icon('plus'), t('pipelines.addLayer')));
      }), h('button', { type: 'button', onClick: () => {
        let k = Object.keys(scenes).length + 1;
        while (scenes[`scene${k}`]) k += 1;
        scenes[`scene${k}`] = { layers: inputNames[0] ? [{ source: inputNames[0], x: 0, y: 0, width: cw, height: ch }] : [] };
        draw2();
      } }, icon('plus'), t('pipelines.addScene')));
    };
    draw2();
    const ok = await modal(t('pipelines.scenesTitle', { node: n.name }), () => [
      h('p', { class: 'small muted' }, t('pipelines.scenesLead', { w: cw, h: ch })), box,
    ], { wide: true, icon: 'layout-grid', actions: (close) => [h('button', { onClick: () => close(false) }, t('common.cancel')), h('button', { class: 'primary', onClick: () => close(true) }, t('pipelines.apply'))] });
    if (!ok) return;
    n.p.scenes = scenes;
    if (n.p.program && !scenes[n.p.program]) n.p.program = Object.keys(scenes)[0] || '';
    if (n.p.preview && !scenes[n.p.preview]) n.p.preview = '';
    changed();
    renderInspector();
  };

  // ---------------------------------------------------------------- 問題・状態・計画
  const renderIssues = () => {
    const errs = localIssues.filter((i) => i.level === 'error').length + remoteIssues.length;
    replace(issuesBox,
      h('h2', null, icon('check'), t('pipelines.checks'), h('span', { class: ['badge', errs ? 'danger' : 'ok'] }, errs ? t('pipelines.problems', { n: errs }) : t('pipelines.noProblems'))),
      remoteIssues.length ? h('div', null, h('h3', null, t('pipelines.remoteChecks')),
        h('ul', { class: 'pl-issues', 'data-remote': 'true' }, remoteIssues.map((i) => h('li', { class: 'error' }, icon('error', { size: 'sm' }), ' ', h('code', null, i.code), i.node ? [' [', h('code', null, i.node), ']'] : null, ' ', i.message)))) : null,
      localIssues.length
        ? h('div', null, remoteIssues.length ? h('h3', null, t('pipelines.localChecks')) : null,
          h('ul', { class: 'pl-issues' }, localIssues.map((i) => h('li', { class: i.level }, icon(i.level === 'error' ? 'error' : 'warning', { size: 'sm' }), ' ', i.text))))
        : remoteIssues.length ? null : h('p', { class: 'muted' }, t('pipelines.noProblemsLong')));
  };

  const renderPlan = () => replace(planBox, planView(plan || (cur && cur.remote && cur.remote.plan)));

  const renderRuntime = () => {
    const r = cur && cur.remote;
    if (!r) { replace(runtimeBox, h('p', { class: 'muted small' }, t('pipelines.draftRuntime'))); return; }
    const st = r.status && r.status.event === 'status' ? r.status : null;
    const hasCompose = (r.graph.nodes || []).some((x) => x.type === 'compose');
    replace(runtimeBox,
      kv([
        [t('common.state'), [badge(`desired: ${r.desired_state}`, r.desired_state === 'running' ? 'ok' : 'outline'), ' ', observedBadge(r.observed_state)]],
        [t('pipelines.generation'), String(r.generation)],
        [t('common.updated'), fmtTime(r.updated_at)],
        r.last_error ? [t('pipelines.lastError'), h('span', { class: 'field-error' }, r.last_error)] : null,
      ]),
      st ? table([
        { label: t('pipelines.input'), render: (x) => h('code', null, x.id) },
        { label: t('common.state'), render: (x) => badge(x.state, x.state === 'live' ? 'ok' : x.state === 'stalled' ? 'warn' : '') },
        { label: 'fps', class: 'num', render: (x) => (x.fps || 0).toFixed(1) },
        { label: t('pipelines.decoded'), class: 'num', render: (x) => String(x.decoded_frames) },
        { label: t('pipelines.tally'), render: (x) => (x.on_program ? badge('PGM', 'live') : x.on_preview ? badge('PVW', 'ok') : '') },
      ], st.sources || [], { empty: t('pipelines.noSourceState') }) : null,
      hasCompose ? h('p', null, h('a', { class: 'row', href: `#/production?pipeline=${encodeURIComponent(r.id)}` }, icon('switcher'), t('pipelines.openSwitcher'), icon('arrow-right', { size: 'sm' }))) : null);
  };

  const renderToolbar = () => {
    const r = cur && cur.remote;
    replace(toolbarState, r ? observedBadge(r.observed_state) : badge(t('pipelines.draft'), 'outline'), cur && cur.pl.dirty && cur.pl.remoteId ? h('span', { class: 'small muted' }, t('pipelines.unsaved')) : null);
    startBtn.disabled = !r || r.desired_state === 'running';
    stopBtn.disabled = !r || r.desired_state !== 'running';
  };

  // ---------------------------------------------------------------- 一覧と切り替え
  const entryFromRemote = (d, etag) => {
    const p = fromGraph(d.graph, { name: d.name, positions: layout[d.id] });
    p.remoteId = d.id;
    p.etag = etag || `"${d.generation}"`;
    p.dirty = false;
    return { pl: p, remote: d };
  };

  const entryFromDraft = (d) => {
    try {
      const p = fromGraph(d.graph, { name: d.name, positions: d.positions });
      p.key = d.key;
      p.dirty = true;
      return { pl: p, remote: null };
    } catch {
      return null;
    }
  };

  const switchTo = (key) => {
    const e = entries.find((x) => x.pl.key === key);
    if (!e) return;
    cur = e;
    sel = null;
    remoteIssues = [];
    plan = null;
    try { localStorage.setItem(SELECTED, e.pl.remoteId || e.pl.key); } catch { /* 何もしない */ }
    localIssues = validate(pl(), { royaltyFree: rf() });
    draw();
    renderIssues();
    renderInspector();
    renderList();
    renderRuntime();
    renderPlan();
    renderToolbar();
  };

  const addEntry = (e) => {
    entries.unshift(e);
    switchTo(e.pl.key);
    persistNow();
  };

  const loadRemote = async () => {
    try {
      const { data } = await api.list();
      available = true;
      const remote = data.items.map((d) => entryFromRemote(d));
      // 編集中の（未保存の）ものは残す
      const keep = new Map(entries.filter((e) => e.pl.remoteId && e.pl.dirty).map((e) => [e.pl.remoteId, e]));
      entries = [...remote.map((e) => keep.get(e.pl.remoteId) || e), ...entries.filter((e) => !e.pl.remoteId)];
    } catch (e) {
      if (!offline(e)) reportError(e, t('pipelines.list'));
    }
    renderBanner();
  };

  // ---------------------------------------------------------------- 操作
  const place = (type) => {
    const p = pl();
    const n = addNode(p, type, 40 + (p.nodes.length % 6) * 210, 40 + (Math.floor(p.nodes.length / 6) % 5) * 110);
    sel = { type: 'node', id: n.id };
    focusNode = n.name;
    changed();
    renderInspector();
  };

  const showRemoteError = (e, what) => {
    if (offline(e)) return;
    if (e.status === 412) {
      toast(t('pipelines.stale'), 'warn', 9000);
      return;
    }
    if (e.details && (e.details.issues || e.details.reason)) {
      remoteIssues = issuesOf(e);
      renderIssues();
      draw();
    }
    reportError(e, what);
  };

  const save = async () => {
    const p = pl();
    localIssues = validate(p, { royaltyFree: rf() });
    renderIssues();
    if (localIssues.some((i) => i.level === 'error')) {
      toast(t('pipelines.fixBeforeSave'), 'warn');
      return;
    }
    const graph = toGraph(p);
    try {
      let r;
      if (p.remoteId) {
        const body = { graph };
        if (!cur.remote || cur.remote.name !== p.name) body.name = p.name;
        r = await api.update(p.remoteId, body, p.etag);
      } else {
        r = await api.create({ name: p.name, graph });
        drafts = drafts.filter((d) => d.key !== p.key);
        writeJson(DRAFTS, drafts);
      }
      p.remoteId = r.data.id;
      p.etag = r.etag || `"${r.data.generation}"`;
      p.dirty = false;
      cur.remote = r.data;
      remoteIssues = [];
      persistNow();
      renderList();
      renderRuntime();
      renderToolbar();
      renderIssues();
      draw();
      toast(t('pipelines.saved'), 'ok');
    } catch (e) {
      showRemoteError(e, t('common.save'));
    }
  };

  const validateRemote = async () => {
    const p = pl();
    localIssues = validate(p, { royaltyFree: rf() });
    renderIssues();
    draw();
    if (!p.remoteId) {
      const bad = localIssues.some((i) => i.level === 'error');
      toast(bad ? t('pipelines.localBad') : t('pipelines.localOk'), bad ? 'warn' : 'ok');
      return;
    }
    try {
      const r = await api.validate(p.remoteId, toGraph(p));
      plan = r.data.plan;
      remoteIssues = [];
      renderIssues();
      renderPlan();
      draw();
      toast(t('pipelines.validated'), 'ok');
    } catch (e) {
      plan = null;
      renderPlan();
      if (e.details && (e.details.issues || e.details.reason)) {
        remoteIssues = issuesOf(e);
        renderIssues();
        draw();
        toast(t('pipelines.validateIssues', { n: remoteIssues.length }), 'warn', 7000);
      } else showRemoteError(e, t('pipelines.validate'));
    }
  };

  const setDesired = async (want) => {
    const p = pl();
    if (!p.remoteId) return;
    if (p.dirty && want === 'running') {
      if (!(await confirmDialog(t('pipelines.unsavedTitle'), t('pipelines.unsavedBody'), { ok: t('pipelines.start'), danger: false }))) return;
    }
    try {
      const r = await api.setDesired(p.remoteId, want, p.etag);
      p.etag = r.etag || `"${r.data.generation}"`;
      cur.remote = r.data;
      renderRuntime();
      renderToolbar();
      renderList();
      toast(want === 'running' ? t('pipelines.started') : t('pipelines.stopped'), 'ok');
    } catch (e) {
      showRemoteError(e, want === 'running' ? t('pipelines.start') : t('pipelines.stop'));
    }
  };

  const reloadCurrent = async () => {
    const p = pl();
    if (!p.remoteId) return;
    try {
      const r = await api.get(p.remoteId);
      const e = entryFromRemote(r.data, r.etag);
      e.pl.key = p.key;
      cur.pl = e.pl;
      cur.remote = r.data;
      switchTo(p.key);
      toast(t('pipelines.reloaded'), 'ok');
    } catch (e) {
      showRemoteError(e, t('common.refresh'));
    }
  };

  const remove = async () => {
    const p = pl();
    if (p.remoteId) {
      if (!(await confirmDialog(t('pipelines.deleteTitle'), t('pipelines.deleteBody', { name: p.name }), { ok: t('common.delete'), requireText: p.name }))) return;
      try {
        await api.remove(p.remoteId);
        delete layout[p.remoteId];
        writeJson(LAYOUT, layout);
      } catch (e) {
        showRemoteError(e, t('common.delete'));
        return;
      }
    } else {
      if (!(await confirmDialog(t('pipelines.deleteDraftTitle'), t('pipelines.deleteDraftBody', { name: p.name }), { ok: t('common.delete') }))) return;
      drafts = drafts.filter((d) => d.key !== p.key);
      writeJson(DRAFTS, drafts);
    }
    entries = entries.filter((e) => e !== cur);
    if (!entries.length) entries = [{ pl: emptyPipeline(), remote: null }];
    switchTo(entries[0].pl.key);
    toast(t('common.deleted'), 'ok');
  };

  const io = async () => {
    const body = { name: pl().name, graph: toGraph(pl()) };
    const ta = h('textarea', { rows: 18, 'aria-label': t('pipelines.jsonLabel'), spellcheck: 'false' }, JSON.stringify(body, null, 2));
    const v = await modal(t('pipelines.jsonTitle'), () => [h('p', { class: 'small muted' }, t('pipelines.jsonLead')), ta], {
      wide: true,
      icon: 'json',
      actions: (close) => [h('button', { onClick: () => close(null) }, t('common.close')), h('button', { class: 'primary', onClick: () => close(ta.value) }, icon('upload'), t('pipelines.import'))],
    });
    if (!v) return;
    try {
      const j = JSON.parse(v);
      const graph = j.graph || j;
      const next = fromGraph(graph, { name: j.name || `imported-${Date.now().toString(36)}` });
      next.dirty = true;
      addEntry({ pl: next, remote: null });
      toast(t('pipelines.imported'), 'ok');
    } catch (e) {
      toast(t('pipelines.importFailed', { reason: e.message }), 'error');
    }
  };

  const startBtn = h('button', { onClick: () => setDesired('running'), 'data-testid': 'pipeline-start' }, icon('play'), t('pipelines.start'));
  const stopBtn = h('button', { onClick: () => setDesired('stopped'), 'data-testid': 'pipeline-stop' }, icon('stop'), t('pipelines.stop'));
  listSel.addEventListener('change', () => switchTo(listSel.value));

  const newDraft = () => addEntry({ pl: Object.assign(emptyPipeline(`pipeline-${Date.now().toString(36)}`), { dirty: true }), remote: null });
  const addSample = () => {
    const rfs = streamList.filter((x) => x.codec_profile === 'royalty_free');
    const p = samplePipeline({ a: rfs[0] ? rfs[0].id : '', b: rfs[1] ? rfs[1].id : '', out: rfs[2] ? rfs[2].id : '' });
    p.name = `pinp-${Date.now().toString(36)}`;
    p.dirty = true;
    addEntry({ pl: p, remote: null });
  };
  const jsonBtn = h('button', { type: 'button', onClick: io }, icon('json'), 'JSON');

  root.append(
    pageHeader({
      title: t('nav.pipelines'),
      description: t('pipelines.lead'),
      help: helpHint({ title: t('nav.pipelines'), body: t('pipelines.help'), guide: '07-pipelines.md' }),
      actions: [
        h('label', { class: 'visually-hidden', for: 'pl-pick' }, t('pipelines.pick')), listSel,
        button(t('pipelines.new'), { icon: 'plus', onClick: newDraft }),
        button(t('pipelines.sample'), { icon: 'lightbulb', onClick: addSample }),
        jsonBtn,
        iconButton('trash', t('pipelines.deleteThis'), { variant: 'danger', onClick: remove }),
      ],
    }),
    banner,
    h('div', { class: 'toolbar pl-toolbar' },
      h('button', { type: 'button', onClick: validateRemote }, icon('check'), t('pipelines.validate')),
      h('button', { type: 'button', class: 'primary', onClick: save }, icon('save'), t('common.save')),
      startBtn, stopBtn,
      iconButton('refresh', t('pipelines.reload'), { variant: 'secondary', onClick: reloadCurrent }),
      h('span', { class: 'spacer' }), toolbarState),
    h('div', { class: 'pl-layout' },
      h('div', { class: 'pl-palette', role: 'group', 'aria-labelledby': 'pl-pal-h' },
        h('div', { class: 'pl-palette-title', id: 'pl-pal-h' }, t('pipelines.palette')),
        Object.entries(KINDS).map(([type, k]) => h('button', { type: 'button', onClick: () => place(type), title: type, 'data-type': type },
          h('span', { class: 'dot', style: { background: k.color } }), k.label)),
        h('p', { class: 'small muted' }, t('pipelines.paletteHint'))),
      h('div', { class: 'pl-canvas' }, svg),
      h('div', { class: 'pl-side' }, inspector,
        h('section', { class: 'panel', 'aria-label': t('pipelines.checks') }, issuesBox),
        panel({ title: t('pipelines.runtime'), icon: 'activity' }, runtimeBox))),
    panel({ title: t('pipelines.plan'), icon: 'list' }, planBox),
    h('p', { class: 'small muted' }, t('pipelines.footer', { list: transitions().map((x) => x[1]).join(t('pipelines.listSep')) })));

  try { streamList = await listAll(streams.list); } catch { /* 選べないだけ */ }
  entries = drafts.map(entryFromDraft).filter(Boolean);
  await loadRemote();
  if (!entries.length) entries = [{ pl: Object.assign(emptyPipeline(), { dirty: true }), remote: null }];
  const want = (() => { const q = new URLSearchParams(location.hash.split('?')[1] || ''); return q.get('id') || localStorage.getItem(SELECTED); })();
  const first = entries.find((e) => e.pl.remoteId === want || e.pl.key === want) || entries[0];
  switchTo(first.pl.key);

  // 動いているものの状態を読む（編集中のグラフは上書きしない）
  const poll = async () => {
    if (dead || !cur || !cur.pl.remoteId || document.visibilityState !== 'visible') return;
    try {
      const r = await api.get(cur.pl.remoteId);
      if (dead) return;
      const before = cur.remote;
      cur.remote = r.data;
      if (!cur.pl.dirty && before && before.generation !== r.data.generation) {
        const e = entryFromRemote(r.data, r.etag);
        e.pl.key = cur.pl.key;
        cur.pl = e.pl;
        draw();
        renderInspector();
      } else if (!cur.pl.dirty) cur.pl.etag = r.etag || cur.pl.etag;
      renderRuntime();
      renderToolbar();
      if (!plan) renderPlan();
    } catch (e) {
      if (e.status === 404) {
        toast(t('pipelines.gone'), 'warn');
        entries = entries.filter((x) => x !== cur);
        if (!entries.length) entries = [{ pl: emptyPipeline(), remote: null }];
        switchTo(entries[0].pl.key);
      }
    }
  };
  pollTimer = setInterval(poll, 2000);

  return () => {
    dead = true;
    clearInterval(pollTimer);
    persist.cancel();
    persistNow();
  };
}
