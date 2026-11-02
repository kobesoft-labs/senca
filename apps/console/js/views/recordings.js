// 録画の一覧。stream で絞る（API の stream クエリ）。state はノードが絞り込まないので画面で絞る。

import { h, replace, table, badge, select, field } from '../lib/dom.js';
import { load } from '../lib/ui.js';
import { recordings, streams, listAll } from '../api/resources.js';
import { fmtTime, fmtDuration, fmtBytes } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';

const PAGE = 50;

export function recBadge(r) {
  if (r.state === 'recording') return badge('録画中', 'live');
  if (r.state === 'interrupted') return badge('中断', 'warn');
  return badge('確定', 'ok');
}

export async function render(root) {
  const box = h('div');
  const pager = h('div', { class: 'pager' });
  const params = new URLSearchParams(location.hash.split('?')[1] || '');
  const streamSel = select([['', 'すべての配信']], params.get('stream') || '');
  const stateSel = select([['', 'すべての状態'], ['recording', '録画中'], ['finalized', '確定'], ['interrupted', '中断']], '');
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
    replace(pager,
      h('button', { class: 'small', disabled: page === 0, onClick: () => { page -= 1; refresh(); } }, '← 前へ'),
      h('span', { class: 'muted small' }, `${page + 1} ページ目${stateSel.value ? `（このページの ${data.items.length} 件から絞り込み）` : ''}`),
      h('button', { class: 'small', disabled: !data.next_cursor, onClick: () => { page += 1; refresh(); } }, '次へ →'));
    return table([
      { label: '録画', render: (r) => h('a', { href: `#/recordings/${encodeURIComponent(r.id)}` }, r.name || fmtTime(r.started_at)) },
      { label: '配信', render: (r) => (names.has(r.stream) ? h('a', { href: `#/streams/${encodeURIComponent(r.stream)}` }, names.get(r.stream)) : h('code', { class: 'muted' }, r.stream.slice(0, 10))) },
      { label: '状態', render: recBadge },
      { label: '開始', render: (r) => fmtTime(r.started_at) },
      { label: '長さ', class: 'num', render: (r) => fmtDuration(r.duration_ms) },
      { label: '大きさ', class: 'num', render: (r) => fmtBytes(r.bytes) },
      { label: '欠落', class: 'num', render: (r) => (r.gaps && r.gaps.length ? badge(String(r.gaps.length), 'warn') : '0') },
      { label: '保全', render: (r) => (r.legal_hold ? badge('法的保全', 'danger') : '') },
      { label: '保持期限', render: (r) => (r.retention_until ? fmtTime(r.retention_until) : '無期限') },
    ], items, { empty: '録画はありません', caption: '録画の一覧' });
  });

  const reset = () => { cursors = [undefined]; page = 0; refresh(); };
  streamSel.addEventListener('change', reset);
  stateSel.addEventListener('change', reset);

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, '録画'), h('span', { class: 'spacer' }), h('button', { onClick: () => refresh() }, '再読み込み')),
    h('section', { class: 'panel' },
      h('div', { class: 'toolbar form' }, field('配信', streamSel), field('状態', stateSel)),
      box, pager));
  await refresh();
  return refreshOn(['recording.'], refresh);
}
