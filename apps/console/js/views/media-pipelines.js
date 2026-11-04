// メディアパイプラインの一覧（部屋で絞り込み）・開始と停止・削除。作成は #/media-pipelines/new（用途から選ぶ手順）。
// #/media-pipelines?new=1 は作成の画面へ、?room=ID はその部屋のものだけを出す。

import { h, replace, select } from '../lib/dom.js';
import { load, reportError, confirmDialog, toast } from '../lib/ui.js';
import { rooms, mediaPipelines } from '../api/realtime.js';
import { listAll } from '../api/resources.js';
import { fmtTime, fmtRelative } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t, tn } from '../i18n/index.js';
import { button, iconButton, pageHeader, dataTable, emptyState, statusPill, searchBox, helpHint, attachMenu, alert } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { guideUrl } from '../lib/guide.js';
import { onPageKey } from '../shortcuts.js';
import { listKeys } from '../lib/listnav.js';
import { invalidatePalette } from '../palette.js';
import { forgetRecent } from '../lib/recent.js';
import { pipelineState, pipelineUse, pipelineProducts, USE_CASES } from '../lib/realtime.js';

export const USE_ICON = { record: 'record', broadcast: 'send-arrow', export: 'streams', captions: 'captions' };

const STATE = {
  running: ['running', 'mp.state.running'],
  idle: ['idle', 'mp.state.idle'],
  waiting: ['pending', 'mp.state.waiting'],
  stopped: ['stopped', 'mp.state.stopped'],
};

/** 状態のピル（書き出している人数つき）。 */
export function mpStatePill(m, testid) {
  const st = pipelineState(m);
  const [kind, key] = STATE[st];
  const label = st === 'running' ? tn('mp.state.runningN', m.status.participants.length) : t(key);
  return statusPill(kind, { label, testid });
}

/** 用途のカード（作成の入口）。href(use) は行き先。 */
export function useCards(href, { compact = false, selected = null, onSelect = null, disabled = [], disabledNote = '' } = {}) {
  return h('div', { class: ['use-cards', compact && 'compact'].filter(Boolean).join(' '), role: onSelect ? 'radiogroup' : null, 'aria-label': onSelect ? t('mp.new.useLabel') : null },
    USE_CASES.map((u) => {
      const inner = [
        h('span', { class: 'use-icon' }, icon(USE_ICON[u])),
        h('span', { class: 'use-text' }, h('strong', null, t(`mp.use.${u}`)), h('span', { class: 'small muted' }, t(compact ? `mp.use.${u}Short` : `mp.use.${u}Desc`))),
        onSelect ? icon('check', { class: 'use-check' }) : icon('chevron-right', { class: 'use-go' }),
      ];
      if (disabled.includes(u)) {
        return h('div', { class: 'use-card is-disabled', 'aria-disabled': 'true', 'data-testid': `use-${u}` },
          inner[0], h('span', { class: 'use-text' }, h('strong', null, t(`mp.use.${u}`)), h('span', { class: 'small muted' }, disabledNote)));
      }
      if (onSelect) {
        return h('button', { type: 'button', class: 'use-card btn-reset', role: 'radio', 'aria-checked': String(selected === u), 'data-use': u, 'data-testid': `use-${u}`, onClick: () => onSelect(u) }, inner);
      }
      return h('a', { class: 'use-card', href: href(u), 'data-testid': `use-${u}` }, inner);
    }));
}

/** 作られるものの短い札（一覧の列）。 */
export function productTags(m) {
  const out = [];
  for (const p of pipelineProducts(m)) {
    if (p.kind === 'participantStreams') {
      if (p.artifact) out.push([USE_ICON.export, p.who ? tn('mp.tag.someParticipants', p.who.length) : t('mp.tag.participants')]);
    } else if (p.kind === 'composite') out.push(['layout-grid', t('mp.tag.composite', { w: p.width, h: p.height, codec: String(p.video_codec).toUpperCase() })]);
    else if (p.kind === 'recording') out.push(['record', t(p.target === 'composite' ? 'mp.tag.recComposite' : 'mp.tag.recParticipants')]);
    else if (p.kind === 'egress') out.push(['send-arrow', p.protocol === 'paas' ? t('mp.tag.paas', { provider: p.provider }) : t('mp.tag.egress', { proto: p.protocol === 'srt_push' ? 'SRT' : 'RTMP' })]);
    else if (p.kind === 'captions') out.push(['captions', t('mp.tag.captions')]);
  }
  return h('div', { class: 'badge-row' }, out.map(([ic, text]) => h('span', { class: 'chip' }, icon(ic, { size: 'sm' }), text)));
}

/** 開始・停止（PATCH desired_state）。 */
export async function setMpState(m, desired) {
  if (desired === 'stopped') {
    const ok = await confirmDialog(t('mp.stopTitle', { name: m.name }), t('mp.stopBody'), {
      ok: t('mp.stop'), danger: false, icon: 'stop', testid: 'mp-stop-dialog', consequences: [t('mp.stopC1'), t('mp.stopC2'), t('mp.stopC3')],
    });
    if (!ok) return null;
  }
  try {
    const { data } = await mediaPipelines.update(m.id, { desired_state: desired }, `"${m.generation}"`);
    toast(desired === 'stopped' ? t('mp.stopped', { name: m.name }) : t('mp.started', { name: m.name }), 'ok');
    return data;
  } catch (e) {
    reportError(e, desired === 'stopped' ? t('mp.stop') : t('mp.start'));
    return null;
  }
}

/** 削除。消したら true。 */
export async function deleteMp(m) {
  const ok = await confirmDialog(t('mp.deleteTitle', { name: m.name }), t('mp.deleteBody'), {
    ok: t('common.delete'), testid: 'mp-delete-dialog', consequences: [t('mp.deleteC1'), t('mp.deleteC2'), t('mp.deleteC3')],
  });
  if (!ok) return false;
  try {
    await mediaPipelines.remove(m.id);
    forgetRecent(`#/media-pipelines/${encodeURIComponent(m.id)}`);
    invalidatePalette();
    toast(t('mp.deleted', { name: m.name }), 'ok');
    return true;
  } catch (e) {
    reportError(e, t('common.delete'));
    return false;
  }
}

export async function render(root, _params, { navigate }) {
  const params = new URLSearchParams(location.hash.split('?')[1] || '');
  if (params.get('new') === '1') {
    navigate(`/media-pipelines/new${params.get('room') ? `?room=${encodeURIComponent(params.get('room'))}` : ''}`);
    return null;
  }
  const box = h('div');
  let items = [];
  let roomList = [];
  let filter = '';
  let room = params.get('room') || '';
  let tableEl = null;
  const roomSel = select([['', t('mp.allRooms')]], room, { 'data-testid': 'mp-room-filter', 'aria-label': t('mp.col.room'), class: 'toolbar-select' });

  const href = (m) => `#/media-pipelines/${encodeURIComponent(m.id)}`;
  const newHref = (u) => `#/media-pipelines/new?use=${u}${room ? `&room=${encodeURIComponent(room)}` : ''}`;
  const after = (fn) => async () => { if (await fn()) refresh(); };

  const columns = [
    { key: 'name', label: t('common.name'), primary: true, sort: (m) => m.name, render: (m) => h('div', { class: 'cell-stack' },
      h('span', { class: 'row' }, icon(USE_ICON[pipelineUse(m)], { size: 'sm', class: 'subtle' }), h('a', { href: href(m), 'data-row-link': '', class: 'strong' }, m.name),
        m.managed ? h('span', { class: 'tag', title: t('mp.managedTitle') }, t('mp.managed')) : null),
      h('span', { class: 'small muted' }, t(`mp.use.${pipelineUse(m)}`))) },
    { key: 'room', label: t('mp.col.room'), sort: (m) => m.room, render: (m) => h('a', { href: `#/rooms/${encodeURIComponent(m.room)}`, class: 'mono' }, m.room) },
    { key: 'products', label: t('mp.col.products'), wrap: true, render: productTags },
    { key: 'state', label: t('common.state'), sort: (m) => ({ running: 0, idle: 1, waiting: 2, stopped: 3 }[pipelineState(m)]), render: (m) => mpStatePill(m, `mp-state-${m.name}`) },
    { key: 'updated', label: t('common.updated'), sort: (m) => m.updated_at, render: (m) => h('span', { title: fmtTime(m.updated_at) }, fmtRelative(m.updated_at)) },
    { key: 'actions', label: t('common.actions'), actions: true, render: (m) => {
      const running = m.desired_state === 'running';
      const toggle = iconButton(running ? 'stop' : 'play', running ? t('mp.stopNamed', { name: m.name }) : t('mp.startNamed', { name: m.name }), {
        testid: `mp-toggle-${m.name}`, attrs: { disabled: m.managed }, onClick: after(() => setMpState(m, running ? 'stopped' : 'running')),
      });
      const more = iconButton('more', t('mp.actionsFor', { name: m.name }));
      attachMenu(more, () => [
        { label: t('mp.open'), icon: 'arrow-right', onSelect: () => navigate(`/media-pipelines/${encodeURIComponent(m.id)}`) },
        { label: t('mp.openRoom'), icon: 'rooms', onSelect: () => navigate(`/rooms/${encodeURIComponent(m.room)}`) },
        { separator: true },
        { label: t('common.delete'), icon: 'trash', danger: true, disabled: m.managed, onSelect: after(() => deleteMp(m)) },
      ]);
      return h('div', { class: 'row-actions' }, toggle, more);
    } },
  ];

  const draw = () => {
    if (!items.length) {
      const noRooms = !roomList.length;
      return h('div', { 'data-testid': 'mp-empty' },
        emptyState({
          icon: 'media-pipelines', title: room ? t('mp.emptyRoomTitle', { room }) : t('mp.emptyTitle'), body: t('mp.emptyBody'),
          actions: [h('a', { class: 'btn', href: guideUrl('11-media-pipelines.md'), target: '_blank', rel: 'noopener noreferrer' }, icon('book'), t('mp.readGuide'), icon('external'))],
        }),
        noRooms
          ? alert('info', t('mp.noRooms'), { actions: [h('a', { class: 'btn sm', href: '#/rooms?new=1' }, icon('plus'), t('rooms.create'))] })
          : h('div', { class: 'empty-uses' }, h('h3', { class: 'section-label' }, t('mp.startWith')), useCards(newHref)));
    }
    const q = filter.trim().toLowerCase();
    const rows = q ? items.filter((m) => m.name.toLowerCase().includes(q) || m.room.toLowerCase().includes(q)) : items;
    return dataTable({ columns, rows, caption: t('mp.caption'), sort: { key: 'state', dir: 'asc' }, rowKey: (m) => m.id, testid: 'mp-table', empty: { icon: 'search', title: t('mp.noMatch', { q: filter }) } });
  };

  const refresh = () => load(box, async () => {
    const [list, rs] = await Promise.all([listAll(mediaPipelines.list, { room: room || undefined }), roomList.length ? roomList : listAll(rooms.list).catch(() => [])]);
    items = list;
    roomList = rs;
    if (roomSel.options.length === 1) for (const r of roomList) roomSel.append(h('option', { value: r.id, selected: r.id === room }, r.id));
    tableEl = draw();
    return tableEl;
  }, { kind: 'table' });

  roomSel.addEventListener('change', () => {
    room = roomSel.value;
    const q = room ? `?room=${encodeURIComponent(room)}` : '';
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/media-pipelines${q}`);
    refresh();
  });
  const search = searchBox({ placeholder: t('mp.filter'), shortcut: true, testid: 'mp-filter', onInput: (v) => { filter = v; if (tableEl) { const n = draw(); tableEl.replaceWith(n); tableEl = n; } } });
  const create = () => navigate(`/media-pipelines/new${room ? `?room=${encodeURIComponent(room)}` : ''}`);

  root.append(
    pageHeader({
      title: t('nav.media-pipelines'),
      description: t('mp.lead'),
      help: helpHint({ title: t('nav.media-pipelines'), body: t('mp.help'), guide: '11-media-pipelines.md' }),
      actions: [
        iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => refresh() }),
        button(t('mp.create'), { variant: 'primary', icon: 'plus', testid: 'mp-create', kbd: 'N', onClick: create }),
      ],
    }),
    h('section', { class: 'panel' },
      h('div', { class: 'table-toolbar' }, search, roomSel),
      box));
  await refresh();
  const offs = [
    onPageKey('n', create),
    listKeys(box),
    refreshOn(['media_pipeline.', 'room.deleted'], () => refresh()),
  ];
  // 実行の状態（status）はイベントにならないので、見えている間は 5 秒ごとに読む
  // 変わったときだけ描き直し、フォーカスのあった要素（data-testid）へ戻す
  const silent = async () => {
    try {
      const list = await listAll(mediaPipelines.list, { room: room || undefined });
      if (JSON.stringify(list) === JSON.stringify(items) || !tableEl || !tableEl.isConnected) return;
      const focused = document.activeElement && box.contains(document.activeElement) ? document.activeElement.dataset.testid : null;
      items = list;
      const n = draw();
      tableEl.replaceWith(n);
      tableEl = n;
      if (focused) { const el = box.querySelector(`[data-testid="${CSS.escape(focused)}"]`); if (el) el.focus({ preventScroll: true }); }
    } catch { /* 次の回に読む */ }
  };
  const timer = setInterval(() => { if (document.visibilityState === 'visible' && items.length) silent(); }, 5000);
  return () => { offs.forEach((off) => off()); clearInterval(timer); };
}

