// 受け取った SSE イベントの直近 50 件（ダッシュボードの「最近の出来事」）。

import { on, emit } from './bus.js';
import { t, has } from '../i18n/index.js';

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

/** イベントの種類 → 画面の言葉（activity.<種類>。知らないものは種類のまま）。 */
export function label(type) {
  return has(`activity.${type}`) ? t(`activity.${type}`) : type;
}
