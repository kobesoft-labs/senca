// 受け取った SSE イベントの直近 50 件（ダッシュボードの「最近の出来事」）。

import { on, emit } from './bus.js';

const MAX = 50;
const items = [];

on('sse', (ev) => {
  items.unshift({ at: new Date(), type: ev.type, id: ev.id, data: ev.data });
  if (items.length > MAX) items.length = MAX;
  emit('activity', null);
});
on('resync', () => {
  items.unshift({ at: new Date(), type: 'resync', id: null, data: {} });
  if (items.length > MAX) items.length = MAX;
  emit('activity', null);
});

export function recent() {
  return items.slice();
}

const LABELS = {
  'stream.created': '配信を作成',
  'stream.updated': '配信を更新',
  'stream.deleted': '配信を削除',
  'recording.updated': '録画を更新',
  'recording.deleted': '録画を削除',
  'schedule.created': '予約を作成',
  'schedule.updated': '予約を更新',
  'schedule.deleted': '予約を削除',
  'schedule.run': '予約を実行',
  'node.updated': 'ノードを更新',
  resync: '再同期（一覧を読み直し）',
};

export function label(type) {
  return LABELS[type] || type;
}
