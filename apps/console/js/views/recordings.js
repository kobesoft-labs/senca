// 録画の一覧: 代表画・配信・状態・開始・長さ・大きさ・保持（法的保全・期限）。その場で再生、MP4 の書き出しもここからできる。
//
// - 絞り込み: 配信（API の stream クエリ）・状態・期間・名前。状態・期間・名前はノードが絞り込まないので、
//   読んだ分（新しい順に最大 1000 件）を画面で絞る。条件はすべて URL（?stream=・?state=・?from=・?to=・?q=）に残す。
// - キーボード: J/K（↑↓）で行を選び Enter で開く、/ で検索。

import { h, replace, select } from '../lib/dom.js';
import { load, modal, reportError } from '../lib/ui.js';
import { recordings, streams, listAll } from '../api/resources.js';
import { fmtTime, fmtHm, fmtDuration, fmtBytes, fmtDate, fmtRelative, fmtNumber, utcToWall, wallToUtcIso, localZone } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t } from '../i18n/index.js';
import {
  iconButton, pageHeader, dataTable, emptyState, statusPill, badge, formField, helpHint, segmented, searchBox, button, listNav, tooltip,
} from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { playerWidget } from '../player/widget.js';
import { recordingThumb, hashParams, setHashParams } from '../lib/streaming.js';
import { exportRecording } from './recording-detail.js';

const MAX = 1000;

export function recBadge(r) {
  if (r.state === 'recording') return statusPill('recording');
  if (r.state === 'interrupted') return statusPill('interrupted');
  return statusPill('finalized');
}

/** 録画の題（名前が無ければ開始の時刻）。 */
export function recTitle(r) {
  return r.name || t('recordings.titleAt', { time: fmtTime(r.started_at) });
}

/** 保持の表示: 法的保全・期限（近ければ注意の色）・無期限。 */
export function retentionCell(r) {
  if (r.legal_hold) return badge(t('recordings.legalHold'), 'accent', { icon: 'shield', title: t('recordings.holdHint') });
  if (!r.retention_until) return h('span', { class: 'subtle', title: t('recordings.keepForeverTitle') }, t('common.unlimited'));
  const left = new Date(r.retention_until).getTime() - Date.now();
  const soon = left < 3 * 86400_000;
  return h('span', { class: soon ? 'text-warn' : null, title: fmtTime(r.retention_until) }, icon('clock', { size: 'sm' }), ' ', fmtRelative(r.retention_until));
}

/** 録画の小さな画（代表画、無ければアイコン）。 */
export function recThumb(r, size = 'md') {
  const el = h('div', { class: `thumb ${size}`, 'data-state': r.state === 'recording' ? 'live' : 'idle', 'aria-hidden': 'true' }, icon(r.state === 'recording' ? 'record' : 'recordings'));
  if (r.thumbnail) {
    recordingThumb(r.id).then((u) => { if (u) { el.prepend(h('img', { src: u, alt: '' })); el.classList.add('has-img'); } });
  }
  if (r.duration_ms) el.append(h('span', { class: 'thumb-dur num' }, fmtDuration(r.duration_ms)));
  return el;
}

/** その場で再生するダイアログ。 */
export function playDialog(r) {
  const player = playerWidget({ getUrl: async () => (await recordings.playbackUrl(r.id)).data.playback.hls, live: false, label: t('recordings.playerLabel'), autostart: true });
  return modal(recTitle(r), () => [player.el, h('p', { class: 'small muted mt-2' }, t('recordings.vodNote'))], {
    wide: true, icon: 'play', testid: 'rec-play-dialog',
    actions: (close) => [h('a', { class: 'btn', href: `#/recordings/${encodeURIComponent(r.id)}`, onClick: () => close() }, t('recordings.openDetail'), icon('arrow-right')), button(t('common.close'), { variant: 'primary', onClick: () => close() })],
  }).finally(() => player.stop());
}

export async function render(root, _p, { navigate }) {
  const params = hashParams();
  const zone = localZone();
  const box = h('div');
  const note = h('p', { class: 'small muted list-note', 'aria-live': 'polite' });
  const streamSel = select([['', t('recordings.allStreams')]], params.get('stream') || '', { 'data-testid': 'rec-filter-stream' });
  let stateF = ['recording', 'finalized', 'interrupted', 'held'].includes(params.get('state')) ? params.get('state') : '';
  const from = h('input', { type: 'date', value: params.get('from') || '', 'data-testid': 'rec-filter-from' });
  const to = h('input', { type: 'date', value: params.get('to') || '', 'data-testid': 'rec-filter-to' });
  let query = params.get('q') || '';
  const names = new Map();
  let items = [];
  let truncated = false;

  try {
    const all = await listAll(streams.list);
    for (const s of all) {
      names.set(s.id, s.name);
      streamSel.appendChild(h('option', { value: s.id, selected: s.id === params.get('stream') }, s.name));
    }
    // 消した配信の録画で選ばれていたら、ID のまま選べるようにする
    if (params.get('stream') && !names.has(params.get('stream'))) streamSel.appendChild(h('option', { value: params.get('stream'), selected: true }, params.get('stream').slice(0, 10)));
  } catch { /* 名前が出ないだけ */ }

  const stateSeg = segmented({
    label: t('common.state'), testid: 'rec-filter-state', value: stateF, size: 'sm',
    options: [['', t('common.all')], ['recording', t('status.recording')], ['finalized', t('status.finalized')], ['interrupted', t('status.interrupted')], ['held', t('recordings.legalHold')]],
    onChange: (v) => { stateF = v; setHashParams({ state: v || null }); draw(); },
  });
  const search = searchBox({ placeholder: t('recordings.searchPh'), shortcut: true, testid: 'rec-filter-q', value: query, onInput: (v) => { query = v; setHashParams({ q: v.trim() || null }); draw(); } });

  const range = () => ({
    from: from.value ? new Date(wallToUtcIso(`${from.value}T00:00`, zone)).getTime() : null,
    to: to.value ? new Date(wallToUtcIso(`${to.value}T00:00`, zone)).getTime() + 86400_000 : null,
  });
  const visible = () => {
    const { from: f, to: tt } = range();
    const q = query.trim().toLowerCase();
    return items.filter((r) => {
      if (stateF === 'held' ? !r.legal_hold : stateF && r.state !== stateF) return false;
      const st = new Date(r.started_at).getTime();
      if (f !== null && st < f) return false;
      if (tt !== null && st >= tt) return false;
      if (q && !((r.name || '').toLowerCase().includes(q) || (names.get(r.stream) || '').toLowerCase().includes(q) || r.id.includes(q))) return false;
      return true;
    });
  };
  const filtered = () => !!(streamSel.value || stateF || from.value || to.value || query.trim());

  const clear = () => {
    streamSel.value = '';
    stateF = '';
    stateSeg.set('');
    from.value = '';
    to.value = '';
    query = '';
    search.input.value = '';
    setHashParams({ stream: null, state: null, from: null, to: null, q: null });
    refresh();
  };

  const columns = [
    {
      key: 'name', label: t('recordings.col.recording'), primary: true, sort: (r) => r.name || r.started_at,
      render: (r) => h('div', { class: 'stream-cell' }, recThumb(r),
        h('div', { class: 'sc-text' },
          h('a', { class: 'sc-name', href: `#/recordings/${encodeURIComponent(r.id)}` }, r.name || names.get(r.stream) || fmtTime(r.started_at)),
          h('div', { class: 'sc-sub' },
            r.name ? (names.has(r.stream) ? names.get(r.stream) : h('code', null, r.stream.slice(0, 10))) : fmtTime(r.started_at),
            r.gaps && r.gaps.length ? [' · ', h('span', { class: 'text-warn' }, t('recordings.gapsN', { n: r.gaps.length }))] : null),
          h('div', { class: 'sc-mobile' }, recBadge(r), h('span', { class: 'num' }, fmtDuration(r.duration_ms)), h('span', { class: 'num' }, fmtBytes(r.bytes)), r.legal_hold ? badge(t('recordings.legalHold'), 'accent', { icon: 'shield' }) : null))),
    },
    { key: 'state', label: t('common.state'), class: 'hide-sm', sort: (r) => r.state, render: recBadge },
    { key: 'started', label: t('recordings.col.started'), class: 'hide-sm', sort: (r) => r.started_at, render: (r) => h('span', { title: fmtTime(r.started_at) }, fmtDate(r.started_at), ' ', h('span', { class: 'muted small' }, fmtHm(r.started_at))) },
    { key: 'dur', label: t('recordings.col.duration'), class: 'num hide-sm', sort: (r) => r.duration_ms, render: (r) => fmtDuration(r.duration_ms) },
    { key: 'size', label: t('recordings.col.size'), class: 'num hide-sm', sort: (r) => r.bytes, render: (r) => fmtBytes(r.bytes) },
    { key: 'until', label: t('recordings.col.retention'), class: 'hide-sm', sort: (r) => (r.legal_hold ? '0' : r.retention_until || '9999'), render: retentionCell },
    {
      key: 'actions', label: t('common.actions'), actions: true,
      render: (r) => h('div', { class: 'row' },
        iconButton('play', t('recordings.playFor', { name: recTitle(r) }), { size: 'sm', testid: 'rec-row-play', onClick: () => playDialog(r) }),
        iconButton('download', t('recordings.downloadFor', { name: recTitle(r) }), {
          size: 'sm', testid: 'rec-row-download', attrs: { disabled: r.state === 'recording' || !r.bytes },
          onClick: () => exportRecording(r).catch((e) => reportError(e, t('recordings.export'))),
        })),
    },
  ];

  const draw = () => {
    const rows = visible();
    note.textContent = truncated ? t('recordings.truncated', { n: fmtNumber(MAX) }) : filtered() && items.length ? t('recordings.showing', { n: fmtNumber(rows.length), total: fmtNumber(items.length) }) : '';
    if (!items.length && !streamSel.value) {
      replace(box, emptyState({
        icon: 'recordings', title: t('recordings.emptyTitle'), body: t('recordings.emptyBody'), testid: 'recordings-empty',
        actions: [h('a', { class: 'btn primary', href: '#/streams' }, t('recordings.toStreams'))],
      }));
      return;
    }
    if (!rows.length) {
      replace(box, emptyState({ compact: true, icon: 'filter', title: t('recordings.noMatch'), body: t('recordings.noMatchBody'), testid: 'recordings-nomatch', actions: [button(t('streams.clearFilters'), { size: 'sm', icon: 'close', onClick: clear })] }));
      return;
    }
    replace(box, dataTable({ caption: t('recordings.caption'), sort: { key: 'started', dir: 'desc' }, rowKey: (r) => r.id, columns, rows, testid: 'recordings-table' }));
  };

  const refresh = () => load(box, async () => {
    const out = [];
    let cursor;
    do {
      const { data } = await recordings.list({ limit: 200, cursor, stream: streamSel.value || undefined });
      out.push(...data.items);
      cursor = data.next_cursor;
    } while (cursor && out.length < MAX);
    truncated = !!cursor;
    items = out;
    draw();
    return [...box.childNodes];
  }, { kind: 'table' });

  streamSel.addEventListener('change', () => { setHashParams({ stream: streamSel.value || null }); refresh(); });
  for (const el of [from, to]) el.addEventListener('change', () => { setHashParams({ from: from.value || null, to: to.value || null }); draw(); });
  const quick = (days) => {
    const now = Date.now();
    from.value = days ? utcToWall(now - (days - 1) * 86400_000, zone).slice(0, 10) : '';
    to.value = days ? utcToWall(now, zone).slice(0, 10) : '';
    setHashParams({ from: from.value || null, to: to.value || null });
    draw();
  };
  const quickMenu = h('div', { class: 'quick-range', role: 'group', 'aria-label': t('recordings.quickRange') },
    button(t('recordings.today'), { size: 'sm', variant: 'ghost', onClick: () => quick(1) }),
    button(t('recordings.last7'), { size: 'sm', variant: 'ghost', onClick: () => quick(7) }),
    button(t('recordings.last30'), { size: 'sm', variant: 'ghost', onClick: () => quick(30) }));

  const legend = h('div', { class: 'legend small muted' },
    h('span', null, badge(t('recordings.legalHold'), 'accent', { icon: 'shield' }), ' ', t('recordings.legendHold')),
    h('span', null, icon('clock', { size: 'sm' }), ' ', t('recordings.legendUntil')),
    helpHint({ title: t('recordings.col.retention'), body: t('recordings.retentionHelp'), guide: '05-recording.md' }));
  tooltip(legend.querySelector('.badge'), t('recordings.holdHint'));

  root.append(
    pageHeader({
      title: t('nav.recordings'),
      description: t('recordings.lead'),
      help: helpHint({ title: t('nav.recordings'), body: t('recordings.help'), guide: '05-recording.md' }),
    }),
    h('section', { class: 'panel' },
      h('div', { class: 'filters' },
        h('div', { class: 'filters-row' }, search, stateSeg, iconButton('refresh', t('common.refresh'), { variant: 'secondary', class: 'refresh', onClick: () => refresh() })),
        h('div', { class: 'filters-row' },
          formField({ label: t('recordings.col.stream'), input: streamSel }),
          h('div', { class: 'field date-range' }, h('span', { class: 'label' }, t('recordings.period')),
            h('div', { class: 'inline' }, from, h('span', { class: 'muted', 'aria-hidden': 'true' }, '–'), to)),
          quickMenu)),
      legend,
      note,
      box));
  from.setAttribute('aria-label', t('recordings.from'));
  to.setAttribute('aria-label', t('recordings.to'));
  await refresh();
  const offs = [
    refreshOn(['recording.'], refresh),
    listNav(box, { rows: () => [...box.querySelectorAll('tbody tr')], open: (tr) => navigate(`/recordings/${encodeURIComponent(tr.dataset.key)}`) }),
  ];
  return () => offs.forEach((off) => off());
}
