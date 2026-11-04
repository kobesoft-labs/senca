// パイプライン（REQ-005）と運用の操作（REQ-008）の API。ノードの crates/senca-node/src/api/pipelines.rs が正。
//
//   POST   /pipelines                 {name, graph, desired_state?}（Idempotency-Key）→ 201 ＋ ETag
//   GET    /pipelines                 {items, next_cursor}
//   GET    /pipelines/{id}            ＋ observed_state・last_error・plan・status・meters
//   PATCH  /pipelines/{id}            {name?, graph?, desired_state?}（If-Match 必須）
//   DELETE /pipelines/{id}
//   POST   /pipelines/{id}/validate   本文が空なら保存済みのグラフ、本文があればそのグラフ → {ok, plan}
//   POST   /pipelines/{id}/control    senca-pipeline の Control（{"op": ...}）→ {ok, reply}
//
// 検証の失敗は problem+json（INVALID_CONFIG・CAPACITY_UNAVAILABLE）で、details.issues に [{code, node?, message}]、
// グラフの形の誤りは details.reason（文字列）に入る。issuesOf() でどちらも一覧にする。
//
// 古いノード（/pipelines が無い）では NotAvailableError を投げる。

import { request, ApiError } from './client.js';
import { idemKey } from '../lib/format.js';
import { t } from '../i18n/index.js';

export class NotAvailableError extends Error {
  constructor(feature) {
    super(t('pipelines.notAvailable', { feature }));
    this.name = 'NotAvailableError';
    this.feature = feature;
  }
}

let available = null;

async function call(method, path, opt = {}) {
  if (available === false) throw new NotAvailableError('pipelines');
  try {
    const r = await request(method, path, opt);
    available = true;
    return r;
  } catch (e) {
    if (e instanceof ApiError && (e.status === 404 || e.status === 405) && available !== true && path === '/pipelines') {
      available = false;
      throw new NotAvailableError('pipelines');
    }
    throw e;
  }
}

const enc = encodeURIComponent;

export const pipelines = {
  list: () => call('GET', '/pipelines', { query: { limit: 100 } }),
  get: (id, opt = {}) => call('GET', `/pipelines/${enc(id)}`, opt),
  create: ({ name, graph, desired_state = 'stopped' }) => call('POST', '/pipelines', { body: { name, graph, desired_state }, idempotencyKey: idemKey() }),
  /** body: {name?, graph?, desired_state?} */
  update: (id, body, etag) => call('PATCH', `/pipelines/${enc(id)}`, { body, ifMatch: etag }),
  setDesired: (id, desired, etag) => call('PATCH', `/pipelines/${enc(id)}`, { body: { desired_state: desired }, ifMatch: etag }),
  /** graph を渡すと保存前のグラフを検証する。 */
  validate: (id, graph) => call('POST', `/pipelines/${enc(id)}/validate`, graph ? { body: graph } : {}),
  remove: (id) => call('DELETE', `/pipelines/${enc(id)}`),
  control: (id, op) => call('POST', `/pipelines/${enc(id)}/control`, { body: op }),
};

/** 運用の操作（Control の JSON）。node は compose・audio_mix の部品 id。 */
export const ops = {
  setPreview: (node, scene) => ({ op: 'set_preview', node, scene }),
  switchProgram: (node, scene, transition) => ({ op: 'switch_program', node, scene, ...(transition ? { transition } : {}) }),
  take: (node, transition) => ({ op: 'take', node, ...(transition ? { transition } : {}) }),
  setAudio: (node, input, patch) => ({ op: 'set_audio', node, input, ...patch }),
  setMaster: (node, gainDb) => ({ op: 'set_master', node, gain_db: gainDb }),
  updateLayer: (node, scene, layer, patch) => ({ op: 'update_layer', node, scene, layer, patch }),
  setScene: (node, scene, layers) => ({ op: 'set_scene', node, scene, layers }),
  snapshot: (node, enabled) => ({ op: 'snapshot', node, enabled }),
  requestKeyframe: (node) => ({ op: 'request_keyframe', ...(node ? { node } : {}) }),
};

/**
 * 検証の失敗（ApiError）を問題の一覧にする: [{code, node, message}]
 * @param {ApiError} e
 */
export function issuesOf(e) {
  const d = e && e.details;
  if (d && Array.isArray(d.issues)) return d.issues.map((i) => ({ code: i.code, node: i.node || null, message: i.message }));
  if (d && d.reason) return [{ code: e.code || 'INVALID_CONFIG', node: null, message: String(d.reason) }];
  return [{ code: (e && e.code) || 'ERROR', node: null, message: (e && (e.title || e.message)) || String(e) }];
}

/**
 * GET /pipelines/{id} の status・meters を読み分ける。
 * ノードは通知（senca-pipeline の Event、`"event"` で種類が分かれる）を種類を見ずに status に入れることがあるので、
 * `event` を見て status（event = status）とメーター（event = meters）に分ける。
 * @returns {{status: object|null, meters: Record<string, object>}}
 */
export function runtimeOf(p) {
  const meters = {};
  let status = null;
  const take = (v) => {
    if (!v || typeof v !== 'object') return;
    if (v.event === 'meters' && v.node) meters[v.node] = v;
    else if (v.event === 'status' || (!v.event && Array.isArray(v.compose))) status = v;
  };
  take(p.status);
  for (const v of Object.values(p.meters || {})) take(v);
  return { status, meters };
}
