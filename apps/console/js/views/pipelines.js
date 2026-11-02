// パイプラインの編集（REQ-005）: SVG 上で部品を置いて線でつなぐ。右の検査器で設定する。
//
// API（/pipelines）はまだノードに無いので、下書きはこのブラウザ（localStorage）に置く。
// API がつながれば「保存」「検証」「反映」が api/pipelines.js 経由でそのまま動く。
//
// 操作: 左の部品を押すと置く。部品はドラッグで動かす。右の丸（出力）から左の丸（入力）へドラッグでつなぐ。
// キーボード: Tab で部品を選び、矢印で動かす（Shift で大きく）、Delete で消す。つなぐのは検査器からもできる。

import { h, s, replace, field, select, debounce } from '../lib/dom.js';
import { modal, confirmDialog, toast, reportError } from '../lib/ui.js';
import { pipelines as api, probe, NotAvailableError } from '../api/pipelines.js';
import { streams, listAll } from '../api/resources.js';
import {
  KINDS, PORT_LABEL, RESOURCES, NODE_W, NODE_H, emptyPipeline, addNode, removeNode, connect, validate,
  toDefinition, fromDefinition, samplePipeline, portType, compatible,
} from '../pipeline/model.js';

const STORE = 'senca.console.pipelines.v1';

function loadDrafts() {
  try {
    const v = JSON.parse(localStorage.getItem(STORE) || '[]');
    return Array.isArray(v) ? v : [];
  } catch {
    return [];
  }
}

function saveDrafts(list) {
  try { localStorage.setItem(STORE, JSON.stringify(list)); } catch { /* 容量切れなど */ }
}

function portPos(n, dir, i) {
  const ports = KINDS[n.kind][dir];
  const step = NODE_H / (ports.length + 1);
  return { x: n.x + (dir === 'out' ? NODE_W : 0), y: n.y + step * (i + 1) };
}

function edgePath(a, b) {
  const dx = Math.max(40, Math.abs(b.x - a.x) / 2);
  return `M${a.x},${a.y} C${a.x + dx},${a.y} ${b.x - dx},${b.y} ${b.x},${b.y}`;
}

export async function render(root) {
  let drafts = loadDrafts();
  let pl = drafts[0] ? drafts[0] : samplePipeline();
  if (!drafts.length) drafts = [pl];
  let sel = null; // {type:'node'|'edge', id}
  let issues = [];
  let available = null;
  let streamList = [];
  let focusNode = null;

  const banner = h('div');
  const listSel = h('select', { 'aria-label': 'パイプラインを選ぶ' });
  const svg = s('svg', { role: 'application', 'aria-label': 'パイプラインのグラフ', viewBox: '0 0 1400 700', width: 1400, height: 700 });
  const inspector = h('div', { class: 'panel', style: { marginBottom: '0' } });
  const issuesBox = h('div');

  const flush = () => {
    pl.updated_at = new Date().toISOString();
    drafts = drafts.map((d) => (d.id === pl.id ? pl : d));
    if (!drafts.some((d) => d.id === pl.id)) drafts.unshift(pl);
    saveDrafts(drafts);
  };
  const persist = debounce(flush, 300);

  const changed = () => {
    issues = validate(pl);
    persist();
    draw();
    renderIssues();
  };

  const renderList = () => {
    replace(listSel, drafts.map((d) => h('option', { value: d.id, selected: d.id === pl.id }, d.name + (d.remoteId ? '' : '（下書き）'))));
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

  const renderBanner = () => {
    replace(banner, available === false
      ? h('div', { class: 'alert warn offline-banner', role: 'status' },
        h('strong', null, 'このノードでは使えません'),
        h('span', null, 'パイプラインの API（POST /pipelines・validate・commit）はまだ実装されていません。編集と検査はこのブラウザの中だけで動き、下書きは localStorage に残ります。'))
      : available === true ? h('div', { class: 'alert ok' }, 'パイプラインの API につながっています。') : null);
  };

  // ---------------------------------------------------------------- 描画
  const toSvg = (ev) => {
    const pt = svg.createSVGPoint();
    pt.x = ev.clientX;
    pt.y = ev.clientY;
    const m = svg.getScreenCTM();
    return m ? pt.matrixTransform(m.inverse()) : { x: ev.clientX, y: ev.clientY };
  };

  let wire = null; // ドラッグ中の線

  const draw = () => {
    const bad = new Set(issues.filter((i) => i.level === 'error' && i.node).map((i) => i.node));
    const badEdges = new Set(issues.filter((i) => i.edge).map((i) => i.edge));
    const grid = s('g', { class: 'pl-grid', 'aria-hidden': 'true' });
    for (let x = 0; x <= 1400; x += 40) grid.appendChild(s('line', { x1: x, y1: 0, x2: x, y2: 700 }));
    for (let y = 0; y <= 700; y += 40) grid.appendChild(s('line', { x1: 0, y1: y, x2: 1400, y2: y }));
    const bg = s('rect', { x: 0, y: 0, width: 1400, height: 700, fill: 'transparent' });
    bg.addEventListener('pointerdown', () => { sel = null; draw(); renderInspector(); });
    const edges = s('g');
    for (const e of pl.edges) {
      const a = pl.nodes.find((n) => n.id === e.from.node);
      const b = pl.nodes.find((n) => n.id === e.to.node);
      if (!a || !b) continue;
      const d = edgePath(portPos(a, 'out', e.from.port), portPos(b, 'in', e.to.port));
      const isSel = sel && sel.type === 'edge' && sel.id === e.id;
      const hit = s('path', { d, class: 'pl-edge-hit' });
      const path = s('path', { d, class: ['pl-edge', isSel && 'selected', badEdges.has(e.id) && 'invalid'] },
        s('title', null, `${a.label} → ${b.label}（${PORT_LABEL[portType(pl, e.from, 'out')]}）`));
      const pick = (ev) => { ev.stopPropagation(); sel = { type: 'edge', id: e.id }; draw(); renderInspector(); };
      hit.addEventListener('pointerdown', pick);
      path.addEventListener('pointerdown', pick);
      edges.append(hit, path);
    }
    const nodes = s('g');
    for (const n of pl.nodes) {
      const k = KINDS[n.kind];
      const isSel = sel && sel.type === 'node' && sel.id === n.id;
      const typeLabel = (k.types.find((t) => t[0] === n.type) || [n.type, n.type])[1];
      const g = s('g', {
        class: ['pl-node', isSel && 'selected', bad.has(n.id) && 'invalid'], transform: `translate(${n.x},${n.y})`, tabindex: '0',
        role: 'button', 'aria-label': `${k.label}「${n.label}」${typeLabel}。矢印で移動、Delete で削除`, 'data-node': n.id,
      },
      s('rect', { class: 'body', width: NODE_W, height: NODE_H, rx: 8 }),
      s('rect', { class: 'stripe', width: 6, height: NODE_H, rx: 3, fill: k.color }),
      s('text', { x: 14, y: 22 }, n.label.length > 20 ? `${n.label.slice(0, 19)}…` : n.label),
      s('text', { x: 14, y: 42, class: 'kind' }, `${k.label}・${typeLabel}`.slice(0, 24)),
      k.external ? s('text', { x: NODE_W - 14, y: 16, class: 'kind', 'text-anchor': 'end' }, '拡張') : null);
      // ドラッグで移動
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
          const p = toSvg(e2);
          const nx = Math.max(0, Math.min(1400 - NODE_W, Math.round((ox + p.x - start.x) / 10) * 10));
          const ny = Math.max(0, Math.min(700 - NODE_H, Math.round((oy + p.y - start.y) / 10) * 10));
          if (nx !== n.x || ny !== n.y) { n.x = nx; n.y = ny; moved = true; draw(); }
        };
        const up = () => {
          window.removeEventListener('pointermove', move);
          window.removeEventListener('pointerup', up);
          if (moved) persist();
          renderInspector();
          focusNode = n.id;
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
          n.x = Math.max(0, Math.min(1400 - NODE_W, n.x + moves[ev.key][0]));
          n.y = Math.max(0, Math.min(700 - NODE_H, n.y + moves[ev.key][1]));
          sel = { type: 'node', id: n.id };
          focusNode = n.id;
          persist();
          draw();
        } else if (ev.key === 'Delete' || ev.key === 'Backspace') {
          ev.preventDefault();
          removeNode(pl, n.id);
          sel = null;
          changed();
          renderInspector();
          svg.focus();
        } else if (ev.key === 'Enter' || ev.key === ' ') {
          ev.preventDefault();
          sel = { type: 'node', id: n.id };
          focusNode = n.id;
          draw();
          renderInspector();
        }
      });
      g.addEventListener('focus', () => {
        if (!sel || sel.id !== n.id) { sel = { type: 'node', id: n.id }; focusNode = n.id; renderInspector(); draw(); }
      });
      // ポート
      k.in.forEach((t, i) => {
        const p = portPos({ ...n, x: 0, y: 0 }, 'in', i);
        const c = s('circle', { class: ['pl-port', wire && compatible(wire.type, t) && 'hot'], cx: p.x, cy: p.y, r: 7, 'data-port-in': `${n.id}:${i}` },
          s('title', null, `入力 ${i + 1}: ${PORT_LABEL[t]}`));
        g.appendChild(c);
      });
      k.out.forEach((t, i) => {
        const p = portPos({ ...n, x: 0, y: 0 }, 'out', i);
        const c = s('circle', { class: 'pl-port', cx: p.x, cy: p.y, r: 7 }, s('title', null, `出力 ${i + 1}: ${PORT_LABEL[t]}（ドラッグでつなぐ）`));
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
              const r = connect(pl, w.from, { node: nid, port: Number(pi) });
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
  const renderInspector = () => {
    if (sel && sel.type === 'node') {
      const n = pl.nodes.find((m) => m.id === sel.id);
      if (!n) { sel = null; renderInspector(); return; }
      const k = KINDS[n.kind];
      const label = h('input', { type: 'text', value: n.label, maxlength: 64 });
      label.addEventListener('input', () => { n.label = label.value; changed(); });
      const type = select(k.types, n.type);
      type.addEventListener('change', () => { n.type = type.value; changed(); renderInspector(); });
      const res = select(RESOURCES, n.resources);
      res.addEventListener('change', () => { n.resources = res.value; changed(); });
      const params = k.params.filter((p) => !p.when || p.when(n.type)).map((p) => {
        let input;
        if (p.type === 'select') input = select(p.options.map((o) => [o, o]), n.params[p.key] ?? p.def ?? p.options[0]);
        else if (p.type === 'stream') input = select([['', '（選ばない）'], ...streamList.map((x) => [x.id, x.name])], n.params[p.key] || '');
        else input = h('input', { type: p.type === 'number' ? 'number' : 'text', value: n.params[p.key] ?? '', min: p.min });
        input.addEventListener('change', () => {
          n.params[p.key] = p.type === 'number' ? (input.value === '' ? null : Number(input.value)) : input.value;
          changed();
        });
        return field(p.label + (p.required ? '（必須）' : ''), input);
      });
      // キーボードでつなぐ
      const targets = [];
      k.out.forEach((ot, oi) => {
        for (const m of pl.nodes) {
          if (m.id === n.id) continue;
          KINDS[m.kind].in.forEach((it, ii) => {
            if (compatible(ot, it) && !pl.edges.some((e) => e.to.node === m.id && e.to.port === ii)) {
              targets.push([`${oi}|${m.id}|${ii}`, `出力 ${oi + 1}（${PORT_LABEL[ot]}）→ ${m.label} の入力 ${ii + 1}`]);
            }
          });
        }
      });
      const target = select(targets.length ? targets : [['', 'つなげる先がありません']], '');
      const linkBtn = h('button', { class: 'small', disabled: !targets.length, onClick: () => {
        const [oi, mid, ii] = target.value.split('|');
        const r = connect(pl, { node: n.id, port: Number(oi) }, { node: mid, port: Number(ii) });
        if (!r.ok) toast(r.reason, 'warn');
        changed();
        renderInspector();
      } }, 'つなぐ');
      replace(inspector,
        h('h2', null, h('span', { class: 'dot', style: { background: k.color } }), k.label),
        h('div', { class: 'form' },
          field('名前', label), field('種類', type), ...params,
          field('必要な資源', res, '配置（どのノードで動かすか）は制御面が決めます'),
          k.external ? h('div', { class: 'alert info small' }, '外部処理は利用者の拡張です。隔離したコンテナで動き、期限を過ぎたフレームは捨てます。') : null,
          k.out.length ? h('div', { class: 'field' }, h('label', { for: target.id || (target.id = `tgt-${n.id}`) }, 'つなぐ（キーボード）'), h('div', { class: 'inline' }, target, linkBtn)) : null,
          h('div', { class: 'small muted' }, `入力: ${k.in.map((t) => PORT_LABEL[t]).join('・') || 'なし'} / 出力: ${k.out.map((t) => PORT_LABEL[t]).join('・') || 'なし'}`),
          h('div', { class: 'actions' }, h('button', { class: 'danger', onClick: () => { removeNode(pl, n.id); sel = null; changed(); renderInspector(); } }, 'この部品を削除'))));
      return;
    }
    if (sel && sel.type === 'edge') {
      const e = pl.edges.find((x) => x.id === sel.id);
      if (!e) { sel = null; renderInspector(); return; }
      const a = pl.nodes.find((n) => n.id === e.from.node);
      const b = pl.nodes.find((n) => n.id === e.to.node);
      replace(inspector, h('h2', null, '接続'),
        h('p', null, `${a.label}（出力 ${e.from.port + 1}）→ ${b.label}（入力 ${e.to.port + 1}）`),
        h('p', { class: 'muted small' }, `種類: ${PORT_LABEL[portType(pl, e.from, 'out')]}`),
        h('button', { class: 'danger', onClick: () => { pl.edges = pl.edges.filter((x) => x.id !== e.id); sel = null; changed(); renderInspector(); } }, 'この接続を削除'));
      return;
    }
    const name = h('input', { type: 'text', value: pl.name, maxlength: 64 });
    name.addEventListener('input', () => { pl.name = name.value; changed(); renderList(); });
    const red = select([['none', 'なし（1 系統）'], ['1+1', '二重化（1+1、別の障害ドメインで 2 系統）']], pl.redundancy);
    red.addEventListener('change', () => { pl.redundancy = red.value; changed(); });
    replace(inspector, h('h2', null, 'パイプライン'),
      h('div', { class: 'form' },
        field('名前', name),
        field('冗長化', red, '1+1 は primary が落ちると secondary に切り替えます。資源は約 2 倍です。'),
        h('div', { class: 'small muted' }, `部品 ${pl.nodes.length} 個・接続 ${pl.edges.length} 本`),
        pl.remoteId ? h('div', { class: 'small' }, 'API の ID: ', h('code', null, pl.remoteId)) : h('div', { class: 'small muted' }, 'まだノードに保存していません（下書き）'),
        h('p', { class: 'small muted' }, '部品を選ぶと、ここで設定できます。')));
  };

  const renderIssues = () => {
    const errs = issues.filter((i) => i.level === 'error').length;
    replace(issuesBox,
      h('h2', null, '検査', h('span', { class: ['badge', errs ? 'danger' : 'ok'] }, errs ? `エラー ${errs}` : 'エラーなし')),
      issues.length
        ? h('ul', { class: 'pl-issues' }, issues.map((i) => h('li', { class: i.level }, i.level === 'error' ? '✕ ' : '! ', i.text)))
        : h('p', { class: 'muted' }, '問題はありません。'));
  };

  // ---------------------------------------------------------------- 操作
  const place = (kind) => {
    const n = addNode(pl, kind, 40 + (pl.nodes.length % 6) * 200, 40 + (Math.floor(pl.nodes.length / 6) % 5) * 110);
    sel = { type: 'node', id: n.id };
    focusNode = n.id;
    changed();
    renderInspector();
  };

  const switchTo = (id) => {
    const d = drafts.find((x) => x.id === id);
    if (!d) return;
    pl = d;
    sel = null;
    changed();
    renderInspector();
    renderList();
  };

  const saveRemote = async () => {
    issues = validate(pl);
    renderIssues();
    if (issues.some((i) => i.level === 'error')) {
      toast('検査のエラーを直してから保存してください', 'warn');
      return;
    }
    try {
      const def = toDefinition(pl);
      const r = pl.remoteId ? await api.update(pl.remoteId, def, pl.etag) : await api.create(def);
      pl.remoteId = r.data && r.data.id ? r.data.id : pl.remoteId;
      pl.etag = r.etag;
      persist();
      renderList();
      toast('ノードに保存しました', 'ok');
    } catch (e) {
      if (!offline(e)) reportError(e, '保存');
      persist();
    }
  };

  const validateRemote = async () => {
    issues = validate(pl);
    renderIssues();
    draw();
    if (!pl.remoteId) {
      toast(issues.some((i) => i.level === 'error') ? 'この画面の検査でエラーがあります' : 'この画面の検査に通りました（ノードの検証は保存後）', issues.some((i) => i.level === 'error') ? 'warn' : 'ok');
      return;
    }
    try {
      await api.validate(pl.remoteId);
      toast('ノードの検証に通りました', 'ok');
    } catch (e) {
      if (!offline(e)) reportError(e, '検証');
    }
  };

  const commit = async () => {
    if (!pl.remoteId) {
      try {
        await api.list();
      } catch (e) {
        if (offline(e)) return;
      }
      toast('先にノードへ保存してください', 'warn');
      return;
    }
    if (!(await confirmDialog('反映（commit）', `「${pl.name}」を新しい世代として反映します。失敗したら今の世代のまま続けます。`, { ok: '反映', danger: false }))) return;
    try {
      await api.commit(pl.remoteId, pl.etag);
      toast('反映しました', 'ok');
    } catch (e) {
      if (!offline(e)) reportError(e, '反映');
    }
  };

  const io = async () => {
    const ta = h('textarea', { rows: 16, 'aria-label': 'パイプラインの定義（JSON）', spellcheck: 'false' }, JSON.stringify(toDefinition(pl), null, 2));
    const v = await modal('定義（JSON）', () => [h('p', { class: 'small muted' }, 'API に送る形です。貼り付けて取り込めます。'), ta], {
      wide: true,
      actions: (close) => [h('button', { onClick: () => close(null) }, '閉じる'), h('button', { class: 'primary', onClick: () => close(ta.value) }, '取り込む')],
    });
    if (!v) return;
    try {
      const next = fromDefinition(JSON.parse(v));
      drafts.unshift(next);
      pl = next;
      sel = null;
      changed();
      renderList();
      renderInspector();
      toast('取り込みました', 'ok');
    } catch (e) {
      toast(`取り込めません: ${e.message}`, 'error');
    }
  };

  listSel.addEventListener('change', () => switchTo(listSel.value));

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, 'パイプライン'), h('span', { class: 'spacer' }),
      h('label', { class: 'visually-hidden', for: 'pl-pick' }, 'パイプライン'), Object.assign(listSel, { id: 'pl-pick' }),
      h('button', { onClick: () => { const n = emptyPipeline(); drafts.unshift(n); switchTo(n.id); } }, '新規'),
      h('button', { onClick: () => { const n = samplePipeline(); drafts.unshift(n); switchTo(n.id); } }, '例を追加'),
      h('button', { class: 'danger', onClick: async () => {
        if (!(await confirmDialog('下書きを削除', `「${pl.name}」をこのブラウザから削除します。`, { ok: '削除' }))) return;
        drafts = drafts.filter((d) => d.id !== pl.id);
        if (!drafts.length) drafts = [emptyPipeline()];
        saveDrafts(drafts);
        switchTo(drafts[0].id);
      } }, '削除')),
    banner,
    h('div', { class: 'toolbar' },
      h('button', { onClick: validateRemote }, '検証'),
      h('button', { onClick: saveRemote }, 'ノードに保存'),
      h('button', { class: 'primary', onClick: commit }, '反映（commit）'),
      h('span', { class: 'spacer' }),
      h('button', { onClick: io }, 'JSON')),
    h('div', { class: 'pl-layout' },
      h('div', { class: 'pl-palette', role: 'group', 'aria-label': '部品を置く' },
        h('div', { class: 'small muted' }, '部品を置く'),
        Object.entries(KINDS).map(([kind, k]) => h('button', { onClick: () => place(kind), title: `${k.label}（入力 ${k.in.length}・出力 ${k.out.length}）` },
          h('span', { class: 'dot', style: { background: k.color } }), k.label, k.external ? h('span', { class: 'tag small muted' }, ' 拡張') : null)),
        h('p', { class: 'small muted' }, '右の丸（出力）から左の丸（入力）へドラッグでつなぎます。矢印キーで動かし、Delete で消せます。')),
      h('div', { class: 'pl-canvas' }, svg),
      h('div', { style: { display: 'grid', gap: '12px', alignContent: 'start' } }, inspector, h('div', { class: 'panel' }, issuesBox))));

  renderList();
  changed();
  renderInspector();

  try { streamList = await listAll(streams.list); } catch { /* 選べないだけ */ }
  try {
    available = await probe('pipelines');
  } catch {
    available = null;
  }
  renderBanner();
  return () => {
    persist.cancel();
    flush();
  };
}
