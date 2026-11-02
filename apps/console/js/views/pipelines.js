// パイプラインの編集（REQ-005）: SVG 上で部品を置いて線でつなぐ。右の検査器で設定する。
//
// グラフは senca-pipeline の JSON（js/pipeline/model.js が行き来する）。ノードの API（js/api/pipelines.js）で
// 保存（POST・PATCH ＋ If-Match）・検証（POST /pipelines/{id}/validate、計画と問題の一覧）・開始/停止
// （PATCH desired_state）・削除をする。保存前の下書きと部品の位置はこのブラウザ（localStorage）に置く
// （グラフは知らない項目を拒否するので、位置はグラフに入れられない）。
//
// 操作: 左の部品を押すと置く。部品はドラッグで動かす。右の丸（出力）から左の丸（入力）へドラッグでつなぐ。
// キーボード: Tab で部品を選び、矢印で動かす（Shift で大きく）、Delete で消す。つなぐのは検査器からもできる。

import { h, s, replace, field, select, debounce, table, badge, kv } from '../lib/dom.js';
import { modal, confirmDialog, toast, reportError } from '../lib/ui.js';
import { pipelines as api, NotAvailableError, issuesOf } from '../api/pipelines.js';
import { streams, listAll } from '../api/resources.js';
import { fmtTime } from '../lib/format.js';
import {
  KINDS, PORT_LABEL, OPTION_FIELDS, NODE_W, TRANSITIONS, nodeHeight, inPorts, outPorts, outType, compatible, emptyPipeline, addNode, removeNode,
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

export const observedBadge = (st) => {
  const kind = { running: 'live', starting: 'warn', error: 'danger', stopped: '' }[st] ?? '';
  return badge(st || '—', kind);
};

/** 計画（senca-pipeline の Plan）の表。 */
export function planView(plan) {
  if (!plan) return h('p', { class: 'muted' }, '計画はまだありません。「検証」でノードに計画を作らせます。');
  const t = plan.totals || {};
  return h('div', null,
    kv([
      ['実行の順', (plan.order || []).join(' → ')],
      ['合計', `画素 ${Math.round((t.pixels_per_sec || 0) / 1e6)} M/s・CPU のコーデック ${t.cpu_codec_sessions ?? 0}・GPU のコーデック ${t.gpu_codec_sessions ?? 0}・スレッド ${t.threads ?? 0}・最大の遅延 ${Math.round(t.max_latency_ms || 0)} ms`],
    ]),
    table([
      { label: '部品', render: (n) => h('code', null, n.id) },
      { label: '種類', render: (n) => n.type },
      { label: '扱い', render: (n) => badge(n.mode, { passthrough: 'ok', transcode: 'warn', process: '', terminal: '' }[n.mode] || '') },
      { label: 'コーデック', render: (n) => (n.codec ? [h('code', null, n.codec), n.hardware === true ? '（HW）' : n.hardware === false ? '（SW）' : '', n.provider ? ` ${n.provider}` : ''] : '—') },
      { label: '出力', render: (n) => (n.outputs || []).join('・') || '—' },
      { label: '遅延', class: 'num', render: (n) => `${Math.round(n.added_latency_ms || 0)} ms` },
      { label: '注記', render: (n) => (n.notes || []).join(' / ') },
    ], plan.nodes || [], { caption: '部品ごとの計画' }),
    h('h3', null, '出力'),
    table([
      { label: 'sink', render: (x) => h('code', null, x.sink) },
      { label: 'stream', render: (x) => h('a', { href: `#/streams/${encodeURIComponent(x.stream)}` }, h('code', null, x.stream)) },
      { label: 'トラック', render: (x) => `${x.track}（${x.kind}）` },
      { label: 'コーデック', render: (x) => h('code', null, x.codec) },
      { label: '扱い', render: (x) => x.mode },
      { label: '元', render: (x) => x.from },
      { label: '遅延', class: 'num', render: (x) => `${Math.round(x.latency_ms || 0)} ms` },
    ], (plan.sinks || []).flatMap((sk) => sk.tracks.map((tr) => ({ sink: sk.id, stream: sk.stream, ...tr }))), { empty: '出力はありません' }));
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
  const listSel = h('select', { id: 'pl-pick', 'aria-label': 'パイプラインを選ぶ' });
  const svg = s('svg', { role: 'application', 'aria-label': 'パイプラインのグラフ', viewBox: '0 0 1400 700', width: 1400, height: 700 });
  const inspector = h('div', { class: 'panel', style: { marginBottom: '0' } });
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

  const label = (e) => `${e.pl.name}${e.pl.remoteId ? '' : '（下書き）'}${e.pl.dirty && e.pl.remoteId ? ' *' : ''}`;
  const renderList = () => {
    replace(listSel, entries.map((e) => h('option', { value: e.pl.key, selected: e === cur }, label(e))));
  };

  const renderBanner = () => {
    replace(banner, available === false
      ? h('div', { class: 'alert warn offline-banner', role: 'status' },
        h('strong', null, 'このノードでは使えません'),
        h('span', null, 'パイプラインの API（/pipelines）がありません。編集と検査はこのブラウザの中だけで動き、下書きは localStorage に残ります。'))
      : null);
  };

  const offline = (e) => {
    if (e instanceof NotAvailableError) {
      available = false;
      renderBanner();
      toast('パイプラインの API はこのノードでは使えません。下書きはこのブラウザに保存しました', 'warn', 7000);
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
        ? ((streamList.find((x) => x.id === n.p.stream) || {}).name || n.p.stream || 'stream 未設定')
        : n.type === 'encode' ? `${n.p.codec || '?'} ${n.p.bitrate_kbps || ''}k` : k.label;
      const g = s('g', {
        class: ['pl-node', isSel && 'selected', bad.has(n.id) && 'invalid'], transform: `translate(${n.x},${n.y})`, tabindex: '0',
        role: 'button', 'aria-label': `${k.label}「${n.name}」。矢印で移動、Delete で削除`, 'data-node': n.name,
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
          s('title', null, `入力「${port.name}」`));
        g.appendChild(c);
      });
      outPorts(n).forEach((o, i) => {
        const q = portPos({ ...n, x: 0, y: 0 }, 'out', i);
        const t = outType(p, { node: n.id, port: i });
        const c = s('circle', { class: 'pl-port', cx: q.x, cy: q.y, r: 7 }, s('title', null, `出力「${o.name}」: ${PORT_LABEL[t] || t}（ドラッグでつなぐ）`));
        c.addEventListener('pointerdown', (ev) => {
          ev.stopPropagation();
          ev.preventDefault();
          const from = portPos(n, 'out', i);
          wire = { from: { node: n.id, port: i }, type: t, a: from, b: from };
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
    if (f.type === 'select') input = select(f.options.map((o) => (Array.isArray(o) ? o : [o, o === '' ? '（使わない）' : o])), v ?? f.def ?? '');
    else if (f.type === 'stream') input = select([['', '（選ぶ）'], ...streamList.map((x) => [x.id, `${x.name}（${x.codec_profile}）`]), ...(v && !streamList.some((x) => x.id === v) ? [[v, v]] : [])], v || '');
    else if (f.type === 'scene') input = select([['', '（なし）'], ...Object.keys((n && n.p.scenes) || {}).map((x) => [x, x])], v || '');
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
    return field(f.label + (f.required ? '（必須）' : ''), input);
  };

  const namedInputsEditor = (n) => {
    const isMix = n.type === 'audio_mix';
    const rows = n.ins.map((inp, i) => {
      const name = h('input', { type: 'text', value: inp.name, 'aria-label': `入力 ${i + 1} の名前`, style: { width: '110px' } });
      name.addEventListener('change', () => {
        const old = inp.name;
        if (!isNodeName(name.value)) { toast('入力名は英数字と _ - です', 'warn'); name.value = old; return; }
        inp.name = name.value;
        if (n.type === 'compose') {
          for (const sc of Object.values(n.p.scenes || {})) for (const l of sc.layers || []) if (l.source === old) l.source = inp.name;
        }
        changed();
        renderInspector();
      });
      const up = pl().edges.find((e) => e.to.node === n.id && e.to.port === i);
      const upName = up ? (() => { const m = pl().nodes.find((x) => x.id === up.from.node); return m ? `${m.name}.${outPorts(m)[up.from.port].name}` : '?'; })() : '（未接続）';
      const extras = [];
      if (isMix) {
        const numIn = (key, labelText, step = 0.5) => {
          const x = h('input', { type: 'number', step, value: inp[key] ?? 0, style: { width: '64px' }, 'aria-label': `${inp.name} の${labelText}` });
          x.addEventListener('change', () => { inp[key] = Number(x.value) || 0; changed(); });
          return h('label', { class: 'small' }, labelText, x);
        };
        const ck = (key, labelText) => {
          const x = h('input', { type: 'checkbox', checked: !!inp[key] });
          x.addEventListener('change', () => { inp[key] = x.checked; changed(); });
          return h('label', { class: 'check small' }, x, labelText);
        };
        extras.push(numIn('gain_db', 'dB'), numIn('pan', 'pan', 0.1), numIn('delay_ms', '遅延 ms', 1), ck('mute', 'ミュート'), ck('solo', 'ソロ'));
      }
      return h('div', { class: 'inline', style: { flexWrap: 'wrap', gap: '4px', borderBottom: '1px solid var(--border)', paddingBottom: '4px' } },
        name, h('span', { class: 'small muted' }, `← ${upName}`), ...extras,
        h('button', { type: 'button', class: 'small danger', 'aria-label': `入力「${inp.name}」を消す`, onClick: () => { removeNamedInput(pl(), n, i); changed(); renderInspector(); } }, '×'));
    });
    const add = h('button', { type: 'button', class: 'small', onClick: () => {
      let k = n.ins.length + 1;
      while (n.ins.some((x) => x.name === `in${k}`)) k += 1;
      n.ins.push(isMix ? { name: `in${k}`, gain_db: 0, mute: false, solo: false, pan: 0, delay_ms: 0 } : { name: `in${k}` });
      changed();
      renderInspector();
    } }, '＋ 入力を足す');
    return h('fieldset', null, h('legend', null, isMix ? 'ミキサーの入力' : '合成の入力（シーンから名前で参照）'), ...rows, add);
  };

  const renderInspector = () => {
    const p = pl();
    if (sel && sel.type === 'node') {
      const n = p.nodes.find((m) => m.id === sel.id);
      if (!n) { sel = null; renderInspector(); return; }
      const k = KINDS[n.type];
      const name = h('input', { type: 'text', value: n.name, maxlength: 64, 'aria-label': '部品の名前（id）' });
      name.addEventListener('change', () => {
        if (!isNodeName(name.value) || p.nodes.some((m) => m !== n && m.name === name.value)) { toast('名前は英数字と _ - の 1〜64 文字で、重ならないものにしてください', 'warn'); name.value = n.name; return; }
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
        const t = outType(p, { node: n.id, port: oi });
        for (const m of p.nodes) {
          if (m.id === n.id) continue;
          inPorts(m).forEach((port, ii) => {
            if (compatible(t, port.accept) && !p.edges.some((e) => e.to.node === m.id && e.to.port === ii)) {
              targets.push([`${oi}|${m.id}|${ii}`, `${o.name} → ${m.name}（${port.name}）`]);
            }
          });
        }
      });
      const target = select(targets.length ? targets : [['', 'つなげる先がありません']], '');
      target.id = `tgt-${n.id}`;
      const linkBtn = h('button', { class: 'small', disabled: !targets.length, onClick: () => {
        const [oi, mid, ii] = target.value.split('|');
        const r = connect(p, { node: n.id, port: Number(oi) }, { node: mid, port: Number(ii) });
        if (!r.ok) toast(r.reason, 'warn');
        changed();
        renderInspector();
      } }, 'つなぐ');
      const extraKeys = Object.keys(n.extra || {});
      replace(inspector,
        h('h2', null, h('span', { class: 'dot', style: { background: k.color } }), k.label),
        h('div', { class: 'form' },
          field('名前（グラフの id）', name),
          ...fields,
          k.named ? namedInputsEditor(n) : null,
          n.type === 'compose' ? h('div', { class: 'inline' }, h('button', { type: 'button', onClick: () => editScenes(n) }, 'シーンを編集'),
            h('span', { class: 'small muted' }, `${Object.keys(n.p.scenes || {}).length} 個`)) : null,
          extraKeys.length ? h('div', { class: 'small muted' }, `この画面で編集しない項目（そのまま保存）: ${extraKeys.join('・')}`) : null,
          outPorts(n).length ? h('div', { class: 'field' }, h('label', { for: target.id }, 'つなぐ（キーボード）'), h('div', { class: 'inline' }, target, linkBtn)) : null,
          h('div', { class: 'small muted' }, `入力: ${inPorts(n).map((x) => x.name).join('・') || 'なし'} / 出力: ${outPorts(n).map((o, i) => `${o.name}（${PORT_LABEL[outType(p, { node: n.id, port: i })] || '?'}）`).join('・') || 'なし'}`),
          h('div', { class: 'actions' }, h('button', { class: 'danger', onClick: () => { removeNode(p, n.id); sel = null; changed(); renderInspector(); } }, 'この部品を削除'))));
      return;
    }
    if (sel && sel.type === 'edge') {
      const e = p.edges.find((x) => x.id === sel.id);
      if (!e) { sel = null; renderInspector(); return; }
      const a = p.nodes.find((n) => n.id === e.from.node);
      const b = p.nodes.find((n) => n.id === e.to.node);
      replace(inspector, h('h2', null, '接続'),
        h('p', null, `${a.name}.${outPorts(a)[e.from.port].name} → ${b.name}（${inPorts(b)[e.to.port].name}）`),
        h('p', { class: 'muted small' }, `型: ${PORT_LABEL[outType(p, e.from)] || '?'}`),
        h('button', { class: 'danger', onClick: () => { p.edges = p.edges.filter((x) => x.id !== e.id); sel = null; changed(); renderInspector(); } }, 'この接続を削除'));
      return;
    }
    const name = h('input', { type: 'text', value: p.name, maxlength: 64, 'aria-label': 'パイプラインの名前' });
    name.addEventListener('change', () => { p.name = name.value; changed(); renderList(); });
    const opts = OPTION_FIELDS.map((f) => fieldInput(f, () => p.options[f.path], (v) => { p.options[f.path] = v; changed(); }));
    replace(inspector, h('h2', null, 'パイプライン'),
      h('div', { class: 'form' },
        field('名前（英数字と . _ -）', name),
        h('details', null, h('summary', null, '全体の設定（options）'), h('div', { class: 'form' }, ...opts)),
        h('div', { class: 'small muted' }, `部品 ${p.nodes.length} 個・接続 ${p.edges.length} 本`),
        p.remoteId ? h('div', { class: 'small' }, 'ID: ', h('code', null, p.remoteId)) : h('div', { class: 'small muted' }, 'まだノードに保存していません（下書き）'),
        h('p', { class: 'small muted' }, '部品を選ぶと、ここで設定できます。')));
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
        const sname = h('input', { type: 'text', value: sn, 'aria-label': 'シーンの名前', style: { width: '160px' } });
        sname.addEventListener('change', () => {
          if (!sname.value || scenes[sname.value]) { sname.value = sn; return; }
          const ordered = Object.fromEntries(Object.entries(scenes).map(([k, v]) => (k === sn ? [sname.value, v] : [k, v])));
          for (const k of Object.keys(scenes)) delete scenes[k];
          Object.assign(scenes, ordered);
          draw2();
        });
        const layers = (sc.layers || []).map((l, li) => {
          const content = select([...inputNames.map((x) => [`source:${x}`, `入力: ${x}`]), ['color', '色'], ...(l.image ? [[`image:${l.image}`, `画像: ${l.image}`]] : [])],
            l.source ? `source:${l.source}` : l.image ? `image:${l.image}` : 'color', { 'aria-label': `${sn} のレイヤー ${li + 1} の中身` });
          const color = h('input', { type: 'color', value: l.color || '#000000', 'aria-label': '色', hidden: !!(l.source || l.image) });
          content.addEventListener('change', () => {
            delete l.source; delete l.image; delete l.color;
            if (content.value.startsWith('source:')) l.source = content.value.slice(7);
            else if (content.value.startsWith('image:')) l.image = content.value.slice(6);
            else l.color = color.value;
            color.hidden = content.value !== 'color';
          });
          color.addEventListener('input', () => { if (!l.source && !l.image) l.color = color.value; });
          const nm = (key, lbl, def, step = 1) => {
            const x = h('input', { type: 'number', value: l[key] ?? def, step, style: { width: '68px' }, 'aria-label': `${sn} のレイヤー ${li + 1} の ${lbl}` });
            x.addEventListener('change', () => { l[key] = Number(x.value); });
            return h('label', { class: 'small' }, lbl, x);
          };
          const vis = h('input', { type: 'checkbox', checked: l.visible !== false });
          vis.addEventListener('change', () => { l.visible = vis.checked; });
          const fit = select(['contain', 'cover', 'stretch'], l.fit || 'contain', { 'aria-label': '縦横比' });
          fit.addEventListener('change', () => { l.fit = fit.value; });
          return h('div', { class: 'inline', style: { flexWrap: 'wrap', gap: '4px' } }, content, color,
            nm('x', 'x', 0), nm('y', 'y', 0), nm('width', '幅', cw), nm('height', '高さ', ch), nm('z', 'z', 0), nm('opacity', '不透明度', 1, 0.05), fit,
            h('label', { class: 'check small' }, vis, '表示'),
            h('button', { type: 'button', class: 'small danger', onClick: () => { sc.layers.splice(li, 1); draw2(); } }, '×'));
        });
        return h('fieldset', null, h('legend', null, `シーン「${sn}」`),
          h('div', { class: 'inline' }, sname, h('button', { type: 'button', class: 'small danger', onClick: () => { delete scenes[sn]; draw2(); } }, 'シーンを削除')),
          ...layers,
          h('button', { type: 'button', class: 'small', onClick: () => {
            sc.layers = sc.layers || [];
            sc.layers.push(inputNames[0] ? { source: inputNames[0], x: 0, y: 0, width: cw, height: ch, z: sc.layers.length } : { color: '#333333', x: 0, y: 0, width: cw, height: ch });
            draw2();
          } }, '＋ レイヤー'));
      }), h('button', { type: 'button', onClick: () => {
        let k = Object.keys(scenes).length + 1;
        while (scenes[`scene${k}`]) k += 1;
        scenes[`scene${k}`] = { layers: inputNames[0] ? [{ source: inputNames[0], x: 0, y: 0, width: cw, height: ch }] : [] };
        draw2();
      } }, '＋ シーン'));
    };
    draw2();
    const ok = await modal(`「${n.name}」のシーン`, () => [
      h('p', { class: 'small muted' }, `位置と大きさはキャンバス（${cw}×${ch}）の画素。z が大きいほど手前。運用中の切り替え・位置の変更はスイッチャーの画面から（保存はここ）。`), box,
    ], { wide: true, actions: (close) => [h('button', { onClick: () => close(false) }, 'やめる'), h('button', { class: 'primary', onClick: () => close(true) }, '決定')] });
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
      h('h2', null, '検査', h('span', { class: ['badge', errs ? 'danger' : 'ok'] }, errs ? `問題 ${errs}` : '問題なし')),
      remoteIssues.length ? h('div', null, h('h3', null, 'ノードの検証'),
        h('ul', { class: 'pl-issues', 'data-remote': 'true' }, remoteIssues.map((i) => h('li', { class: 'error' }, '✕ ', h('code', null, i.code), i.node ? [' [', h('code', null, i.node), ']'] : null, ' ', i.message)))) : null,
      localIssues.length
        ? h('div', null, remoteIssues.length ? h('h3', null, 'この画面の検査') : null,
          h('ul', { class: 'pl-issues' }, localIssues.map((i) => h('li', { class: i.level }, i.level === 'error' ? '✕ ' : '! ', i.text))))
        : remoteIssues.length ? null : h('p', { class: 'muted' }, '問題はありません。'));
  };

  const renderPlan = () => replace(planBox, planView(plan || (cur && cur.remote && cur.remote.plan)));

  const renderRuntime = () => {
    const r = cur && cur.remote;
    if (!r) { replace(runtimeBox, h('p', { class: 'muted small' }, '下書きです。「保存」でノードに作ります（止めた状態で作ります）。')); return; }
    const st = r.status && r.status.event === 'status' ? r.status : null;
    const hasCompose = (r.graph.nodes || []).some((x) => x.type === 'compose');
    replace(runtimeBox,
      kv([
        ['状態', [badge(`desired: ${r.desired_state}`, r.desired_state === 'running' ? 'ok' : ''), ' ', observedBadge(r.observed_state)]],
        ['世代', String(r.generation)],
        ['更新', fmtTime(r.updated_at)],
        r.last_error ? ['最後の誤り', h('span', { class: 'field-error' }, r.last_error)] : null,
      ]),
      st ? table([
        { label: '入力', render: (x) => h('code', null, x.id) },
        { label: '状態', render: (x) => badge(x.state, x.state === 'live' ? 'ok' : x.state === 'stalled' ? 'warn' : '') },
        { label: 'fps', class: 'num', render: (x) => (x.fps || 0).toFixed(1) },
        { label: '復号', class: 'num', render: (x) => String(x.decoded_frames) },
        { label: 'タリー', render: (x) => (x.on_program ? badge('PGM', 'live') : x.on_preview ? badge('PVW', 'ok') : '') },
      ], st.sources || [], { empty: '入力の状態はまだありません' }) : null,
      hasCompose ? h('p', null, h('a', { href: `#/production?pipeline=${encodeURIComponent(r.id)}` }, 'スイッチャーで操作する →')) : null);
  };

  const renderToolbar = () => {
    const r = cur && cur.remote;
    replace(toolbarState, r ? observedBadge(r.observed_state) : badge('下書き', ''), cur && cur.pl.dirty && cur.pl.remoteId ? h('span', { class: 'small muted' }, '未保存の変更あり') : null);
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
      if (!offline(e)) reportError(e, 'パイプラインの一覧');
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
      toast('他の操作で更新されていました。「再読み込み」で最新を読み、もう一度操作してください', 'warn', 9000);
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
      toast('検査のエラーを直してから保存してください', 'warn');
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
      toast('ノードに保存しました', 'ok');
    } catch (e) {
      showRemoteError(e, '保存');
    }
  };

  const validateRemote = async () => {
    const p = pl();
    localIssues = validate(p, { royaltyFree: rf() });
    renderIssues();
    draw();
    if (!p.remoteId) {
      const bad = localIssues.some((i) => i.level === 'error');
      toast(bad ? 'この画面の検査でエラーがあります' : 'この画面の検査に通りました（ノードでの検証は保存後）', bad ? 'warn' : 'ok');
      return;
    }
    try {
      const r = await api.validate(p.remoteId, toGraph(p));
      plan = r.data.plan;
      remoteIssues = [];
      renderIssues();
      renderPlan();
      draw();
      toast('ノードの検証に通りました', 'ok');
    } catch (e) {
      plan = null;
      renderPlan();
      if (e.details && (e.details.issues || e.details.reason)) {
        remoteIssues = issuesOf(e);
        renderIssues();
        draw();
        toast(`ノードの検証で問題があります（${remoteIssues.length} 件）`, 'warn', 7000);
      } else showRemoteError(e, '検証');
    }
  };

  const setDesired = async (want) => {
    const p = pl();
    if (!p.remoteId) return;
    if (p.dirty && want === 'running') {
      if (!(await confirmDialog('未保存の変更', '画面の変更はまだ保存していません。保存済みのグラフで開始します。', { ok: '開始', danger: false }))) return;
    }
    try {
      const r = await api.setDesired(p.remoteId, want, p.etag);
      p.etag = r.etag || `"${r.data.generation}"`;
      cur.remote = r.data;
      renderRuntime();
      renderToolbar();
      renderList();
      toast(want === 'running' ? '開始しました' : '停止しました', 'ok');
    } catch (e) {
      showRemoteError(e, want === 'running' ? '開始' : '停止');
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
      toast('ノードから読み直しました', 'ok');
    } catch (e) {
      showRemoteError(e, '読み込み');
    }
  };

  const remove = async () => {
    const p = pl();
    if (p.remoteId) {
      if (!(await confirmDialog('パイプラインを削除', `「${p.name}」をノードから削除します。動いていれば止まります。`, { ok: '削除', requireText: p.name }))) return;
      try {
        await api.remove(p.remoteId);
        delete layout[p.remoteId];
        writeJson(LAYOUT, layout);
      } catch (e) {
        showRemoteError(e, '削除');
        return;
      }
    } else {
      if (!(await confirmDialog('下書きを削除', `「${p.name}」をこのブラウザから削除します。`, { ok: '削除' }))) return;
      drafts = drafts.filter((d) => d.key !== p.key);
      writeJson(DRAFTS, drafts);
    }
    entries = entries.filter((e) => e !== cur);
    if (!entries.length) entries = [{ pl: emptyPipeline(), remote: null }];
    switchTo(entries[0].pl.key);
    toast('削除しました', 'ok');
  };

  const io = async () => {
    const body = { name: pl().name, graph: toGraph(pl()) };
    const ta = h('textarea', { rows: 18, 'aria-label': 'パイプラインの定義（JSON）', spellcheck: 'false' }, JSON.stringify(body, null, 2));
    const v = await modal('定義（JSON）', () => [h('p', { class: 'small muted' }, 'POST /pipelines の本文（{name, graph}）です。グラフだけ（{version, nodes}）も貼り付けて取り込めます。取り込むと新しい下書きになります。'), ta], {
      wide: true,
      actions: (close) => [h('button', { onClick: () => close(null) }, '閉じる'), h('button', { class: 'primary', onClick: () => close(ta.value) }, '取り込む')],
    });
    if (!v) return;
    try {
      const j = JSON.parse(v);
      const graph = j.graph || j;
      const next = fromGraph(graph, { name: j.name || `imported-${Date.now().toString(36)}` });
      next.dirty = true;
      addEntry({ pl: next, remote: null });
      toast('取り込みました（下書き）', 'ok');
    } catch (e) {
      toast(`取り込めません: ${e.message}`, 'error');
    }
  };

  const startBtn = h('button', { onClick: () => setDesired('running') }, '▶ 開始');
  const stopBtn = h('button', { onClick: () => setDesired('stopped') }, '■ 停止');
  listSel.addEventListener('change', () => switchTo(listSel.value));

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, 'パイプライン'), h('span', { class: 'spacer' }),
      h('label', { class: 'visually-hidden', for: 'pl-pick' }, 'パイプライン'), listSel,
      h('button', { onClick: () => addEntry({ pl: Object.assign(emptyPipeline(`pipeline-${Date.now().toString(36)}`), { dirty: true }), remote: null }) }, '新規'),
      h('button', { onClick: () => {
        const rfs = streamList.filter((x) => x.codec_profile === 'royalty_free');
        const p = samplePipeline({ a: rfs[0] ? rfs[0].id : '', b: rfs[1] ? rfs[1].id : '', out: rfs[2] ? rfs[2].id : '' });
        p.name = `pinp-${Date.now().toString(36)}`;
        p.dirty = true;
        addEntry({ pl: p, remote: null });
      } }, '例を追加'),
      h('button', { onClick: io }, 'JSON'),
      h('button', { class: 'danger', onClick: remove }, '削除')),
    banner,
    h('div', { class: 'toolbar' },
      h('button', { onClick: validateRemote }, '検証'),
      h('button', { class: 'primary', onClick: save }, '保存'),
      startBtn, stopBtn,
      h('button', { onClick: reloadCurrent }, '再読み込み'),
      h('span', { class: 'spacer' }), toolbarState),
    h('div', { class: 'pl-layout' },
      h('div', { class: 'pl-palette', role: 'group', 'aria-label': '部品を置く' },
        h('div', { class: 'small muted' }, '部品を置く'),
        Object.entries(KINDS).map(([type, k]) => h('button', { onClick: () => place(type), title: type, 'data-type': type },
          h('span', { class: 'dot', style: { background: k.color } }), k.label)),
        h('p', { class: 'small muted' }, '右の丸（出力）から左の丸（入力）へドラッグでつなぎます。符号化済みの入力は「復号」、出力の前は「符号化」を挟みます。')),
      h('div', { class: 'pl-canvas' }, svg),
      h('div', { style: { display: 'grid', gap: '12px', alignContent: 'start' } }, inspector,
        h('div', { class: 'panel' }, issuesBox),
        h('section', { class: 'panel', 'aria-labelledby': 'pl-rt' }, h('h2', { id: 'pl-rt' }, '実行'), runtimeBox))),
    h('section', { class: 'panel', 'aria-labelledby': 'pl-plan' }, h('h2', { id: 'pl-plan' }, '計画'), planBox),
    h('p', { class: 'small muted' }, 'トランジションの種類: ', TRANSITIONS.map((t) => t[1]).join('・'), '。H.264/H.265 の符号化・復号にはハードウェアが要ります（無いと CAPACITY_UNAVAILABLE）。'));

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
        toast('このパイプラインは削除されました', 'warn');
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
