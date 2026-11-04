// 録画の一覧。stream で絞る（API の stream クエリ）。state はノードが絞り込まないので画面で絞る。

import { h, replace, select } from '../lib/dom.js';
import { load } from '../lib/ui.js';
import { recordings, streams, listAll } from '../api/resources.js';
import { fmtTime, fmtDuration, fmtBytes } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t } from '../i18n/index.js';
import { iconButton, pageHeader, dataTable, pagination, emptyState, statusPill, badge, formField, helpHint } from '../ui/index.js';

const PAGE = 50;

export function recBadge(r) {
  if (r.state === 'recording') return statusPill('recording');
  if (r.state === 'interrupted') return statusPill('interrupted');
  return statusPill('finalized');
}

export async function render(root) {
  const box = h('div');
  const pager = h('div');
  const params = new URLSearchParams(location.hash.split('?')[1] || '');
  const streamSel = select([['', t('recordings.allStreams')]], params.get('stream') || '');
  const stateSel = select([['', t('recordings.allStates')], ['recording', t('status.recording')], ['finalized', t('status.finalized')], ['interrupted', t('status.interrupted')]], '');
  const names = new Map();
  let cursors = [undefined];
  let page = 0;

  try {
    const all = await listAll(streams.list);
    for (const s of all) {
      names.set(s.id, s.name);
      streamSel.appendChild(h('option', { value: s.id, selected: s.id === params.get('stream') }, s.name));
    }
  } catch { /* 名前が出ないだけ */ }

  const refresh = () => load(box, async () => {
    const { data } = await recordings.list({ limit: PAGE, cursor: cursors[page], stream: streamSel.value || undefined });
    cursors[page + 1] = data.next_cursor || null;
    const items = stateSel.value ? data.items.filter((r) => r.state === stateSel.value) : data.items;
    replace(pager, pagination({
      page, hasPrev: page > 0, hasNext: !!data.next_cursor,
      onPrev: () => { page -= 1; refresh(); }, onNext: () => { page += 1; refresh(); },
      note: stateSel.value ? t('recordings.filteredNote', { n: data.items.length }) : '',
    }));
    const filtered = !!(streamSel.value || stateSel.value);
    return dataTable({
      caption: t('recordings.caption'),
      sort: { key: 'started', dir: 'desc' },
      rowKey: (r) => r.id,
      columns: [
        { key: 'name', label: t('recordings.col.recording'), primary: true, sort: (r) => r.name || r.started_at, render: (r) => h('a', { href: `#/recordings/${encodeURIComponent(r.id)}` }, r.name || fmtTime(r.started_at)) },
        { key: 'stream', label: t('recordings.col.stream'), sort: (r) => names.get(r.stream) || r.stream, render: (r) => (names.has(r.stream) ? h('a', { href: `#/streams/${encodeURIComponent(r.stream)}` }, names.get(r.stream)) : h('code', { class: 'muted' }, r.stream.slice(0, 10))) },
        { key: 'state', label: t('common.state'), sort: (r) => r.state, render: recBadge },
        { key: 'started', label: t('recordings.col.started'), sort: (r) => r.started_at, render: (r) => fmtTime(r.started_at) },
        { key: 'dur', label: t('recordings.col.duration'), class: 'num', sort: (r) => r.duration_ms, render: (r) => fmtDuration(r.duration_ms) },
        { key: 'size', label: t('recordings.col.size'), class: 'num', sort: (r) => r.bytes, render: (r) => fmtBytes(r.bytes) },
        { label: t('recordings.col.gaps'), class: 'num', render: (r) => (r.gaps && r.gaps.length ? badge(String(r.gaps.length), 'warn') : '0') },
        { label: t('recordings.col.hold'), render: (r) => (r.legal_hold ? badge(t('recordings.legalHold'), 'accent', { icon: 'shield' }) : '—') },
        { key: 'until', label: t('recordings.col.retention'), sort: (r) => r.retention_until || '9999', render: (r) => (r.retention_until ? fmtTime(r.retention_until) : t('common.unlimited')) },
      ],
      rows: items,
      empty: filtered
        ? { icon: 'filter', title: t('recordings.noMatch') }
        : { icon: 'recordings', title: t('recordings.emptyTitle'), body: t('recordings.emptyBody'), actions: [h('a', { class: 'btn primary', href: '#/streams' }, t('recordings.toStreams'))] },
    });
  }, { kind: 'table' });

  const reset = () => { cursors = [undefined]; page = 0; refresh(); };
  streamSel.addEventListener('change', reset);
  stateSel.addEventListener('change', reset);

  root.append(
    pageHeader({
      title: t('nav.recordings'),
      description: t('recordings.lead'),
      help: helpHint({ title: t('nav.recordings'), body: t('recordings.help'), guide: '05-recording.md' }),
      actions: [iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => refresh() })],
    }),
    h('section', { class: 'panel' },
      h('div', { class: 'toolbar form' }, formField({ label: t('recordings.col.stream'), input: streamSel }), formField({ label: t('common.state'), input: stateSel })),
      box, pager));
  await refresh();
  return refreshOn(['recording.'], refresh);
}
