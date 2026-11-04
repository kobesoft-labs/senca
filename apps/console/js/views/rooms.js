// 部屋の一覧（参加者の数を生で）と作成。#/rooms?new=1 で作成のダイアログを開く。
//
// 一覧の API は参加者の数を持たないので、見えている部屋ごとに GET /rooms/{id}/participants を読む（同時に 6 本まで）。
// 部屋の中の出来事（room.connection.* など。参加者がいるノードの SSE にだけ流れる）で読み直し、見えている間は 8 秒ごとにも読む。

import { h, replace } from '../lib/dom.js';
import { load, modal, reportError, errorBox, confirmDialog, toast, copyText } from '../lib/ui.js';
import { rooms, mediaPipelines } from '../api/realtime.js';
import { listAll } from '../api/resources.js';
import { fmtTime, fmtRelative, fmtNumber, idemKey } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t, tn } from '../i18n/index.js';
import { button, iconButton, pageHeader, dataTable, emptyState, statusPill, searchBox, helpHint, badge, statTile, alert, attachMenu, listNav } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { guideUrl } from '../lib/guide.js';
import { invalidatePalette } from '../palette.js';
import { onPageKey } from '../shortcuts.js';
import { forgetRecent } from '../lib/recent.js';
import { roomState, roomFeatures } from '../lib/realtime.js';
import { roomForm } from './room-form.js';
import { tokenDialog } from './room-token.js';

const STATE_PILL = {
  active: ['running', 'rooms.state.active'],
  empty: ['idle', 'rooms.state.empty'],
  pending: ['pending', 'rooms.state.pending'],
  unschedulable: ['failed', 'rooms.state.unschedulable'],
  unknown: ['unknown', 'rooms.state.unknown'],
};

/** 部屋の状態のピル（count は参加者の数、分からなければ null）。 */
export function roomPill(room, count, testid) {
  const st = roomState(room, count);
  const [kind, key] = STATE_PILL[st];
  const label = st === 'active' ? tn('rooms.state.activeN', count) : t(key);
  return statusPill(kind, { label, testid });
}

const FEATURE = {
  e2ee: ['lock', 'rooms.feature.e2ee'],
  cascade: ['grid', 'rooms.feature.cascade'],
  spotlight: ['zap', 'rooms.feature.spotlight'],
  'recording.per_participant': ['record', 'rooms.feature.recPer'],
  'recording.composite': ['record', 'rooms.feature.recComposite'],
  filter: ['filter', 'rooms.feature.filter'],
  webhook: ['shield', 'rooms.feature.webhook'],
};

/** 設定のバッジ（コーデック＋既定から変えたもの）。 */
export function roomBadges(room) {
  const s = room.settings || {};
  return h('div', { class: 'badge-row' },
    h('span', { class: 'tag' }, (s.video_codec || 'vp8').toUpperCase()),
    roomFeatures(s).map((f) => badge(t(FEATURE[f][1]), 'outline', { icon: FEATURE[f][0] })),
    room.auto ? badge(t('rooms.feature.auto'), 'info', { title: t('rooms.feature.autoTitle') }) : null);
}

export function roomTitle(room) {
  const m = room.metadata;
  return m && typeof m === 'object' && typeof m.title === 'string' && m.title.trim() ? m.title.trim() : null;
}

/** 作成のダイアログ。作ったら Room を返す。 */
export function createRoomDialog() {
  const form = roomForm(null);
  const errBox = h('div');
  const key = idemKey();
  return modal(t('rooms.createTitle'), () => [
    h('p', { class: 'muted small mb-0' }, t('rooms.createLead')),
    form.el, errBox,
  ], {
    wide: true, icon: 'rooms', testid: 'room-create-dialog', initialFocus: '[data-testid="room-id"]',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'room-create-submit', onClick: async () => {
        const body = form.value();
        if (!body) return;
        ok.disabled = true;
        replace(errBox);
        try {
          const { data } = await rooms.create(body, key);
          invalidatePalette();
          close(data);
        } catch (e) {
          const title = e.status === 409 ? t('rooms.createConflict') : t('rooms.createFailed');
          replace(errBox, errorBox(e, { title }));
        } finally {
          ok.disabled = false;
        }
      } }, t('rooms.create'));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  });
}

/** 削除の確認と実行。消したら true。 */
export async function deleteRoom(room, count) {
  const ok = await confirmDialog(t('rooms.deleteTitle'), t('rooms.deleteBody', { id: room.id }), {
    ok: t('common.delete'), requireText: room.id, testid: 'room-delete-dialog',
    consequences: [count ? tn('rooms.deleteC1n', count) : t('rooms.deleteC1'), t('rooms.deleteC2'), t('rooms.deleteC3')],
  });
  if (!ok) return false;
  try {
    await rooms.remove(room.id);
    forgetRecent(`#/rooms/${encodeURIComponent(room.id)}`);
    invalidatePalette();
    toast(t('rooms.deleted', { id: room.id }), 'ok');
    return true;
  } catch (e) {
    reportError(e, t('common.delete'));
    return false;
  }
}

/** 同時に n 本までで map する。 */
async function pool(items, n, fn) {
  const out = new Array(items.length);
  let i = 0;
  await Promise.all(Array.from({ length: Math.min(n, items.length) }, async () => {
    while (i < items.length) { const k = i++; out[k] = await fn(items[k]); }
  }));
  return out;
}

export async function render(root, _params, { caps, navigate }) {
  const box = h('div');
  const tiles = h('div', { class: 'grid cards stat-grid', 'data-testid': 'rooms-stats' });
  let items = [];
  let live = new Map(); // id → {n, video, audio} | null（読めない）
  let mps = [];
  let filter = '';
  let tableEl = null;
  let timer = null;

  const create = async () => {
    const r = await createRoomDialog();
    if (!r) return;
    toast(t('rooms.created', { id: r.id }), 'ok');
    navigate(`/rooms/${encodeURIComponent(r.id)}`);
  };
  const createSafe = () => create().catch((e) => reportError(e, t('common.create')));

  const href = (r) => `#/rooms/${encodeURIComponent(r.id)}`;
  const count = (r) => (live.has(r.id) && live.get(r.id) ? live.get(r.id).n : null);
  const mpOf = (r) => mps.filter((m) => m.room === r.id);

  const rowMenu = (r) => {
    const more = iconButton('more', t('rooms.actionsFor', { id: r.id }), { testid: `room-more-${r.id}` });
    attachMenu(more, () => [
      { label: t('rooms.open'), icon: 'arrow-right', onSelect: () => navigate(`/rooms/${encodeURIComponent(r.id)}`) },
      { label: t('rooms.issueToken'), icon: 'user-plus', onSelect: () => tokenDialog(r) },
      { label: t('rooms.newMediaPipeline'), icon: 'media-pipelines', onSelect: () => navigate(`/media-pipelines/new?room=${encodeURIComponent(r.id)}`) },
      { label: t('rooms.copyId'), icon: 'copy', onSelect: () => copyText(r.id, t('rooms.id')) },
      { separator: true },
      { label: t('common.delete'), icon: 'trash', danger: true, onSelect: async () => { if (await deleteRoom(r, count(r))) refresh(); } },
    ]);
    return more;
  };

  const columns = [
    { key: 'id', label: t('rooms.col.room'), primary: true, sort: (r) => r.id, render: (r) => h('div', { class: 'cell-stack' },
      h('a', { href: href(r), class: 'strong' }, roomTitle(r) || r.id),
      roomTitle(r) ? h('span', { class: 'small muted mono' }, r.id) : null) },
    { key: 'state', label: t('common.state'), sort: (r) => ({ active: 0, empty: 1, pending: 2, unknown: 3, unschedulable: 4 }[roomState(r, count(r))]), render: (r) => roomPill(r, count(r), `room-state-${r.id}`) },
    { key: 'people', label: t('rooms.col.participants'), class: 'num', sort: (r) => count(r), render: (r) => h('span', { 'data-testid': `room-count-${r.id}` },
      count(r) === null ? '—' : `${fmtNumber(count(r))} / ${fmtNumber(r.settings.max_participants)}`) },
    { key: 'tracks', label: t('rooms.col.publishing'), render: (r) => {
      const l = live.get(r.id);
      if (!l || (!l.video && !l.audio)) return h('span', { class: 'muted' }, '—');
      return h('span', { class: 'track-counts' },
        l.video ? h('span', { title: tn('rooms.videoTracks', l.video) }, icon('send', { size: 'sm' }), String(l.video)) : null,
        l.audio ? h('span', { title: tn('rooms.audioTracks', l.audio) }, icon('mic', { size: 'sm' }), String(l.audio)) : null);
    } },
    { key: 'settings', label: t('rooms.col.settings'), wrap: true, render: roomBadges },
    { key: 'mp', label: t('rooms.col.mediaPipelines'), sort: (r) => mpOf(r).length, render: (r) => {
      const list = mpOf(r);
      if (!list.length) return h('span', { class: 'muted' }, '—');
      const running = list.filter((m) => m.desired_state === 'running').length;
      return h('a', { href: `#/media-pipelines?room=${encodeURIComponent(r.id)}`, class: 'row nowrap' },
        running ? h('span', { class: 'dot ok', 'aria-hidden': 'true' }) : null, tn('rooms.mpCount', list.length));
    } },
    { key: 'updated', label: t('common.updated'), sort: (r) => r.updated_at, render: (r) => h('span', { title: fmtTime(r.updated_at) }, fmtRelative(r.updated_at)) },
    { key: 'actions', label: t('common.actions'), actions: true, render: (r) => h('div', { class: 'row-actions' },
      iconButton('user-plus', t('rooms.issueToken'), { testid: `room-token-${r.id}`, onClick: () => tokenDialog(r) }),
      rowMenu(r)) },
  ];

  const drawTiles = () => {
    const known = [...live.values()].filter(Boolean);
    const people = known.reduce((a, l) => a + l.n, 0);
    const tracks = known.reduce((a, l) => a + l.video + l.audio, 0);
    const activeRooms = known.filter((l) => l.n > 0).length;
    const runningMp = mps.filter((m) => m.desired_state === 'running').length;
    replace(tiles,
      statTile({ label: t('rooms.tile.rooms'), value: fmtNumber(items.length), sub: tn('rooms.tile.roomsActive', activeRooms), icon: 'rooms' }),
      statTile({ label: t('rooms.tile.participants'), value: fmtNumber(people), sub: t('rooms.tile.participantsSub'), icon: 'users', testid: 'rooms-tile-participants' }),
      statTile({ label: t('rooms.tile.tracks'), value: fmtNumber(tracks), sub: t('rooms.tile.tracksSub'), icon: 'live' }),
      statTile({ label: t('rooms.tile.mp'), value: fmtNumber(runningMp), sub: tn('rooms.tile.mpSub', mps.length), icon: 'media-pipelines', href: '#/media-pipelines' }));
  };

  const draw = () => {
    toolbar.hidden = !items.length;
    if (!items.length) {
      tiles.hidden = true;
      return h('div', { 'data-testid': 'rooms-empty' },
        emptyState({
          icon: 'rooms', title: t('rooms.emptyTitle'),
          body: h('span', null, t('rooms.emptyBody')),
          actions: [
            button(t('rooms.create'), { variant: 'primary', icon: 'plus', onClick: createSafe }),
            h('a', { class: 'btn', href: guideUrl('10-rooms.md'), target: '_blank', rel: 'noopener noreferrer' }, icon('book'), t('rooms.readGuide'), icon('external')),
          ],
        }),
        h('ol', { class: 'how-steps', 'aria-label': t('rooms.how.title') },
          [['rooms', 'rooms.how.1', 'rooms.how.1b'], ['user-plus', 'rooms.how.2', 'rooms.how.2b'], ['code', 'rooms.how.3', 'rooms.how.3b']].map(([ic, a, b], i) => h('li', null,
            h('span', { class: 'how-num', 'aria-hidden': 'true' }, String(i + 1)),
            h('span', { class: 'use-icon sm' }, icon(ic)),
            h('span', { class: 'use-text' }, h('strong', null, t(a)), h('span', { class: 'small muted' }, t(b)))))));
    }
    tiles.hidden = false;
    const q = filter.trim().toLowerCase();
    const rows = q ? items.filter((r) => r.id.toLowerCase().includes(q) || (roomTitle(r) || '').toLowerCase().includes(q)) : items;
    return dataTable({
      columns, rows, caption: t('rooms.caption'), sort: { key: 'state', dir: 'asc' }, rowKey: (r) => r.id, testid: 'rooms-table',
      empty: { icon: 'search', title: t('rooms.noMatch', { q: filter }) },
    });
  };
  const redraw = () => {
    if (!tableEl || !tableEl.isConnected) return;
    const n = draw();
    tableEl.replaceWith(n);
    tableEl = n;
    drawTiles();
  };

  // 参加者の数（配置済みの部屋だけ。見えている一覧の分）
  const readLive = async () => {
    const placed = items.filter((r) => ['running', 'degraded'].includes(r.placement_status && r.placement_status.state));
    const res = await pool(placed, 6, async (r) => {
      try {
        const { data } = await rooms.participants(r.id);
        const tracks = data.items.flatMap((p) => p.tracks || []);
        return [r.id, { n: data.items.length, video: tracks.filter((x) => x.kind === 'video').length, audio: tracks.filter((x) => x.kind === 'audio').length }];
      } catch {
        return [r.id, null];
      }
    });
    live = new Map(res);
  };
  const readMps = async () => {
    try { mps = await listAll(mediaPipelines.list); } catch { mps = []; }
  };

  const refresh = () => load(box, async () => {
    items = await listAll(rooms.list);
    await Promise.all([readLive(), readMps()]);
    tableEl = draw();
    drawTiles();
    return tableEl;
  }, { kind: 'table' });
  const refreshLive = async () => {
    if (!items.length) return;
    await Promise.all([readLive(), readMps()]);
    redraw();
  };

  const search = searchBox({
    placeholder: t('rooms.filter'), shortcut: true, testid: 'rooms-filter',
    onInput: (v) => { filter = v; redraw(); },
  });

  const toolbar = h('div', { class: 'table-toolbar' }, search, h('span', { class: 'live-note small muted' }, h('span', { class: 'pulse', 'aria-hidden': 'true' }), t('rooms.liveNote')));
  const webrtcOff = caps && caps.ingest && caps.ingest.whip === false;
  root.append(...[
    pageHeader({
      title: t('nav.rooms'),
      description: t('rooms.lead'),
      help: helpHint({ title: t('nav.rooms'), body: t('rooms.help'), guide: '10-rooms.md' }),
      actions: [
        iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => refresh() }),
        button(t('rooms.create'), { variant: 'primary', icon: 'plus', testid: 'rooms-create', kbd: 'N', onClick: createSafe }),
      ],
    }),
    webrtcOff ? alert('warn', t('rooms.webrtcOff'), { title: t('rooms.webrtcOffTitle'), testid: 'rooms-webrtc-off' }) : null,
    tiles,
    h('section', { class: 'panel' }, toolbar, box),
  ].filter(Boolean));
  await refresh();
  if (new URLSearchParams(location.hash.split('?')[1] || '').get('new') === '1') {
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/rooms`);
    createSafe();
  }
  const offs = [
    onPageKey('n', createSafe),
    listNav(box, { rows: () => [...box.querySelectorAll('tbody tr')], open: (tr) => navigate(`/rooms/${encodeURIComponent(tr.dataset.key)}`) }),
    refreshOn(['room.updated', 'room.deleted'], () => refresh()),
    refreshOn(['room.connection.', 'room.track.', 'room.started', 'room.stopped', 'media_pipeline.'], refreshLive, { wait: 300 }),
  ];
  timer = setInterval(() => { if (document.visibilityState === 'visible') refreshLive(); }, 8000);
  return () => { offs.forEach((off) => off()); clearInterval(timer); };
}
