// グリッド: 何がどのノードで動いているか、なぜそこか（GET /grid）。node:admin が要る。
//
//   上: 状態ごとの数（そのまま絞り込みになる）・検索・種類・ノード。条件は URL（#/grid?state=&kind=&node=&q=）に残す。
//   中: ノードごとの列（予約の量・ワークロード）と「配置できていない」列。
//   下: 一覧（名前・種類・状態・ノード・装置・優先度・割り当て・段階・理由）。
//
// ワークロードの名前は資源から引く（input:/output: は持ち主の配信の名前、pipeline: はパイプラインの名前）。

import { h, replace, select, debounce } from '../lib/dom.js';
import { errorBox } from '../lib/ui.js';
import { grid as gridApi } from '../api/ops.js';
import { loadNames, forgetNames, ioInfo, shortId } from '../lib/names.js';
import { fmtNumber, fmtPercent } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t, tn, has } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { guideUrl } from '../lib/guide.js';
import { listNav, button, pageHeader, panel, statusPill, badge, alert, helpHint, meter, skeleton, emptyState, dataTable, segmented, searchBox, tooltip } from '../ui/index.js';

const KIND_ICON = { ingest: 'download', egress: 'upload', pipeline: 'pipelines', room: 'rooms', recording: 'record', transcode: 'zap', capture: 'camera', relay: 'move', media_pipeline: 'media-pipelines' };
const STATE_ICON = { running: 'success', degraded: 'trending-down', pending: 'hourglass', unschedulable: 'ban' };
const PRIO = { low: 0, normal: 1, high: 2, critical: 3 };
const STATES = ['running', 'degraded', 'pending', 'unschedulable'];

function params() {
  return new URLSearchParams(location.hash.split('?')[1] || '');
}

/** 配置の理由を文にする。 */
export function reasonText(w, nodeName) {
  if (w.state === 'unschedulable' && w.unschedulable && w.unschedulable.length) {
    return w.unschedulable.map((r) => tn('grid.rejectCount', r.nodes, { reason: has(`grid.reject.${r.reason}`) ? t(`grid.reject.${r.reason}`) : r.reason })).join(t('grid.listSep'));
  }
  if (w.state === 'pending') return t('grid.pendingText');
  const r = w.reason;
  if (!r) return '—';
  const p = { node: r.node ? nodeName(r.node) : '', device: r.device || '', by: r.by || '' };
  return has(`grid.reason.${r.code}`) ? t(`grid.reason.${r.code}`, p) : r.code;
}

export async function render(root, _p, { caps }) {
  root = root.appendChild(h('div', { class: 'ops-view' }));
  const q0 = params();
  const f = { state: q0.get('state') || '', kind: q0.get('kind') || '', node: q0.get('node') || '', q: q0.get('q') || '' };
  const noteBox = h('div');
  const filterBox = h('div', { class: 'filter-bar' });
  const boardBox = h('div', null, skeleton({ kind: 'cards', rows: 3 }));
  const listBox = h('div');
  const names = new Map(); // workload id → {name, sub, href}
  let data = null;
  let disposed = false;

  const nodeName = (id) => {
    const n = data && data.nodes.find((x) => x.id === id);
    return n ? (n.name || shortId(n.id)) : shortId(id);
  };

  const kindLabel = (k) => (has(`grid.kind.${k}`) ? t(`grid.kind.${k}`) : k);

  const info = (w) => names.get(w.id) || { name: w.id.replace(/^[a-z_]+:/, '').slice(0, 10), sub: kindLabel(w.kind), href: null };

  const resolveNames = async () => {
    let nm = await loadNames(['stream', 'pipeline']);
    // 表を読んだ後に作られた配信・パイプラインがあれば、表を読み直す
    const ios = await Promise.all(data.workloads.map((w) => (/^(input|output):/.test(w.id) ? ioInfo(...w.id.split(':')) : null)));
    const stale = ios.some((io) => io && !nm('stream', io.stream))
      || data.workloads.some((w) => w.id.startsWith('pipeline:') && !nm('pipeline', w.id.slice(9)));
    if (stale) {
      forgetNames('stream');
      forgetNames('pipeline');
      nm = await loadNames(['stream', 'pipeline']);
    }
    await Promise.all(data.workloads.map(async (w) => {
      if (names.has(w.id)) return;
      const [type, ...rest] = w.id.split(':');
      const id = rest.join(':');
      if (type === 'input' || type === 'output') {
        const io = await ioInfo(type, id);
        if (io) {
          names.set(w.id, {
            name: nm('stream', io.stream) || shortId(io.stream),
            sub: t(type === 'input' ? 'grid.ioIn' : 'grid.ioOut', { protocol: has(`grid.proto.${io.protocol}`) ? t(`grid.proto.${io.protocol}`) : io.protocol }),
            href: `#/streams/${encodeURIComponent(io.stream)}`,
          });
        }
      } else if (type === 'pipeline') {
        names.set(w.id, { name: nm('pipeline', id) || shortId(id), sub: kindLabel(w.kind), href: `#/pipelines?id=${encodeURIComponent(id)}` });
      } else if (type === 'room') {
        names.set(w.id, { name: id, sub: kindLabel(w.kind), href: `#/rooms/${encodeURIComponent(id)}` });
      }
    }));
  };

  // ---------------------------------------------------------------- 絞り込み
  const syncUrl = () => {
    const u = new URLSearchParams();
    for (const [k, v] of Object.entries(f)) if (v) u.set(k, v);
    const qs = u.toString();
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/grid${qs ? `?${qs}` : ''}`);
  };
  const matches = (w) => {
    if (f.state && w.state !== f.state) return false;
    if (f.kind && w.kind !== f.kind) return false;
    if (f.node && w.node !== f.node) return false;
    if (f.q) {
      const i = info(w);
      const hay = `${w.id} ${i.name} ${i.sub} ${w.device || ''} ${w.level_label || ''}`.toLowerCase();
      if (!hay.includes(f.q.toLowerCase())) return false;
    }
    return true;
  };
  const setFilter = (k, v) => { f[k] = v; syncUrl(); drawFilters(); drawBody(); };

  const drawFilters = () => {
    if (!data) return;
    const count = (s) => data.workloads.filter((w) => (!s || w.state === s)).length;
    const seg = segmented({
      label: t('grid.filter.state'), value: f.state, testid: 'grid-state-filter',
      options: [['', `${t('common.all')} ${count('')}`], ...STATES.map((s) => [s, `${t(`grid.state.${s}`)} ${count(s)}`])],
      onChange: (v) => setFilter('state', v),
    });
    const kinds = [...new Set(data.workloads.map((w) => w.kind))].sort();
    const kindSel = select([['', t('grid.filter.allKinds')], ...kinds.map((k) => [k, kindLabel(k)])], f.kind, { 'aria-label': t('grid.filter.kind'), 'data-testid': 'grid-kind-filter' });
    kindSel.addEventListener('change', () => setFilter('kind', kindSel.value));
    const nodeSel = select([['', t('grid.filter.allNodes')], ...data.nodes.map((n) => [n.id, n.name || shortId(n.id)])], f.node, { 'aria-label': t('grid.filter.node'), 'data-testid': 'grid-node-filter' });
    nodeSel.addEventListener('change', () => setFilter('node', nodeSel.value));
    const active = f.state || f.kind || f.node || f.q;
    replace(filterBox,
      h('div', { class: 'filter-row' }, seg),
      h('div', { class: 'filter-row' },
        search,
        kindSel, nodeSel,
        active ? button(t('grid.filter.clear'), { variant: 'ghost', size: 'sm', icon: 'close', testid: 'grid-clear', onClick: () => { Object.assign(f, { state: '', kind: '', node: '', q: '' }); search.input.value = ''; syncUrl(); drawFilters(); drawBody(); } }) : null));
  };
  const search = searchBox({ placeholder: t('grid.filter.search'), value: f.q, shortcut: true, testid: 'grid-search', onInput: debounce((v) => { f.q = v.trim(); syncUrl(); drawFilters(); drawBody(); }, 200) });

  // ---------------------------------------------------------------- ノードの列
  const chip = (w) => {
    const i = info(w);
    const reasonCode = w.reason && w.reason.code;
    const moved = ['node_down', 'device_down', 'draining', 'constraint_changed', 'preempted', 'overcommitted'].includes(reasonCode);
    const el = h(i.href ? 'a' : 'div', {
      class: ['wl-chip', `st-${w.state}`].join(' '), href: i.href, 'data-key': w.id, 'data-testid': 'grid-chip', 'data-workload': w.id,
    },
    h('span', { class: 'wl-icon' }, icon(KIND_ICON[w.kind] || 'circle')),
    h('span', { class: 'wl-text' },
      h('span', { class: 'wl-name truncate' }, i.name),
      h('span', { class: 'wl-sub truncate' }, i.sub)),
    h('span', { class: 'wl-flags' },
      w.pinned ? h('span', { class: 'wl-flag', title: t('grid.pinned') }, icon('pin', { label: t('grid.pinned') })) : null,
      moved ? h('span', { class: 'wl-flag warn', title: reasonText(w, nodeName) }, icon('move', { label: t('grid.moved') })) : null,
      w.state === 'degraded' ? badge(w.level_label || t('grid.level', { n: w.level }), 'warn') : null,
      w.priority !== 'normal' ? badge(t(`grid.prio.${w.priority}`), w.priority === 'critical' || w.priority === 'high' ? 'accent' : 'outline') : null));
    tooltip(el, reasonText(w, nodeName));
    return el;
  };

  const column = (n, items) => {
    const isLeader = data.scheduler === n.id;
    const cpu = n.capacity.cpu_millis ? n.reserved.cpu_millis / n.capacity.cpu_millis : 0;
    const mem = n.capacity.memory_mb ? n.reserved.memory_mb / n.capacity.memory_mb : 0;
    return h('section', { class: ['grid-col', !n.up && 'is-down', n.draining && 'is-draining'].filter(Boolean).join(' '), 'data-testid': 'grid-col', 'data-node': n.id, 'aria-label': n.name || n.id },
      h('header', { class: 'grid-col-head' },
        h('div', { class: 'row' },
          h('strong', { class: 'truncate' }, n.name || shortId(n.id)),
          isLeader ? h('span', { class: 'leader-mark', title: t('nodes.leaderHelp') }, icon('crown', { label: t('nodes.leader') })) : null,
          h('span', { class: 'spacer' }),
          !n.up ? badge(t('nodes.state.down'), 'danger', { dot: true }) : n.draining ? statusPill('draining') : statusPill('accepting')),
        h('div', { class: 'grid-col-meters' },
          meter({ label: 'CPU', value: n.reserved.cpu_millis, max: n.capacity.cpu_millis || 1, kind: cpu >= 0.95 ? 'danger' : cpu >= 0.8 ? 'warn' : undefined, valueText: fmtPercent(cpu) }),
          meter({ label: t('nodes.meter.memoryShort'), value: n.reserved.memory_mb, max: n.capacity.memory_mb || 1, kind: mem >= 0.95 ? 'danger' : mem >= 0.8 ? 'warn' : undefined, valueText: fmtPercent(mem) })),
        n.gpus.length ? h('div', { class: 'row small muted' }, icon('gpu'), n.gpus.map((g) => g.id).join(', ')) : null),
      items.length
        ? h('div', { class: 'grid-col-body' }, items.map(chip))
        : h('p', { class: 'grid-col-empty' }, n.up ? t('grid.colEmpty') : t('grid.colDown')));
  };

  const unplacedColumn = (items) => h('section', { class: 'grid-col unplaced', 'data-testid': 'grid-col-unplaced', 'aria-label': t('grid.unplaced') },
    h('header', { class: 'grid-col-head' },
      h('div', { class: 'row' }, icon('ban'), h('strong', null, t('grid.unplaced')), h('span', { class: 'spacer' }), badge(fmtNumber(items.length), 'danger')),
      h('p', { class: 'small muted' }, t('grid.unplacedHelp'))),
    h('div', { class: 'grid-col-body' }, items.map((w) => h('div', { class: 'unplaced-item' }, chip(w), h('p', { class: 'small' }, reasonText(w, nodeName))))));

  // ---------------------------------------------------------------- 一覧
  const drawList = (rows) => {
    replace(listBox, dataTable({
      caption: t('grid.listCaption'),
      testid: 'grid-table',
      sort: { key: 'state', dir: 'desc' },
      rowKey: (w) => w.id,
      columns: [
        { key: 'name', label: t('grid.col.workload'), primary: true, sort: (w) => info(w).name, render: (w) => {
          const i = info(w);
          return h('div', { class: 'wl-cell' }, h('span', { class: 'wl-icon' }, icon(KIND_ICON[w.kind] || 'circle')),
            h('div', null, i.href ? h('a', { href: i.href }, i.name) : h('span', null, i.name), h('div', { class: 'small muted' }, i.sub)));
        } },
        { key: 'state', label: t('common.state'), sort: (w) => STATES.indexOf(w.state), render: (w) => badge(t(`grid.state.${w.state}`), { running: 'ok', degraded: 'warn', pending: 'info', unschedulable: 'danger' }[w.state], { icon: STATE_ICON[w.state] }) },
        { key: 'node', label: t('grid.col.node'), sort: (w) => (w.node ? nodeName(w.node) : null), render: (w) => (w.node ? h('span', null, nodeName(w.node), w.device ? h('span', { class: 'muted small' }, ` · ${w.device}`) : null) : h('span', { class: 'muted' }, '—')) },
        { key: 'prio', label: t('grid.col.priority'), sort: (w) => PRIO[w.priority], render: (w) => t(`grid.prio.${w.priority}`) },
        { label: t('grid.col.placement'), render: (w) => (w.pinned ? h('span', { class: 'row' }, icon('pin'), t('grid.pinned')) : t('grid.auto')) },
        { label: t('grid.col.level'), render: (w) => (w.level === null || w.level === undefined ? '—' : w.level === 0 ? t('grid.full') : h('span', null, t('grid.level', { n: w.level }), w.level_label ? h('span', { class: 'muted small' }, ` (${w.level_label})`) : null)) },
        { label: t('grid.col.reason'), wrap: true, render: (w) => h('span', { class: 'small' }, reasonText(w, nodeName)) },
      ],
      rows,
      empty: { icon: 'filter', title: t('grid.noMatch'), body: t('grid.noMatchBody') },
    }));
  };

  const drawBody = () => {
    if (!data) return;
    if (!data.workloads.length) {
      replace(boardBox, h('div', { class: 'grid-board' }, data.nodes.map((n) => column(n, []))));
      replace(listBox, emptyState({
        icon: 'grid', title: t('grid.empty'), body: t('grid.emptyBody'), testid: 'grid-empty',
        actions: [h('a', { class: 'btn', href: guideUrl('09-cluster-grid.md'), target: '_blank', rel: 'noopener noreferrer' }, icon('book'), t('grid.readGuide'), icon('external'))],
      }));
      return;
    }
    const rows = data.workloads.filter(matches);
    const placed = rows.filter((w) => w.node && (w.state === 'running' || w.state === 'degraded'));
    const unplaced = rows.filter((w) => !placed.includes(w));
    const nodes = f.node ? data.nodes.filter((n) => n.id === f.node) : data.nodes;
    const sorter = (a, b) => (PRIO[b.priority] - PRIO[a.priority]) || info(a).name.localeCompare(info(b).name);
    replace(boardBox, h('div', { class: 'grid-board', 'data-testid': 'grid-board' },
      unplaced.length ? unplacedColumn(unplaced.sort(sorter)) : null,
      nodes.map((n) => column(n, placed.filter((w) => w.node === n.id).sort(sorter)))));
    drawList(rows);
  };

  const drawNote = () => {
    if (!data) return;
    const notes = [];
    if (!data.scheduler && caps && caps.cluster) notes.push(alert('info', t('grid.notLeader'), { small: true }));
    const down = data.nodes.filter((n) => !n.up);
    if (down.length) notes.push(alert('warn', tn('grid.nodesDown', down.length, { names: down.map((n) => n.name || shortId(n.id)).join(', ') }), { small: true }));
    replace(noteBox, notes);
  };

  let seq = 0;
  const refresh = async () => {
    const my = ++seq;
    try {
      const { data: g } = await gridApi.get();
      if (my !== seq || disposed) return;
      data = g;
      data.nodes.sort((a, b) => String(a.name || a.id).localeCompare(String(b.name || b.id), undefined, { numeric: true }));
      drawNote();
      drawFilters();
      drawBody();
      await resolveNames();
      if (my !== seq || disposed) return;
      drawFilters();
      drawBody();
    } catch (e) {
      if (disposed || e.status === 401) return;
      replace(filterBox);
      replace(boardBox, e.status === 403 ? alert('warn', t('grid.forbidden'), { title: t('nodes.forbiddenTitle') }) : errorBox(e, { actions: [button(t('common.retry'), { size: 'sm', icon: 'refresh', onClick: () => refresh() })] }));
      replace(listBox);
    }
  };

  root.append(
    pageHeader({
      title: t('nav.grid'),
      description: t('grid.lead'),
      help: helpHint({ title: t('nav.grid'), body: t('grid.help'), guide: '09-cluster-grid.md' }),
      actions: [h('a', { class: 'btn', href: '#/nodes' }, icon('nodes'), t('nav.nodes')), button(t('common.refresh'), { iconOnly: true, icon: 'refresh', onClick: () => refresh() })],
    }),
    noteBox,
    filterBox,
    panel({ title: t('grid.board'), icon: 'layout-grid', description: t('grid.boardDesc'), testid: 'grid-board-panel' }, boardBox),
    panel({ title: t('grid.list'), icon: 'list', testid: 'grid-list-panel' }, listBox));
  await refresh();
  const offNav = listNav(root, { rows: () => [...root.querySelectorAll('.grid-board .wl-chip')], open: (el) => { if (el.getAttribute('href')) location.hash = el.getAttribute('href'); } });
  const offLive = refreshOn(['grid.', 'node.', 'device.', 'input.', 'output.', 'pipeline.', 'room.'], refresh, { wait: 700 });
  return () => { disposed = true; offNav(); offLive(); };
}
