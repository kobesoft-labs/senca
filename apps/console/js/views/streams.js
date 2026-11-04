// 配信の一覧: 一目で「どれが配信中か」が分かるように、配信中を先に、今の画・受信の要約・ビットレートの動き・視聴者・録画中を並べる。
//
// - 絞り込み（すべて・配信中・入力待ち・停止）と名前の検索は URL（?state=・?q=）に残す。
// - publish の開始・終了とビットレートは SSE のイベントにならないので、見えている間は 5 秒ごとに読み直す。
//   行の作りが変わらなければ（状態・名前・録画）表は作り直さず、数とグラフだけ差し替える（押そうとしているボタンを奪わない）。
// - キーボード: J/K（↑↓）で行を選び Enter で開く、N で作成、/ で検索。
// - 作成は stream-create.js（はじめての人向けの流れ）。作ったら詳細の「送り手をつなぐ」（?setup=）へ。

import { h, replace } from '../lib/dom.js';
import { load, reportError } from '../lib/ui.js';
import { streams, recordings, listAll } from '../api/resources.js';
import { fmtBps, fmtDuration, fmtNumber, fmtRelative } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { stashSecret } from '../lib/secrets.js';
import { t } from '../i18n/index.js';
import {
  button, iconButton, pageHeader, dataTable, emptyState, statusPill, searchBox, helpHint, segmented, badge, copyText, attachMenu, sparkline, listNav,
} from '../ui/index.js';
import { onPageKey } from '../shortcuts.js';
import { icon } from '../lib/icons.js';
import { createStreamDialog } from './stream-create.js';
import {
  streamState, liveSummary, uptimeMs, viewerCount, recordHealth, healthSamples, liveThumb, hashParams, setHashParams,
} from '../lib/streaming.js';

export { streamState };

export function stateBadge(s) {
  return statusPill(streamState(s));
}

/** 配信の小さな画（配信中なら今の画、無ければ状態のアイコン）。 */
export function streamThumb(s, { size = 'md' } = {}) {
  const state = streamState(s);
  const el = h('div', { class: `thumb ${size}`, 'data-state': state, 'aria-hidden': 'true' },
    icon(state === 'live' ? 'live' : state === 'stopped' ? 'stop' : 'streams'));
  if (state === 'live') {
    liveThumb(s.id).then((url) => {
      if (!url) return;
      const img = h('img', { src: url, alt: '' });
      img.addEventListener('load', () => { el.classList.add('has-img'); });
      el.prepend(img);
    });
  }
  return el;
}

/** 受信の要約の 1 行（配信中: 経路・コーデック・大きさ、そうでなければ状態の説明）。 */
export function streamSubline(s) {
  const st = streamState(s);
  if (st === 'live') return liveSummary(s).text;
  if (st === 'stopped') return t('streams.sub.stopped');
  return t('streams.sub.waiting');
}

const STATE_ORDER = { live: 0, waiting: 1, stopped: 2 };

export async function render(root, _params, { caps, navigate }) {
  const params = hashParams();
  let filter = ['live', 'waiting', 'stopped'].includes(params.get('state')) ? params.get('state') : 'all';
  let query = params.get('q') || '';
  let items = [];
  let recActive = new Set();
  let shapeKey = '';
  let table = null;
  const box = h('div', { class: 'streams-list' });

  const create = async () => {
    const r = await createStreamDialog(caps);
    if (!r) return;
    const s = r.stream;
    if (s.ingest && s.ingest.stream_key) stashSecret(s.id, s.ingest.stream_key);
    navigate(`/streams/${encodeURIComponent(s.id)}?setup=${r.source}`);
  };
  const createBtn = (testid, variant = 'primary') => button(t('streams.create'), {
    variant, icon: 'plus', testid, kbd: testid === 'streams-create' ? 'N' : undefined, onClick: () => create().catch((e) => reportError(e, t('common.create'))),
  });
  const open = (id, tab) => navigate(`/streams/${encodeURIComponent(id)}${tab ? `?tab=${tab}` : ''}`);

  const counts = () => {
    const c = { all: items.length, live: 0, waiting: 0, stopped: 0 };
    for (const s of items) c[streamState(s)] += 1;
    return c;
  };
  const segLabel = (k, n) => [t(`streams.filter.${k}`), h('span', { class: 'seg-count', 'data-count': k }, fmtNumber(n))];
  const seg = segmented({
    label: t('streams.filter.label'), testid: 'streams-state-filter', value: filter,
    options: [['all', segLabel('all', 0)], ['live', segLabel('live', 0), 'live'], ['waiting', segLabel('waiting', 0)], ['stopped', segLabel('stopped', 0)]],
    onChange: (v) => { filter = v; setHashParams({ state: v === 'all' ? null : v }); draw(true); },
  });
  const search = searchBox({
    placeholder: t('streams.filterPh'), shortcut: true, testid: 'streams-filter', value: query,
    onInput: (v) => { query = v; setHashParams({ q: v.trim() || null }); draw(true); },
  });

  const recCell = (s) => {
    if (recActive.has(s.id)) return badge(t('streams.rec.now'), 'rec', { testid: 'rec-now' });
    if (s.recording.enabled) return h('span', { class: 'tag', title: t('streams.rec.autoTitle') }, t('streams.recAuto'));
    return h('span', { class: 'subtle' }, '—');
  };

  const rowMenu = (s) => {
    const b = iconButton('more', t('streams.row.more', { name: s.name }), { size: 'sm', testid: 'stream-row-menu' });
    attachMenu(b, () => [
      { label: t('streams.row.open'), icon: 'arrow-right', onSelect: () => open(s.id) },
      streamState(s) === 'live' ? { label: t('streams.row.previewShort'), icon: 'play', onSelect: () => navigate(`/streams/${encodeURIComponent(s.id)}?play=1`) } : null,
      { label: t('streams.row.connect'), icon: 'upload', onSelect: () => open(s.id, 'connect') },
      { label: t('streams.row.copyRtmp'), icon: 'copy', onSelect: () => copyText(s.ingest.rtmp_url, t('streams.detail.rtmpUrl')) },
      s.ingest.srt_url ? { label: t('streams.row.copySrt'), icon: 'copy', onSelect: () => copyText(s.ingest.srt_url, t('streams.detail.srtUrl')) } : null,
      s.ingest.whip_url ? { label: t('streams.row.sendBrowser'), icon: 'camera', onSelect: () => navigate(`/send?stream=${encodeURIComponent(s.id)}`) } : null,
      { separator: true },
      { label: t('streams.row.recordings'), icon: 'recordings', onSelect: () => navigate(`/recordings?stream=${encodeURIComponent(s.id)}`) },
      { label: t('streams.row.settings'), icon: 'settings', onSelect: () => open(s.id, 'settings') },
    ]);
    return b;
  };

  const columns = [
    {
      key: 'name', label: t('streams.col.stream'), primary: true, sort: (s) => s.name,
      render: (s) => h('div', { class: 'stream-cell' },
        streamThumb(s),
        h('div', { class: 'sc-text' },
          h('a', { class: 'sc-name', href: `#/streams/${encodeURIComponent(s.id)}` }, s.name),
          h('div', { class: 'sc-sub', 'data-live': 'sub' }, streamSubline(s)),
          // 狭い画面のカードで出す 1 行（状態・ビットレート・視聴者・録画）
          h('div', { class: 'sc-mobile' }, stateBadge(s),
            streamState(s) === 'live' ? h('span', { class: 'num', 'data-live': 'bitrate-m' }, fmtBps(s.live.bitrate_bps)) : null,
            recActive.has(s.id) ? badge(t('streams.rec.now'), 'rec') : null))),
    },
    {
      key: 'state', label: t('common.state'), class: 'hide-sm', sort: (s) => STATE_ORDER[streamState(s)] * 1e12 - (uptimeMs(s) || 0),
      render: (s) => h('div', { class: 'state-cell' }, stateBadge(s),
        streamState(s) === 'live' ? h('span', { class: 'uptime num', 'data-live': 'uptime', title: t('streams.col.uptime') }, fmtDuration(uptimeMs(s))) : null),
    },
    {
      key: 'bitrate', label: t('streams.col.bitrate'), class: 'num hide-sm', sort: (s) => (s.live ? s.live.bitrate_bps : null),
      render: (s) => (s.live
        ? h('div', { class: 'br-cell' }, h('span', { 'data-live': 'spark' }, sparkline({ values: healthSamples(s.id), width: 64, height: 20 })), h('span', { 'data-live': 'bitrate' }, fmtBps(s.live.bitrate_bps)))
        : h('span', { class: 'subtle' }, '—')),
    },
    {
      key: 'viewers', label: t('streams.col.viewers'), class: 'num hide-sm', sort: (s) => viewerCount(s),
      render: (s) => (viewerCount(s) !== null ? h('span', { 'data-live': 'viewers' }, fmtNumber(viewerCount(s))) : h('span', { class: 'subtle' }, '—')),
    },
    { key: 'rec', label: t('streams.col.recording'), class: 'hide-sm', sort: (s) => (recActive.has(s.id) ? 0 : s.recording.enabled ? 1 : 2), render: recCell },
    {
      key: 'actions', label: t('common.actions'), actions: true,
      render: (s) => h('div', { class: 'row' },
        iconButton('copy', t('streams.row.copyRtmpFor', { name: s.name }), { size: 'sm', testid: 'stream-copy-ingest', onClick: () => copyText(`${s.ingest.rtmp_url}`, t('streams.detail.rtmpUrl')) }),
        iconButton('play', t('streams.row.preview', { name: s.name }), { size: 'sm', testid: 'stream-row-play', onClick: () => navigate(`/streams/${encodeURIComponent(s.id)}?play=1`), attrs: { disabled: streamState(s) !== 'live' } }),
        rowMenu(s)),
    },
  ];

  const visible = () => {
    const q = query.trim().toLowerCase();
    return items.filter((s) => (filter === 'all' || streamState(s) === filter)
      && (!q || s.name.toLowerCase().includes(q) || s.id.includes(q) || Object.values(s.metadata || {}).some((v) => String(v).toLowerCase().includes(q))));
  };

  const updateCounts = () => {
    const c = counts();
    for (const k of Object.keys(c)) {
      const el = seg.querySelector(`[data-count="${k}"]`);
      if (el) el.textContent = fmtNumber(c[k]);
    }
  };

  // 表を作り直さずに数とグラフだけ差し替える
  const patch = () => {
    for (const s of items) {
      const tr = box.querySelector(`tr[data-key="${CSS.escape(s.id)}"]`);
      if (!tr) continue;
      const set = (k, v) => { const el = tr.querySelector(`[data-live="${k}"]`); if (el && el.textContent !== v) el.textContent = v; };
      if (s.live) {
        set('bitrate', fmtBps(s.live.bitrate_bps));
        set('bitrate-m', fmtBps(s.live.bitrate_bps));
        set('uptime', fmtDuration(uptimeMs(s)));
        if (viewerCount(s) !== null) set('viewers', fmtNumber(viewerCount(s)));
        const sp = tr.querySelector('[data-live="spark"]');
        if (sp) replace(sp, sparkline({ values: healthSamples(s.id), width: 64, height: 20 }));
      }
    }
  };

  const draw = (force) => {
    const shape = JSON.stringify([filter, query, items.map((s) => [s.id, s.name, streamState(s), recActive.has(s.id), s.recording.enabled, streamSubline(s), viewerCount(s) !== null])]);
    updateCounts();
    if (!force && shape === shapeKey && table && table.isConnected) { patch(); return; }
    shapeKey = shape;
    if (!items.length) {
      table = null;
      replace(box, emptyState({
        icon: 'streams', title: t('streams.emptyTitle'), body: t('streams.emptyBody'), testid: 'streams-empty',
        actions: [createBtn('streams-empty-create'), h('a', { class: 'btn', href: '#/send' }, icon('camera'), t('streams.emptyTry'))],
      }));
      return;
    }
    const rows = visible();
    if (!rows.length) {
      table = null;
      replace(box, emptyState({
        compact: true, icon: 'filter', testid: 'streams-nomatch',
        title: query.trim() ? t('streams.noMatch', { q: query.trim() }) : t(`streams.noneIn.${filter}`),
        body: filter === 'live' && !query.trim() ? t('streams.noneLiveBody') : null,
        actions: [button(t('streams.clearFilters'), { size: 'sm', icon: 'close', onClick: () => { filter = 'all'; query = ''; seg.set('all'); search.input.value = ''; setHashParams({ state: null, q: null }); draw(true); } })],
      }));
      return;
    }
    table = dataTable({ columns, rows, caption: t('streams.caption'), sort: { key: 'state', dir: 'asc' }, rowKey: (s) => s.id, testid: 'streams-table' });
    replace(box, table);
  };

  const fetchAll = async () => {
    const [list, recs] = await Promise.all([
      listAll(streams.list),
      recordings.list({ limit: 200 }).then((r) => r.data.items).catch(() => []),
    ]);
    items = list;
    recActive = new Set(recs.filter((r) => r.state === 'recording').map((r) => r.stream));
    for (const s of items) recordHealth(s);
  };

  const refresh = async () => {
    if (box.dataset.loaded !== '1') {
      return load(box, async () => { await fetchAll(); draw(true); return [...box.childNodes]; }, { kind: 'table' });
    }
    try { await fetchAll(); draw(false); } catch (e) { reportError(e, t('streams.reload'), { retry: refresh }); }
    return true;
  };

  root.append(
    pageHeader({
      title: t('nav.streams'),
      description: t('streams.lead'),
      help: helpHint({ title: t('nav.streams'), body: t('streams.help'), guide: '04-ingest-playback.md' }),
      actions: [createBtn('streams-create')],
    }),
    h('section', { class: 'panel streams-panel' },
      h('div', { class: 'table-toolbar' }, seg, search, h('span', { class: 'spacer' }),
        h('span', { class: 'kbd-hint', 'aria-hidden': 'true' }, h('kbd', null, 'J'), h('kbd', null, 'K'), t('streams.kbdHint'))),
      box));
  await refresh();

  // コマンドパレットの「新しい配信を作成」（#/streams?new=1）
  if (hashParams().get('new') === '1') {
    setHashParams({ new: null });
    create().catch((e) => reportError(e, t('common.create')));
  }
  const offs = [
    onPageKey('n', () => create().catch((err) => reportError(err, t('common.create')))),
    refreshOn(['stream.', 'recording.'], refresh),
    listNav(box, { rows: () => [...box.querySelectorAll('tbody tr')], open: (tr) => open(tr.dataset.key) }),
  ];
  const timer = setInterval(() => { if (document.visibilityState === 'visible') refresh(); }, 5000);
  return () => { offs.forEach((off) => off()); clearInterval(timer); };
}
