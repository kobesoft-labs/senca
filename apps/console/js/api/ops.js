// 運用の API: グリッド（GET /grid）、装置の無効化・有効化、監査ログ（GET /audit）。
// ノードの一覧・drain は resources.js の system にある。

import { get, post } from './client.js';
import { idemKey } from '../lib/format.js';

const enc = encodeURIComponent;

export const grid = {
  /** スケジューラーが把握している状態（node:admin）。scheduler は応答したノードがリーダーのときだけ値が入る。 */
  get: () => get('/grid'),
  /** 装置（GPU・キャプチャ機器）を手動で無効・有効にする。node は ID か名前、dev は `nvidia:0` など。 */
  setDevice: (node, dev, enabled) => post(`/nodes/${enc(node)}/devices/${enc(dev)}/${enabled ? 'enable' : 'disable'}`, undefined, { idempotencyKey: idemKey() }),
};

/**
 * 監査ログ（新しい順）。q: {limit, cursor, since, until, action, subject, resource, q, node, tenant}。
 * 1 ページの項目が limit より少なくても（0 件でも）next_cursor が返ることがある。fill() で埋める。
 */
export const audit = {
  list: (q = {}, opt = {}) => get('/audit', { query: q, signal: opt.signal }),
};

/**
 * 表示に足りるまで監査ログをたどる。want 件以上集まるか、next_cursor が null になるか、maxRequests に達したら止める。
 * @returns {Promise<{items: object[], next: string|null, unavailable: string[], requests: number}>}
 */
export async function auditFill(query, { cursor, want = 50, limit = 100, maxRequests = 20, signal } = {}) {
  const items = [];
  const unavailable = new Set();
  let next = cursor || undefined;
  let requests = 0;
  do {
    const { data } = await audit.list({ ...query, limit, cursor: next }, { signal });
    requests += 1;
    items.push(...data.items);
    for (const n of data.unavailable_nodes || []) unavailable.add(n);
    next = data.next_cursor || null;
  } while (next && items.length < want && requests < maxRequests);
  return { items, next, unavailable: [...unavailable], requests };
}
