// パイプラインの編集の画面（#/pipelines/{id}、下書きは #/pipelines/{key}）。
//
//   ┌ 見出し（名前・状態）
//   ├ 実行の帯: 状態の説明 ……… 元に戻す・やり直す｜検証・保存・開始/停止・その他
//   ├ 部品の一覧 ｜ グラフ（canvas.js）｜ 検査器（inspector.js）
//   └ タブ: 問題・計画・部品の一覧（文字で読むグラフ）・実行の様子
//
// 保存 = POST /pipelines（下書き）か PATCH（If-Match）。保存済みのものは、変えると 1.2 秒後にノードで検証し（POST /validate に
// 編集中のグラフ）、計画と問題を出し直す。下書き・位置・未保存の変更はこのブラウザに残す（store.js）。

import { h, replace, debounce } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { toast, reportError, confirmDialog, modal, errorBox } from '../lib/ui.js';
import { button, iconButton, pageHeader, alert, badge, attachMenu, attachPopover, emptyState, kvList, skeleton } from '../ui/index.js';
import { pipelines as api, NotAvailableError, issuesOf, runtimeOf } from '../api/pipelines.js';
import { streams as streamsApi, listAll } from '../api/resources.js';
import { fmtTime, fmtRelative, fmtClock } from '../lib/format.js';
import { rememberRecent } from '../lib/recent.js';
import { isTyping, MOD } from '../shortcuts.js';
import { invalidatePalette } from '../palette.js';
import {
  KINDS, PALETTE_GROUPS, addNode, removeNode, connect, validate, toGraph, fromGraph, positionsOf, autoLayout,
  serialize, restore, copyNodes, pasteNodes, nodeById, topoOrder, inPorts, outPorts, edgeInto, referencedStreams, emptyPipeline, refOf, composeNodes,
} from './model.js';
import { createCanvas } from './canvas.js';
import { renderInspector } from './inspector.js';
import { planView, issueList } from './plan.js';
import { editScenes } from './scenes.js';
import { drafts, layouts, edits, clipboard } from './store.js';
import { observedBadge } from './status.js';

const HISTORY_MAX = 120;

/**
 * @param {HTMLElement} root
 * @param {{id: string, ctx: object}} o
 */
export async function renderEditor(root, { id, ctx }) {
  root.classList.add('full');
  replace(root, skeleton({ kind: 'block' }));
  let streamList = [];
  let remote = null; // GET /pipelines/{id}
  let pl = null;
  let restoredNote = null;
  const isDraft = id.startsWith('pl_');
  try { streamList = await listAll(streamsApi.list); } catch { /* 選べないだけ */ }
  if (isDraft) {
    const d = drafts.get(id);
    if (!d) { replace(root, notFound()); return () => {}; }
    try {
      pl = fromGraph(d.graph, { name: d.name, positions: d.positions });
    } catch (e) {
      replace(root, errorBox(e, { title: t('pipelines.ed.cannotOpen') }));
      return () => {};
    }
    pl.key = d.key;
    pl.dirty = true;
  } else {
    try {
      const r = await api.get(id);
      remote = r.data;
      pl = fromGraph(remote.graph, { name: remote.name, positions: layouts.get(id) });
      pl.remoteId = remote.id;
      pl.etag = r.etag || `"${remote.generation}"`;
      const ed = edits.get(id);
      if (ed) {
        try {
          const mine = fromGraph(ed.graph, { name: ed.name, positions: ed.positions });
          if (ed.generation === remote.generation) {
            Object.assign(pl, { name: mine.name, options: mine.options, nodes: mine.nodes, edges: mine.edges, dirty: true });
            restoredNote = { kind: 'restored', at: ed.at };
          } else restoredNote = { kind: 'conflict', at: ed.at, mine };
        } catch { edits.remove(id); }
      }
    } catch (e) {
      if (e.status === 404) { replace(root, notFound()); return () => {}; }
      if (e instanceof NotAvailableError) { replace(root, alert('warn', t('pipelines.offlineBody'), { title: t('pipelines.offlineTitle') })); return () => {}; }
      replace(root, errorBox(e, { title: t('pipelines.ed.cannotOpen'), actions: [button(t('common.retry'), { icon: 'refresh', onClick: () => ctx.navigate(location.hash) })] }));
      return () => {};
    }
  }
  rememberRecent({ kind: 'pipeline', id: pl.remoteId || pl.key, label: pl.name, href: `#/pipelines/${encodeURIComponent(pl.remoteId || pl.key)}` });

  let dead = false;
  let localIssues = [];
  let remoteIssues = [];
  let plan = remote && remote.plan ? remote.plan : null;
  let checkState = 'idle'; // idle・checking・ok・bad・error
  let checkedAt = null;
  let savedAt = null;
  const streamName = (sid) => (streamList.find((x) => x.id === sid) || {}).name || (sid ? sid.slice(0, 8) : t('pipelines.noStream'));
  const royaltyFree = () => referencedStreams(pl).some((sid) => (streamList.find((x) => x.id === sid) || {}).codec_profile === 'royalty_free');

  // ---------------------------------------------------------------- 元に戻す・やり直す
  const hist = { past: [], future: [], last: serialize(pl), key: null, at: 0 };
  const commit = (coalesce) => {
    const now = serialize(pl);
    if (now === hist.last) return;
    const same = coalesce && hist.key === coalesce && Date.now() - hist.at < 1500;
    if (!same) {
      hist.past.push(hist.last);
      if (hist.past.length > HISTORY_MAX) hist.past.shift();
    }
    hist.last = now;
    hist.future = [];
    hist.key = coalesce || null;
    hist.at = Date.now();
  };
  const travel = (from, to) => {
    if (!from.length) return;
    to.push(hist.last);
    hist.last = from.pop();
    hist.key = null;
    const keepSel = [...canvas.sel.nodes];
    restore(pl, hist.last);
    canvas.sel.nodes = new Set(keepSel.filter((x) => nodeById(pl, x)));
    pl.dirty = true;
    refresh({ rerender: true });
  };
  const undo = () => { travel(hist.past, hist.future); announce(t('pipelines.ed.undone')); };
  const redo = () => { travel(hist.future, hist.past); announce(t('pipelines.ed.redone')); };

  // ---------------------------------------------------------------- 残す
  const persistNow = () => {
    if (dead && !pl) return;
    if (pl.remoteId) {
      layouts.put(pl.remoteId, positionsOf(pl));
      if (pl.dirty) edits.put(pl.remoteId, { name: pl.name, graph: toGraph(pl), positions: positionsOf(pl), generation: remote ? remote.generation : null });
      else edits.remove(pl.remoteId);
    } else {
      drafts.put({ key: pl.key, name: pl.name, graph: toGraph(pl), positions: positionsOf(pl) });
      savedAt = Date.now();
    }
  };
  const persist = debounce(persistNow, 400);

  // ---------------------------------------------------------------- ノードで検証（保存済みのものだけ）
  let checkSeq = 0;
  const checkRemote = async ({ quiet = true } = {}) => {
    if (!pl.remoteId) return;
    const seq = ++checkSeq;
    checkState = 'checking';
    renderRunbar();
    try {
      const r = await api.validate(pl.remoteId, toGraph(pl));
      if (seq !== checkSeq || dead) return;
      plan = r.data.plan;
      remoteIssues = [];
      checkState = 'ok';
      if (!quiet) toast(t('pipelines.validated'), 'ok');
    } catch (e) {
      if (seq !== checkSeq || dead) return;
      if (e.details && (e.details.issues || e.details.reason)) {
        remoteIssues = issuesOf(e).map((i) => ({ level: 'error', remote: true, code: i.code, node: i.node ? (pl.nodes.find((n) => n.name === i.node) || {}).id || null : null, text: i.message, nodeName: i.node }));
        plan = null;
        checkState = 'bad';
        if (!quiet) { toast(t('pipelines.validateIssues', { n: remoteIssues.length }), 'warn', 7000); selectTab('issues', true); }
      } else {
        checkState = 'error';
        if (!quiet) reportError(e, t('pipelines.validate'), { retry: () => checkRemote({ quiet: false }) });
      }
    }
    checkedAt = Date.now();
    refreshIssues();
    renderPlan();
    canvas.draw();
    renderRunbar();
  };
  const scheduleCheck = debounce(() => { if (!localIssues.some((i) => i.level === 'error')) checkRemote(); }, 1200);

  // ---------------------------------------------------------------- 変更の受け口
  const change = (what, opt = {}) => {
    const structural = opt.structural !== false;
    if (structural) {
      pl.dirty = true;
      checkState = pl.remoteId ? 'stale' : checkState;
    }
    commit(opt.coalesce);
    refresh({ rerender: !!opt.rerender || ['connect', 'disconnect', 'add', 'remove', 'paste', 'layout'].includes(what) });
    if (opt.renamed) renderHead();
    if (structural && pl.remoteId) scheduleCheck();
  };

  const refresh = ({ rerender = false } = {}) => {
    localIssues = validate(pl, { royaltyFree: royaltyFree() });
    persist();
    canvas.draw();
    if (rerender) inspect();
    refreshIssues();
    renderRunbar();
    renderOutline();
  };

  // ---------------------------------------------------------------- 部品の操作
  const add = (type, at, link) => {
    const pos = at || canvas.freeSpot();
    const n = addNode(pl, type, pos.x, pos.y);
    if (link && link.from) {
      const port = inPorts(n).findIndex((_, i) => connect(pl, link.from, { node: n.id, port: i }).ok);
      if (port < 0) toast(t('pipelines.ed.couldNotLink'), 'warn');
    } else if (link && link.to) {
      const ok = outPorts(n).some((_, i) => connect(pl, { node: n.id, port: i }, link.to, { replace: true }).ok);
      if (!ok) toast(t('pipelines.ed.couldNotLink'), 'warn');
    }
    canvas.sel.nodes = new Set([n.id]);
    canvas.sel.edge = null;
    change('add', { rerender: true });
    canvas.focusNode(n.id);
    announce(t('pipelines.ed.added', { kind: KINDS[type].label, node: n.name }));
    return n;
  };

  const removeIds = (ids) => {
    if (!ids.length) return;
    const names = ids.map((x) => (nodeById(pl, x) || {}).name).filter(Boolean);
    for (const x of ids) removeNode(pl, x);
    canvas.sel.nodes.clear();
    change('remove', { rerender: true });
    svgFocus();
    toast(t('pipelines.ed.removed', { list: names.join(', ') }), 'info', { action: { label: t('pipelines.ed.undo'), onClick: undo } });
  };

  const copyIds = (ids) => {
    if (!ids.length) return false;
    clipboard.put(copyNodes(pl, ids));
    announce(t('pipelines.ed.copied', { n: ids.length }));
    return true;
  };
  const paste = (at) => {
    const clip = clipboard.get();
    if (!clip) { toast(t('pipelines.ed.nothingToPaste'), 'info'); return; }
    const minX = Math.min(...clip.nodes.map((n) => n.x));
    const minY = Math.min(...clip.nodes.map((n) => n.y));
    const spot = at || canvas.freeSpot();
    const ids = pasteNodes(pl, clip, at || !clip.nodes.every((c) => pl.nodes.some((m) => m.x === c.x && m.y === c.y)) ? { dx: spot.x - minX, dy: spot.y - minY } : { dx: 40, dy: 40 });
    canvas.sel.nodes = new Set(ids);
    canvas.sel.edge = null;
    change('paste', { rerender: true });
    announce(t('pipelines.ed.pasted', { n: ids.length }));
  };
  const duplicate = (ids) => {
    if (!ids.length) return;
    const clip = copyNodes(pl, ids);
    const made = pasteNodes(pl, clip, { dx: 40, dy: 40 });
    canvas.sel.nodes = new Set(made);
    change('paste', { rerender: true });
  };

  const doAutoLayout = () => {
    autoLayout(pl);
    change('layout', { structural: false });
    requestAnimationFrame(() => canvas.fit());
    announce(t('pipelines.ed.laidOut'));
  };

  const svgFocus = () => { const s = canvas.el.querySelector('svg'); if (s) s.focus({ preventScroll: true }); };

  const openNode = (nid) => {
    canvas.sel.nodes = new Set([nid]);
    canvas.sel.edge = null;
    canvas.draw();
    inspect();
    const first = inspectorBox.querySelector('input, select, textarea, button');
    if (first) first.focus();
    if (window.matchMedia('(max-width: 1100px)').matches) inspectorBox.scrollIntoView({ behavior: 'smooth', block: 'start' });
  };

  const focusNode = (nid) => {
    if (!nodeById(pl, nid)) return;
    canvas.sel.nodes = new Set([nid]);
    canvas.sel.edge = null;
    canvas.draw();
    canvas.focusNode(nid);
    inspect();
    if (window.matchMedia('(max-width: 1100px)').matches) canvas.el.scrollIntoView({ behavior: 'smooth', block: 'center' });
  };

  // ---------------------------------------------------------------- 画面の部品
  const live = h('div', { class: 'visually-hidden', role: 'status', 'aria-live': 'polite' });
  const announce = (msg) => { live.textContent = ''; setTimeout(() => { live.textContent = msg; }, 30); };
  const head = h('div');
  const banner = h('div', { class: 'pl-banners' });
  const inspectorBox = h('section', { class: 'pl-inspector panel', 'aria-label': t('pipelines.inspector'), 'data-testid': 'inspector' });
  const issuesBox = h('div');
  const planBox = h('div');
  const outlineBox = h('div');
  const runtimeBox = h('div');

  const marks = () => {
    const nodes = new Map();
    for (const i of [...localIssues, ...remoteIssues]) {
      if (!i.node) continue;
      if (i.level === 'error' || !nodes.has(i.node)) nodes.set(i.node, i.level === 'error' ? 'error' : 'warn');
    }
    return { nodes, edges: new Set(localIssues.filter((i) => i.edge).map((i) => i.edge)) };
  };
  let runtimeMap = null;
  const summary = (n) => {
    if (n.type === 'source') return `${n.p.stream ? streamName(n.p.stream) : t('pipelines.noStream')}${n.p['video.codec'] ? ` · ${n.p['video.codec'].toUpperCase()}` : ''}`;
    if (n.type === 'sink') return n.p.stream ? `→ ${streamName(n.p.stream)}` : t('pipelines.noStream');
    if (n.type === 'encode') return `${(n.p.codec || '?').toUpperCase()} · ${n.p.bitrate_kbps || '?'} kbps`;
    if (n.type === 'scale') return `${n.p.width || '?'}×${n.p.height || '?'}`;
    if (n.type === 'crop') return `${n.p.width || '?'}×${n.p.height || '?'} @ ${n.p.x || 0},${n.p.y || 0}`;
    if (n.type === 'compose') return t('pipelines.sum.compose', { n: Object.keys(n.p.scenes || {}).length, w: n.p['canvas.width'] || '?', h: n.p['canvas.height'] || '?' });
    if (n.type === 'audio_mix') return t('pipelines.sum.mix', { n: (n.ins || []).length });
    if (n.type === 'board') return n.p['params.mode'] === 'blackboard' ? t('pipelines.opt.blackboard') : t('pipelines.opt.whiteboard');
    if (n.type === 'decode') { const e = edgeInto(pl, n.id, 0); return e ? t('pipelines.sum.from', { ref: refOf(pl, e.from) }) : KINDS.decode.label; }
    return KINDS[n.type].label;
  };

  const nodeMenu = (nid) => {
    const ids = canvas.sel.nodes.size ? [...canvas.sel.nodes] : [nid];
    return [
      { label: t('pipelines.ed.openSettings'), icon: 'settings', kbd: 'Enter', onSelect: () => openNode(nid) },
      { label: t('pipelines.ed.copy'), icon: 'copy', kbd: `${MOD}+C`, onSelect: () => copyIds(ids) },
      { label: t('pipelines.insp.duplicate'), icon: 'copy', kbd: `${MOD}+D`, onSelect: () => duplicate(ids) },
      { label: t('pipelines.ed.disconnectAll'), icon: 'unlink', onSelect: () => { pl.edges = pl.edges.filter((e) => !ids.includes(e.from.node) && !ids.includes(e.to.node)); change('disconnect', { rerender: true }); } },
      { separator: true },
      { label: t('common.delete'), icon: 'trash', kbd: 'Del', danger: true, onSelect: () => removeIds(ids) },
    ];
  };
  const canvasMenu = (at) => [
    { heading: t('pipelines.ed.addHere') },
    ...PALETTE_GROUPS.flatMap(([, types]) => types).map((type) => ({ label: KINDS[type].label, icon: KINDS[type].icon, cat: KINDS[type].cat, onSelect: () => add(type, { x: Math.round(at.x / 20) * 20, y: Math.round(at.y / 20) * 20 }) })),
    { separator: true },
    { label: t('pipelines.ed.paste'), icon: 'clipboard', kbd: `${MOD}+V`, onSelect: () => paste({ x: Math.round(at.x / 20) * 20, y: Math.round(at.y / 20) * 20 }) },
  ];

  const canvas = createCanvas({
    model: () => pl,
    summary,
    marks,
    runtime: () => runtimeMap,
    onSelect: () => { inspect(); renderOutline(); },
    onChange: (what, opt) => change(what, { ...opt, rerender: true }),
    onAddRequest: (type, at, link) => add(type, at, link),
    onOpenNode: openNode,
    onEmptyAction: (what) => { if (what === 'source') add('source'); else ctx.navigate('#/pipelines?new=1'); },
    onAutoLayout: doAutoLayout,
    onReject: (reason) => toast(reason, 'warn'),
    nodeMenu,
    canvasMenu,
  });

  const inspect = () => renderInspector(inspectorBox, {
    pl: () => pl,
    sel: canvas.sel,
    streamList: () => streamList,
    royaltyFree,
    issuesOf: (nid) => [...localIssues, ...remoteIssues].filter((i) => i.node === nid),
    change: (what, opt = {}) => change(what, opt),
    focusNode,
    duplicate,
    remove: removeIds,
    openScenes: async (n) => {
      const r = await editScenes(n);
      if (!r) return;
      n.p.scenes = r.scenes;
      n.p.program = r.scenes[r.program] ? r.program : Object.keys(r.scenes)[0] || '';
      n.p.preview = r.preview && r.scenes[r.preview] ? r.preview : '';
      change('scenes', { rerender: true });
    },
    refreshStreams: async () => { try { streamList = await listAll(streamsApi.list); } catch { /* 前のまま */ } },
  });

  // ---------------------------------------------------------------- 部品の一覧（左）
  const paletteFilter = h('input', { type: 'search', class: 'pl-pal-search', placeholder: t('pipelines.pal.search'), 'aria-label': t('pipelines.pal.search'), 'data-search': 'page' });
  const paletteList = h('div', { class: 'pl-pal-groups' });
  const drawPalette = () => {
    const q = paletteFilter.value.trim().toLowerCase();
    replace(paletteList, PALETTE_GROUPS.map(([g, types]) => {
      const items = types.filter((type) => !q || `${KINDS[type].label} ${type} ${KINDS[type].desc}`.toLowerCase().includes(q));
      if (!items.length) return null;
      return h('div', { class: 'pl-pal-group', role: 'group', 'aria-label': t(`pipelines.pal.${g}`) },
        h('div', { class: 'pl-pal-title' }, t(`pipelines.pal.${g}`)),
        items.map((type) => {
          const k = KINDS[type];
          const b = h('button', { type: 'button', class: ['pl-pal-item', `k-${k.cat}`], draggable: 'true', 'data-type': type, title: `${k.label} — ${k.desc}`, 'aria-label': k.label, onClick: () => add(type) },
            h('span', { class: 'pl-kind-chip sm' }, icon(k.icon)), h('span', { class: 'pl-pal-label' }, k.label), icon('plus', { size: 'sm', class: 'pl-pal-plus' }));
          b.addEventListener('dragstart', (e) => { e.dataTransfer.setData('application/x-senca-kind', type); e.dataTransfer.effectAllowed = 'copy'; });
          return b;
        }));
    }));
  };
  paletteFilter.addEventListener('input', drawPalette);
  paletteFilter.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { const first = paletteList.querySelector('.pl-pal-item'); if (first) { e.preventDefault(); first.click(); paletteFilter.value = ''; drawPalette(); } }
    if (e.key === 'Escape') { paletteFilter.value = ''; drawPalette(); svgFocus(); }
  });
  drawPalette();
  // 部品の一覧は畳める（狭い画面では最初から畳む）。畳むとアイコンだけの細い列になる
  const PAL = 'senca.console.pipeline-palette';
  let palOpen = (() => { try { const v = localStorage.getItem(PAL); return v ? v === 'open' : window.innerWidth >= 1600; } catch { return true; } })();
  const palToggle = iconButton(palOpen ? 'sidebar-close' : 'sidebar-open', palOpen ? t('pipelines.pal.collapse') : t('pipelines.pal.expand'), { size: 'sm', testid: 'palette-toggle', onClick: () => {
    palOpen = !palOpen;
    try { localStorage.setItem(PAL, palOpen ? 'open' : 'closed'); } catch { /* 残らないだけ */ }
    palette.classList.toggle('collapsed', !palOpen);
    replace(palToggle, icon(palOpen ? 'sidebar-close' : 'sidebar-open'));
    palToggle.setAttribute('aria-label', palOpen ? t('pipelines.pal.collapse') : t('pipelines.pal.expand'));
    palToggle.setAttribute('aria-expanded', String(palOpen));
    requestAnimationFrame(() => canvas.fit());
  } });
  palToggle.setAttribute('aria-expanded', String(palOpen));
  const palette = h('aside', { class: ['pl-palette', !palOpen && 'collapsed'], 'aria-label': t('pipelines.palette') },
    h('div', { class: 'pl-pal-head' }, h('div', { class: 'pl-pal-top' }, h('h2', null, t('pipelines.palette')), palToggle), paletteFilter),
    paletteList,
    h('p', { class: 'pl-pal-hint' }, t('pipelines.paletteHint')));

  // ---------------------------------------------------------------- 実行の帯
  const runbar = h('div', { class: 'pl-runbar', role: 'toolbar', 'aria-label': t('pipelines.ed.runbar'), 'data-testid': 'runbar' });
  const undoBtn = iconButton('undo', t('pipelines.ed.undo'), { onClick: undo, kbd: `${MOD}+Z`, testid: 'undo' });
  const redoBtn = iconButton('redo', t('pipelines.ed.redo'), { onClick: redo, kbd: `${MOD}+Shift+Z`, testid: 'redo' });
  const addMenuBtn = button(t('pipelines.ed.addPart'), { icon: 'plus', class: 'pl-add-mobile', testid: 'add-part' });
  attachMenu(addMenuBtn, () => PALETTE_GROUPS.flatMap(([g, types]) => [{ heading: t(`pipelines.pal.${g}`) }, ...types.map((type) => ({ label: KINDS[type].label, icon: KINDS[type].icon, onSelect: () => add(type) }))]), { align: 'start' });
  const keysBtn = iconButton('keyboard', t('pipelines.ed.keys'), { class: 'pl-keys-btn' });
  attachPopover(keysBtn, () => h('div', { class: 'pl-keys' }, h('h3', null, t('pipelines.ed.keys')), h('dl', null,
    [[`${MOD}+Z`, t('pipelines.ed.undo')], [`${MOD}+Shift+Z`, t('pipelines.ed.redo')], [`${MOD}+S`, t('common.save')], [`${MOD}+C / ${MOD}+V`, t('pipelines.ed.copyPaste')], [`${MOD}+D`, t('pipelines.insp.duplicate')],
      [`${MOD}+A`, t('pipelines.ed.selectAll')], ['Del', t('common.delete')], ['Tab', t('pipelines.ed.kNextNode')], ['← ↑ → ↓', t('pipelines.ed.kMove')], ['Enter', t('pipelines.ed.openSettings')],
      ['0 / 1 / + / −', t('pipelines.ed.kZoom')], ['L', t('pipelines.canvas.autoLayout')], ['/', t('pipelines.pal.search')], ['Esc', t('pipelines.ed.kDeselect')]]
      .flatMap(([k, v]) => [h('dt', null, h('kbd', null, k)), h('dd', null, v)]))), { label: t('pipelines.ed.keys') });

  const save = async ({ then } = {}) => {
    localIssues = validate(pl, { royaltyFree: royaltyFree() });
    refreshIssues();
    const errs = localIssues.filter((i) => i.level === 'error');
    if (errs.length) {
      toast(t('pipelines.fixBeforeSave', { n: errs.length }), 'warn');
      selectTab('issues');
      return false;
    }
    const graph = toGraph(pl);
    try {
      let r;
      if (pl.remoteId) {
        const body = { graph };
        if (!remote || remote.name !== pl.name) body.name = pl.name;
        r = await api.update(pl.remoteId, body, pl.etag);
      } else {
        r = await api.create({ name: pl.name, graph });
        drafts.remove(pl.key);
        layouts.put(r.data.id, positionsOf(pl));
        invalidatePalette();
      }
      const wasDraft = !pl.remoteId;
      pl.remoteId = r.data.id;
      pl.etag = r.etag || `"${r.data.generation}"`;
      pl.dirty = false;
      remote = r.data;
      edits.remove(pl.remoteId);
      persistNow();
      toast(wasDraft ? t('pipelines.ed.createdOnNode') : t('pipelines.saved'), 'ok');
      if (wasDraft) {
        // 下書きの経路から保存済みの経路へ（作り直さずに URL だけ変える）
        history.replaceState(null, '', `#/pipelines/${encodeURIComponent(pl.remoteId)}`);
        rememberRecent({ kind: 'pipeline', id: pl.remoteId, label: pl.name, href: `#/pipelines/${encodeURIComponent(pl.remoteId)}` });
      }
      renderHead();
      renderRunbar();
      renderRuntime();
      checkRemote();
      if (then) await then();
      return true;
    } catch (e) {
      if (e instanceof NotAvailableError) { toast(t('pipelines.offlineToast'), 'warn', 7000); return false; }
      if (e.status === 412) { await staleDialog(); return false; }
      if (e.status === 409) { toast(t('pipelines.ed.nameTaken', { name: pl.name }), 'warn', 8000); return false; }
      if (e.details && (e.details.issues || e.details.reason)) {
        remoteIssues = issuesOf(e).map((i) => ({ level: 'error', remote: true, code: i.code, node: i.node ? (pl.nodes.find((n) => n.name === i.node) || {}).id || null : null, text: i.message }));
        refreshIssues();
        canvas.draw();
        selectTab('issues');
      }
      reportError(e, t('common.save'), { retry: () => save({ then }) });
      return false;
    }
  };

  const staleDialog = async () => {
    const v = await modal(t('pipelines.ed.staleTitle'), () => [h('p', null, t('pipelines.ed.staleBody'))], {
      icon: 'warning', kind: 'warn',
      actions: (close) => [
        h('button', { type: 'button', class: 'btn', onClick: () => close('reload') }, icon('refresh'), t('pipelines.ed.staleReload')),
        h('button', { type: 'button', class: 'btn primary', onClick: () => close('overwrite') }, t('pipelines.ed.staleOverwrite')),
      ],
    });
    if (v === 'reload') await reloadFromNode();
    else if (v === 'overwrite') {
      try { const r = await api.get(pl.remoteId); pl.etag = r.etag || `"${r.data.generation}"`; remote = r.data; await save(); } catch (e) { reportError(e, t('common.save')); }
    }
  };

  const reloadFromNode = async () => {
    try {
      const r = await api.get(pl.remoteId);
      remote = r.data;
      const fresh = fromGraph(remote.graph, { name: remote.name, positions: layouts.get(pl.remoteId) });
      Object.assign(pl, { name: fresh.name, options: fresh.options, nodes: fresh.nodes, edges: fresh.edges, dirty: false, etag: r.etag || `"${remote.generation}"` });
      edits.remove(pl.remoteId);
      replace(banner);
      commit();
      refresh({ rerender: true });
      renderHead();
      renderRuntime();
      toast(t('pipelines.reloaded'), 'ok');
    } catch (e) { reportError(e, t('common.refresh')); }
  };

  const setDesired = async (want) => {
    if (want === 'running' && (pl.dirty || !pl.remoteId)) {
      // 未保存の変更があれば、保存してから開始する
      await save({ then: () => setDesired('running') });
      return;
    }
    if (want === 'stopped') {
      const outs = pl.nodes.filter((n) => n.type === 'sink' && n.p.stream).map((n) => streamName(n.p.stream));
      const ok = await confirmDialog(t('pipelines.ed.stopTitle'), t('pipelines.ed.stopBody', { name: pl.name }), {
        ok: t('pipelines.stop'), danger: false, icon: 'stop', consequences: outs.length ? [t('pipelines.ed.stopOutputs', { list: outs.join(', ') })] : [],
      });
      if (!ok) return;
    }
    try {
      const r = await api.setDesired(pl.remoteId, want, pl.etag);
      pl.etag = r.etag || `"${r.data.generation}"`;
      remote = r.data;
      renderRunbar();
      renderRuntime();
      renderHead();
      toast(want === 'running' ? t('pipelines.ed.starting') : t('pipelines.stopped'), 'ok');
      fastPoll(want);
    } catch (e) {
      if (e.status === 412) { await staleDialog(); return; }
      if (e.details && (e.details.issues || e.details.reason)) {
        remoteIssues = issuesOf(e).map((i) => ({ level: 'error', remote: true, code: i.code, node: i.node ? (pl.nodes.find((n) => n.name === i.node) || {}).id || null : null, text: i.message }));
        refreshIssues();
        selectTab('issues');
      }
      reportError(e, want === 'running' ? t('pipelines.start') : t('pipelines.stop'), { retry: () => setDesired(want) });
    }
  };

  const exportJson = async () => {
    const body = { name: pl.name, graph: toGraph(pl) };
    const ta = h('textarea', { rows: 18, 'aria-label': t('pipelines.jsonLabel'), spellcheck: 'false', 'data-testid': 'json-text' }, JSON.stringify(body, null, 2));
    const v = await modal(t('pipelines.jsonTitle'), () => [h('p', { class: 'small muted' }, t('pipelines.ed.jsonLead')), ta], {
      wide: true, icon: 'json', testid: 'json-dialog',
      actions: (close) => [
        h('button', { type: 'button', class: 'btn', onClick: () => close(null) }, t('common.close')),
        h('button', { type: 'button', class: 'btn primary', 'data-testid': 'json-apply', onClick: () => close(ta.value) }, icon('upload'), t('pipelines.ed.jsonApply')),
      ],
    });
    if (!v || v === JSON.stringify(body, null, 2)) return;
    try {
      const j = JSON.parse(v);
      const next = fromGraph(j.graph || j, { name: j.name || pl.name });
      Object.assign(pl, { name: next.name, options: next.options, nodes: next.nodes, edges: next.edges });
      canvas.sel.nodes.clear();
      change('paste', { rerender: true });
      renderHead();
      requestAnimationFrame(() => canvas.fit());
      toast(t('pipelines.ed.jsonApplied'), 'ok');
    } catch (e) {
      toast(t('pipelines.importFailed', { reason: e.message }), 'error');
    }
  };

  const duplicatePipeline = () => {
    const key = emptyPipeline().key;
    drafts.put({ key, name: `${pl.name}-copy`.slice(0, 64), graph: toGraph(pl), positions: positionsOf(pl) });
    ctx.navigate(`#/pipelines/${key}`);
    toast(t('pipelines.ed.duplicated'), 'ok');
  };

  const removePipeline = async () => {
    if (pl.remoteId) {
      const outs = pl.nodes.filter((n) => n.type === 'sink' && n.p.stream).map((n) => streamName(n.p.stream));
      if (!(await confirmDialog(t('pipelines.deleteTitle'), t('pipelines.deleteBody', { name: pl.name }), { ok: t('common.delete'), requireText: pl.name, consequences: outs.length && remote && remote.desired_state === 'running' ? [t('pipelines.ed.stopOutputs', { list: outs.join(', ') })] : [] }))) return;
      try {
        await api.remove(pl.remoteId);
        layouts.remove(pl.remoteId);
        edits.remove(pl.remoteId);
        invalidatePalette();
      } catch (e) { reportError(e, t('common.delete')); return; }
    } else {
      if (!(await confirmDialog(t('pipelines.deleteDraftTitle'), t('pipelines.deleteDraftBody', { name: pl.name }), { ok: t('common.delete') }))) return;
      drafts.remove(pl.key);
    }
    dead = true;
    toast(t('common.deleted'), 'ok');
    ctx.navigate('#/pipelines');
  };

  const moreBtn = iconButton('more', t('pipelines.ed.more'), { testid: 'pipeline-more' });
  attachMenu(moreBtn, () => [
    { label: t('pipelines.ed.json'), icon: 'json', onSelect: exportJson, testid: 'menu-json' },
    { label: t('pipelines.ed.duplicatePipeline'), icon: 'copy', onSelect: duplicatePipeline },
    pl.remoteId ? { label: t('pipelines.reload'), icon: 'refresh', onSelect: reloadFromNode } : null,
    composeNodes(toGraph(pl)).length && pl.remoteId ? { label: t('pipelines.openSwitcher'), icon: 'switcher', onSelect: () => ctx.navigate(`#/production?pipeline=${encodeURIComponent(pl.remoteId)}`) } : null,
    { separator: true },
    { label: pl.remoteId ? t('pipelines.deleteThis') : t('pipelines.deleteDraftTitle'), icon: 'trash', danger: true, onSelect: removePipeline, testid: 'menu-delete' },
  ].filter(Boolean));

  const renderRunbar = () => {
    const errs = localIssues.filter((i) => i.level === 'error').length + remoteIssues.length;
    const warns = localIssues.filter((i) => i.level === 'warn').length;
    const running = remote && remote.desired_state === 'running';
    const st = remote ? remote.observed_state : null;
    // 次にすること（primary は 1 つ）: 下書きか未保存 → 保存、保存済みで止まっている → 開始
    const needSave = !pl.remoteId || pl.dirty;
    let msg;
    let msgIcon = 'info';
    let tone = '';
    if (!pl.remoteId) { msg = t('pipelines.ed.stDraft'); msgIcon = 'edit'; }
    else if (pl.dirty) { msg = t('pipelines.ed.stDirty'); msgIcon = 'edit'; tone = 'warn'; }
    else if (st === 'error') { msg = remote.last_error ? t('pipelines.ed.stError', { error: remote.last_error }) : t('status.error'); msgIcon = 'error'; tone = 'danger'; }
    else if (running && st === 'running') { msg = t('pipelines.ed.stRunning', { where: placementName() }); msgIcon = 'activity'; tone = 'ok'; }
    else if (running) { msg = t('pipelines.ed.stStarting'); msgIcon = 'spinner'; }
    else { msg = t('pipelines.ed.stSaved', { gen: remote.generation }); msgIcon = 'success'; tone = 'ok'; }
    const checkChip = pl.remoteId ? h('span', { class: ['pl-check', checkState], 'data-testid': 'check-state', 'data-state': checkState },
      icon(checkState === 'checking' ? 'spinner' : checkState === 'ok' ? 'success' : checkState === 'bad' ? 'error' : checkState === 'error' ? 'warning' : 'circle', { spin: checkState === 'checking' }),
      t(`pipelines.ed.check.${checkState}`)) : null;
    const issuesBtn = h('button', { type: 'button', class: ['pl-issue-count', errs ? 'bad' : warns ? 'warn' : 'ok'], 'data-testid': 'issue-count', onClick: () => selectTab('issues', true) },
      icon(errs ? 'error' : warns ? 'warning' : 'success', { size: 'sm' }),
      errs ? t('pipelines.problems', { n: errs }) : warns ? t('pipelines.ed.warnings', { n: warns }) : t('pipelines.noProblems'));
    const saveBtn = button(pl.remoteId ? t('common.save') : t('pipelines.ed.saveToNode'), { variant: needSave ? 'primary' : 'secondary', icon: 'save', onClick: () => save(), kbd: `${MOD}+S`, testid: 'pipeline-save', attrs: { disabled: !needSave } });
    const validateBtn = button(t('pipelines.validate'), { icon: 'shield-check', onClick: () => (pl.remoteId ? checkRemote({ quiet: false }) : localCheckToast()), testid: 'pipeline-validate' });
    const startBtn = running
      ? button(t('pipelines.stop'), { icon: 'stop', onClick: () => setDesired('stopped'), testid: 'pipeline-stop' })
      : button(needSave ? t('pipelines.ed.saveAndStart') : t('pipelines.start'), { variant: needSave ? 'secondary' : 'primary', icon: 'play', onClick: () => setDesired('running'), testid: 'pipeline-start' });
    undoBtn.disabled = !hist.past.length;
    redoBtn.disabled = !hist.future.length;
    replace(runbar,
      h('div', { class: ['pl-run-state', tone], 'data-testid': 'run-state' }, icon(msgIcon, { spin: msgIcon === 'spinner' }), h('span', { class: 'pl-run-msg' }, msg), !pl.remoteId && savedAt ? h('span', { class: 'muted small' }, t('pipelines.ed.autosaved', { time: fmtClock(new Date(savedAt).toISOString()) })) : null),
      h('div', { class: 'pl-run-mid' }, issuesBtn, checkChip),
      h('div', { class: 'pl-run-actions' },
        addMenuBtn,
        h('span', { class: 'btn-group pl-undo' }, undoBtn, redoBtn),
        h('span', { class: 'pl-sep', 'aria-hidden': 'true' }),
        validateBtn, saveBtn, startBtn, keysBtn, moreBtn));
  };

  const localCheckToast = () => {
    localIssues = validate(pl, { royaltyFree: royaltyFree() });
    refreshIssues();
    const bad = localIssues.some((i) => i.level === 'error');
    toast(bad ? t('pipelines.localBad') : t('pipelines.localOk'), bad ? 'warn' : 'ok');
    if (bad) selectTab('issues', true);
  };

  const placementName = () => {
    const ps = remote && remote.placement_status;
    if (ps && (ps.state === 'running' || ps.state === 'degraded')) return ps.node_name || ps.node.slice(0, 8);
    return t('pipelines.plan.thisNode');
  };

  // ---------------------------------------------------------------- 見出し
  const renderHead = () => {
    replace(head, pageHeader({
      title: pl.name,
      crumbs: [{ label: t('nav.pipelines'), href: '#/pipelines' }, { label: pl.name }],
      badges: [remote ? observedBadge(remote.observed_state) : badge(t('pipelines.draft'), 'outline', { dot: true })],
      help: h('span', { class: 'ph-help' }, helpPop()),
    }));
  };
  const helpPop = () => {
    const b = iconButton('help', t('pipelines.ed.howTo'), { variant: 'ghost', class: 'help-hint' });
    attachPopover(b, () => [h('h3', null, t('pipelines.ed.howTo')), h('ol', { class: 'pl-howto' },
      h('li', null, t('pipelines.ed.how1')), h('li', null, t('pipelines.ed.how2')), h('li', null, t('pipelines.ed.how3')), h('li', null, t('pipelines.ed.how4')))], { label: t('pipelines.ed.howTo') });
    return b;
  };

  // ---------------------------------------------------------------- タブ（問題・計画・一覧・実行）
  const TABS = ['issues', 'plan', 'outline', 'runtime'];
  let tab = 'issues';
  const tabBtns = {};
  const panels = { issues: issuesBox, plan: planBox, outline: outlineBox, runtime: runtimeBox };
  const tablist = h('div', { class: 'pl-tabs', role: 'tablist', 'aria-label': t('pipelines.ed.details') });
  for (const k of TABS) {
    tabBtns[k] = h('button', { type: 'button', role: 'tab', id: `pl-tab-${k}`, 'aria-controls': `pl-panel-${k}`, class: 'pl-tab', 'data-tab': k, onClick: () => selectTab(k) });
    panels[k].setAttribute('role', 'tabpanel');
    panels[k].id = `pl-panel-${k}`;
    panels[k].setAttribute('aria-labelledby', `pl-tab-${k}`);
    panels[k].classList.add('pl-tabpanel');
    tablist.append(tabBtns[k]);
  }
  tablist.addEventListener('keydown', (e) => {
    const i = TABS.indexOf(tab);
    const j = e.key === 'ArrowRight' ? (i + 1) % TABS.length : e.key === 'ArrowLeft' ? (i - 1 + TABS.length) % TABS.length : e.key === 'Home' ? 0 : e.key === 'End' ? TABS.length - 1 : -1;
    if (j < 0) return;
    e.preventDefault();
    selectTab(TABS[j]);
    tabBtns[TABS[j]].focus();
  });
  const tabLabel = (k) => {
    const errs = localIssues.filter((i) => i.level === 'error').length + remoteIssues.length;
    const warns = localIssues.filter((i) => i.level === 'warn').length;
    const ic = { issues: 'warning', plan: 'list', outline: 'list-tree', runtime: 'activity' }[k];
    return [icon(ic, { size: 'sm' }), t(`pipelines.tab.${k}`),
      k === 'issues' && errs + warns ? h('span', { class: ['pl-tab-count', errs ? 'bad' : 'warn'] }, String(errs + warns)) : null];
  };
  const drawTabs = () => {
    for (const k of TABS) {
      replace(tabBtns[k], ...tabLabel(k));
      tabBtns[k].setAttribute('aria-selected', String(k === tab));
      tabBtns[k].tabIndex = k === tab ? 0 : -1;
      panels[k].hidden = k !== tab;
    }
  };
  const selectTab = (k, scroll = false) => {
    tab = k;
    drawTabs();
    if (scroll) tabsWrap.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  };

  const refreshIssues = () => {
    drawTabs();
    const all = [...remoteIssues, ...localIssues.filter((i) => i.level === 'error'), ...localIssues.filter((i) => i.level === 'warn')];
    const nameOf = (nid) => (nodeById(pl, nid) || {}).name || '?';
    replace(issuesBox, all.length
      ? [remoteIssues.length ? h('p', { class: 'pl-issues-src' }, icon('nodes', { size: 'sm' }), t('pipelines.ed.fromNode')) : null,
        issueList(all, { onFocus: focusNode, nameOf })]
      : emptyState({ icon: 'success', title: t('pipelines.ed.noIssuesTitle'), body: pl.remoteId ? t('pipelines.ed.noIssuesSaved') : t('pipelines.ed.noIssuesDraft'), compact: true }));
  };

  const renderPlan = () => {
    replace(planBox,
      pl.remoteId ? null : alert('info', t('pipelines.ed.planDraft'), { small: true }),
      planView(plan, { streamName, graphNodes: toGraph(pl).nodes, placement: remote && remote.placement_status }));
  };

  const renderOutline = () => {
    const order = topoOrder(pl).map((x) => nodeById(pl, x)).filter(Boolean);
    const m = marks();
    replace(outlineBox, order.length ? h('ol', { class: 'pl-outline', 'data-testid': 'outline' }, order.map((n) => {
      const k = KINDS[n.type];
      const ins = inPorts(n).map((port, i) => { const e = edgeInto(pl, n.id, i); return e ? `${port.name} ← ${refOf(pl, e.from)}` : `${port.name} ← ${t('pipelines.insp.notConnected')}`; });
      const outs = pl.edges.filter((e) => e.from.node === n.id).map((e) => { const b = nodeById(pl, e.to.node); return `${b.name}.${inPorts(b)[e.to.port].name}`; });
      return h('li', { class: ['pl-out-item', canvas.sel.nodes.has(n.id) && 'sel'] },
        h('button', { type: 'button', class: ['pl-out-btn', `k-${k.cat}`], onClick: () => focusNode(n.id), 'data-outline': n.name },
          h('span', { class: 'pl-kind-chip sm' }, icon(k.icon)),
          h('span', { class: 'pl-out-name' }, h('strong', null, n.name), h('span', { class: 'muted' }, ` · ${k.label}`)),
          m.nodes.get(n.id) ? badge(t(m.nodes.get(n.id) === 'error' ? 'pipelines.ed.hasError' : 'pipelines.ed.hasWarn'), m.nodes.get(n.id) === 'error' ? 'danger' : 'warn') : null),
        h('div', { class: 'pl-out-links small' },
          ins.length ? h('div', null, ins.join(' · ')) : null,
          outs.length ? h('div', null, `→ ${outs.join(', ')}`) : null));
    })) : h('p', { class: 'muted' }, t('pipelines.ed.outlineEmpty')));
  };

  const renderRuntime = () => {
    if (!remote) { replace(runtimeBox, emptyState({ icon: 'activity', title: t('pipelines.ed.rtDraftTitle'), body: t('pipelines.draftRuntime'), compact: true })); return; }
    const rt = runtimeOf(remote);
    const st = rt.status;
    const hasCompose = (remote.graph.nodes || []).some((x) => x.type === 'compose');
    const ps = remote.placement_status || {};
    replace(runtimeBox, h('div', { class: 'pl-rt' },
      kvList([
        [t('common.state'), h('span', { class: 'row' }, observedBadge(remote.observed_state), h('span', { class: 'muted small' }, t(`pipelines.ed.desired.${remote.desired_state}`)))],
        [t('pipelines.ed.where'), ps.state ? h('span', null, ps.state === 'running' || ps.state === 'degraded' ? placementName() : t(`pipelines.ed.place.${ps.state}`) || ps.state, ps.state === 'degraded' ? [' ', badge(ps.level_label, 'warn')] : null) : '—'],
        [t('pipelines.generation'), String(remote.generation)],
        [t('common.updated'), h('span', { title: fmtTime(remote.updated_at) }, fmtRelative(remote.updated_at))],
      ]),
      remote.last_error ? alert('error', remote.last_error, { title: t('pipelines.lastError'), small: true, actions: [
        button(t('pipelines.ed.seeDevices'), { size: 'sm', icon: 'nodes', onClick: () => ctx.navigate('#/nodes') }),
        button(t('pipelines.validate'), { size: 'sm', icon: 'shield-check', onClick: () => { selectTab('issues'); checkRemote({ quiet: false }); } })] }) : null,
      st && (st.sources || []).length ? h('div', null, h('h3', { class: 'pl-plan-h' }, t('pipelines.ed.sources')), h('ul', { class: 'pl-rt-sources' }, st.sources.map((x) => h('li', null,
        h('span', { class: ['dot', x.state === 'live' ? 'ok' : x.state === 'stalled' ? 'warn' : 'danger'] }),
        h('code', null, x.id),
        h('span', { class: 'muted small' }, t('pipelines.ed.sourceLine', { state: t(`pipelines.live.${['live', 'stalled', 'missing'].includes(x.state) ? x.state : 'missing'}`), fps: (x.fps || 0).toFixed(1), n: x.decoded_frames ?? 0 })),
        x.on_program ? badge('PGM', 'live') : x.on_preview ? badge('PVW', 'ok') : null)))) : h('p', { class: 'muted small' }, t('pipelines.noSourceState')),
      hasCompose ? h('a', { class: 'btn', href: `#/production?pipeline=${encodeURIComponent(remote.id)}`, 'data-testid': 'open-switcher' }, icon('switcher'), t('pipelines.openSwitcher'), icon('arrow-right', { size: 'sm' })) : null));
    // 入力の状態を部品に（点の色）
    runtimeMap = st && st.sources ? new Map(st.sources.map((x) => [x.id, ['live', 'stalled'].includes(x.state) ? x.state : 'missing'])) : null;
    canvas.setRunning(remote.observed_state === 'running');
  };

  // ---------------------------------------------------------------- 復元のお知らせ
  if (restoredNote && restoredNote.kind === 'restored') {
    replace(banner, alert('info', t('pipelines.ed.restored', { time: fmtRelative(restoredNote.at) }), {
      small: true, actions: [button(t('pipelines.ed.discard'), { size: 'sm', onClick: reloadFromNode })],
    }));
  } else if (restoredNote && restoredNote.kind === 'conflict') {
    replace(banner, alert('warn', t('pipelines.ed.conflict', { time: fmtRelative(restoredNote.at) }), {
      small: true,
      actions: [
        button(t('pipelines.ed.useMine'), { size: 'sm', onClick: () => {
          const m = restoredNote.mine;
          Object.assign(pl, { name: m.name, options: m.options, nodes: m.nodes, edges: m.edges });
          change('paste', { rerender: true });
          replace(banner);
        } }),
        button(t('pipelines.ed.discardMine'), { size: 'sm', variant: 'ghost', onClick: () => { edits.remove(pl.remoteId); replace(banner); } }),
      ],
    }));
  }

  // ---------------------------------------------------------------- キーボード
  const onKey = (e) => {
    if (dead || document.querySelector('dialog[open]')) return;
    const mod = e.ctrlKey || e.metaKey;
    const k = e.key.toLowerCase();
    if (mod && k === 's') { e.preventDefault(); if (!pl.remoteId || pl.dirty) save(); return; }
    if (isTyping(e.target)) return;
    const inCanvas = canvas.el.contains(document.activeElement) || document.activeElement === document.body || (root.contains(document.activeElement) && !inspectorBox.contains(document.activeElement));
    if (mod && k === 'z' && !e.shiftKey) { e.preventDefault(); undo(); return; }
    if ((mod && k === 'z' && e.shiftKey) || (mod && k === 'y')) { e.preventDefault(); redo(); return; }
    if (!inCanvas) return;
    const selIds = [...canvas.sel.nodes];
    if (mod && k === 'a') { e.preventDefault(); canvas.select(pl.nodes.map((n) => n.id)); inspect(); return; }
    if (mod && k === 'c') { if (copyIds(selIds)) e.preventDefault(); return; }
    if (mod && k === 'x') { if (copyIds(selIds)) { e.preventDefault(); removeIds(selIds); } return; }
    if (mod && k === 'v') { e.preventDefault(); paste(); return; }
    if (mod && k === 'd') { e.preventDefault(); duplicate(selIds); return; }
    if (mod || e.altKey) return;
    if (e.key === 'Delete' || e.key === 'Backspace') {
      if (selIds.length) { e.preventDefault(); removeIds(selIds); } else if (canvas.sel.edge) { e.preventDefault(); pl.edges = pl.edges.filter((x) => x.id !== canvas.sel.edge); canvas.sel.edge = null; change('disconnect', { rerender: true }); }
      return;
    }
    if (e.key === 'Escape') { if (selIds.length || canvas.sel.edge) { canvas.select([]); inspect(); } canvas.closeMenu(); return; }
    if (e.key === '0') { e.preventDefault(); canvas.fit(); return; }
    if (e.key === '1') { e.preventDefault(); canvas.zoomTo(1); return; }
    if (e.key === '+' || e.key === '=') { e.preventDefault(); canvas.zoomBy(1.2); return; }
    if (e.key === '-') { e.preventDefault(); canvas.zoomBy(1 / 1.2); return; }
    if (k === 'l' && !e.shiftKey) { e.preventDefault(); doAutoLayout(); }
  };
  document.addEventListener('keydown', onKey);

  // ---------------------------------------------------------------- 組み立て
  const tabsWrap = h('section', { class: 'pl-details panel', 'aria-label': t('pipelines.ed.details') }, tablist, issuesBox, planBox, outlineBox, runtimeBox);
  replace(root,
    head,
    banner,
    h('div', { class: 'pl-runbar-wrap' }, runbar),
    h('div', { class: 'pl-workspace' }, palette, canvas.el, inspectorBox),
    tabsWrap,
    live);
  renderHead();
  localIssues = validate(pl, { royaltyFree: royaltyFree() });
  canvas.draw();
  inspect();
  refreshIssues();
  renderPlan();
  renderOutline();
  renderRuntime();
  renderRunbar();
  requestAnimationFrame(() => canvas.fit());
  if (pl.remoteId && !localIssues.some((i) => i.level === 'error')) checkRemote();

  // ---------------------------------------------------------------- 動いているものの様子
  let pollTimer = null;
  let fastUntil = 0;
  const poll = async () => {
    if (dead || !pl.remoteId || document.visibilityState !== 'visible') return;
    try {
      const r = await api.get(pl.remoteId);
      if (dead) return;
      const before = remote;
      remote = r.data;
      if (!pl.dirty && before && before.generation !== remote.generation) {
        const fresh = fromGraph(remote.graph, { name: remote.name, positions: layouts.get(pl.remoteId) });
        Object.assign(pl, { name: fresh.name, options: fresh.options, nodes: fresh.nodes, edges: fresh.edges, etag: r.etag || pl.etag });
        refresh({ rerender: true });
        renderHead();
      } else if (!pl.dirty) pl.etag = r.etag || pl.etag;
      if (!before || before.observed_state !== remote.observed_state || before.desired_state !== remote.desired_state) { renderHead(); renderRunbar(); }
      renderRuntime();
      if (remote.plan && !plan) { plan = remote.plan; renderPlan(); }
      canvas.draw();
    } catch (e) {
      if (e.status === 404) { toast(t('pipelines.gone'), 'warn'); dead = true; ctx.navigate('#/pipelines'); }
    }
  };
  const fastPoll = () => { fastUntil = Date.now() + 15000; };
  let tick = 0;
  pollTimer = setInterval(() => { tick += 1; if (Date.now() < fastUntil || tick % 4 === 0) poll(); }, 500);

  return () => {
    dead = true;
    clearInterval(pollTimer);
    document.removeEventListener('keydown', onKey);
    persist.cancel();
    persistNow();
    canvas.destroy();
    scheduleCheck.cancel();
  };

  function notFound() {
    return emptyState({
      icon: 'pipelines', title: t('pipelines.ed.notFoundTitle'), body: t('pipelines.ed.notFoundBody'),
      actions: [h('a', { class: 'btn primary', href: '#/pipelines' }, icon('arrow-left'), t('pipelines.ed.backToList'))],
    });
  }
}

