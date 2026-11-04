// 配信の一覧（カーソルでページ送り・名前で絞り込み・並べ替え）と作成。#/streams?new=1 で作成のダイアログを開く。

import { h, replace } from '../lib/dom.js';
import { load, modal, reportError, errorBox } from '../lib/ui.js';
import { streams } from '../api/resources.js';
import { fmtTime, fmtBps, idemKey } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { streamForm } from './stream-form.js';
import { stashSecret } from '../lib/secrets.js';
import { t } from '../i18n/index.js';
import { button, iconButton, pageHeader, dataTable, pagination, emptyState, statusPill, searchBox, helpHint } from '../ui/index.js';
import { invalidatePalette } from '../palette.js';
import { onPageKey } from '../shortcuts.js';

const PAGE = 50;

/** 配信の状態（観測した状態と、望む状態）→ 表示の状態。 */
export function streamState(s) {
  if (s.observed_state === 'live') return 'live';
  if (s.observed_state === 'stopped' || s.desired_state === 'stopped') return 'stopped';
  return 'waiting';
}

export function stateBadge(s) {
  return statusPill(streamState(s));
}

/** 作成のダイアログ。作ったら Stream を返す。 */
export function createStreamDialog(caps) {
  const form = streamForm(null, caps);
  const errBox = h('div');
  // 同じ入力の再送で二重に作らないよう、ダイアログごとに 1 つの鍵
  const key = idemKey();
  return modal(t('streams.createTitle'), () => [form.el, errBox], {
    wide: true,
    icon: 'streams',
    testid: 'stream-create-dialog',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'stream-create-submit', onClick: async () => {
        const body = form.value();
        if (!body) return;
        ok.disabled = true;
        replace(errBox);
        try {
          const { data } = await streams.create(body, key);
          invalidatePalette();
          close(data);
        } catch (e) {
          replace(errBox, errorBox(e, { title: t('streams.createFailed') }));
        } finally {
          ok.disabled = false;
        }
      } }, t('common.create'));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  });
}

export async function render(root, _params, { caps, navigate }) {
  const box = h('div');
  const pager = h('div');
  const cursors = [undefined];
  let page = 0;
  let filter = '';
  let items = [];

  const create = async () => {
    const s = await createStreamDialog(caps);
    if (!s) return;
    if (s.ingest && s.ingest.stream_key) stashSecret(s.id, s.ingest.stream_key);
    navigate(`/streams/${encodeURIComponent(s.id)}`);
  };

  const columns = [
    { key: 'name', label: t('common.name'), primary: true, sort: (s) => s.name, render: (s) => h('a', { href: `#/streams/${encodeURIComponent(s.id)}` }, s.name) },
    { key: 'state', label: t('common.state'), sort: (s) => ({ live: 0, waiting: 1, stopped: 2 }[streamState(s)]), render: stateBadge },
    { key: 'latency', label: t('streams.col.latency'), sort: (s) => s.latency_profile, render: (s) => s.latency_profile },
    { key: 'codec', label: t('streams.col.codec'), sort: (s) => s.codec_profile, render: (s) => s.codec_profile },
    { key: 'hls', label: 'HLS', render: (s) => s.hls.container },
    { key: 'rec', label: t('streams.col.recording'), render: (s) => (s.recording.enabled ? t('streams.recAuto') : '—') },
    { key: 'bitrate', label: t('streams.col.bitrate'), class: 'num', sort: (s) => (s.live ? s.live.bitrate_bps : null), render: (s) => (s.live ? fmtBps(s.live.bitrate_bps) : '—') },
    { key: 'updated', label: t('common.updated'), sort: (s) => s.updated_at, render: (s) => fmtTime(s.updated_at) },
  ];

  const draw = () => {
    const q = filter.trim().toLowerCase();
    const rows = q ? items.filter((s) => s.name.toLowerCase().includes(q) || s.id.includes(q)) : items;
    if (!items.length && page === 0) {
      return emptyState({
        icon: 'streams', title: t('streams.emptyTitle'), body: t('streams.emptyBody'), testid: 'streams-empty',
        actions: [button(t('streams.create'), { variant: 'primary', icon: 'plus', onClick: () => create().catch((e) => reportError(e, t('common.create'))) })],
      });
    }
    return dataTable({
      columns, rows, caption: t('streams.caption'), sort: { key: 'state', dir: 'asc' }, rowKey: (s) => s.id, testid: 'streams-table',
      empty: { icon: 'search', title: t('streams.noMatch', { q: filter }) },
    });
  };

  let tableEl = null;
  const refresh = () => load(box, async () => {
    const { data } = await streams.list({ limit: PAGE, cursor: cursors[page] });
    cursors[page + 1] = data.next_cursor || null;
    items = data.items;
    replace(pager, pagination({
      page, hasPrev: page > 0, hasNext: !!data.next_cursor,
      onPrev: () => { page -= 1; refresh(); }, onNext: () => { page += 1; refresh(); },
    }));
    tableEl = draw();
    return tableEl;
  }, { kind: 'table' });

  const search = searchBox({
    placeholder: t('streams.filter'), shortcut: true, testid: 'streams-filter',
    onInput: (v) => { filter = v; if (tableEl) { const n = draw(); tableEl.replaceWith(n); tableEl = n; } },
  });

  root.append(
    pageHeader({
      title: t('nav.streams'),
      description: t('streams.lead'),
      help: helpHint({ title: t('nav.streams'), body: t('streams.help'), guide: '04-ingest-playback.md' }),
      actions: [
        iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => refresh() }),
        button(t('streams.create'), { variant: 'primary', icon: 'plus', testid: 'streams-create', kbd: 'N', onClick: () => create().catch((e) => reportError(e, t('common.create'))) }),
      ],
    }),
    h('section', { class: 'panel' },
      h('div', { class: 'table-toolbar' }, search),
      box, pager));
  await refresh();
  // コマンドパレットの「新しい配信を作成」（#/streams?new=1）
  if (new URLSearchParams(location.hash.split('?')[1] || '').get('new') === '1') {
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/streams`);
    create().catch((e) => reportError(e, t('common.create')));
  }
  const offKey = onPageKey('n', () => create().catch((err) => reportError(err, t('common.create'))));
  return () => { off(); offKey(); };
}
