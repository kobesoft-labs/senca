// パイプライン（REQ-005）と制作・スイッチャー（REQ-008）の API。
//
// どちらも openapi.yaml にはまだ無い（今のノードは 404）。画面はこのモジュールだけを通して呼ぶので、
// ノードが実装したらここを仕様に合わせるだけでつながる（画面は書き直さない）。
//
// - 集合の経路（GET /pipelines・GET /productions）が 404 なら「このノードでは使えない」と覚え、
//   以降の呼び出しは NotAvailableError を投げる（画面はローカルの状態だけで動く）。
// - 個々の資源の 404 は普通の ApiError（NOT_FOUND）のまま。

import { request, ApiError } from './client.js';
import { idemKey } from '../lib/format.js';

export class NotAvailableError extends Error {
  constructor(feature) {
    super(`${feature} の API はこのノードでは使えません（未実装）`);
    this.name = 'NotAvailableError';
    this.feature = feature;
  }
}

const availability = new Map(); // feature -> true | false

/** 機能の有無を確かめる（結果は覚える）。 */
export async function probe(feature) {
  if (availability.has(feature)) return availability.get(feature);
  try {
    await request('GET', `/${feature}`, { query: { limit: 1 } });
    availability.set(feature, true);
  } catch (e) {
    if (e instanceof ApiError && (e.status === 404 || e.status === 405 || e.status === 501)) availability.set(feature, false);
    else throw e;
  }
  return availability.get(feature);
}

async function call(feature, method, path, opt = {}) {
  if (availability.get(feature) === false) throw new NotAvailableError(feature);
  try {
    return await request(method, path, opt);
  } catch (e) {
    if (e instanceof ApiError && e.status === 404 && availability.get(feature) !== true) {
      // 集合が無いのか資源が無いのかを確かめる
      const ok = await probe(feature).catch(() => null);
      if (ok === false) throw new NotAvailableError(feature);
    }
    throw e;
  }
}

const enc = encodeURIComponent;

/** パイプライン（入力 → 変換 → 出力の DAG）。REQ-002・REQ-005 の案に合わせた形。 */
export const pipelines = {
  list: () => call('pipelines', 'GET', '/pipelines'),
  get: (id) => call('pipelines', 'GET', `/pipelines/${enc(id)}`),
  create: (def) => call('pipelines', 'POST', '/pipelines', { body: def, idempotencyKey: idemKey() }),
  update: (id, def, etag) => call('pipelines', 'PATCH', `/pipelines/${enc(id)}`, { body: def, ifMatch: etag }),
  validate: (id) => call('pipelines', 'POST', `/pipelines/${enc(id)}/validate`),
  commit: (id, etag) => call('pipelines', 'POST', `/pipelines/${enc(id)}/commit`, { ifMatch: etag, idempotencyKey: idemKey() }),
  remove: (id, etag) => call('pipelines', 'DELETE', `/pipelines/${enc(id)}`, { ifMatch: etag }),
};

/** 制作（サーバー側のスイッチャー）。REQ-008 の API 案に合わせた形。 */
export const productions = {
  list: () => call('productions', 'GET', '/productions'),
  get: (id) => call('productions', 'GET', `/productions/${enc(id)}`),
  create: (body) => call('productions', 'POST', '/productions', { body, idempotencyKey: idemKey() }),
  createScene: (id, scene) => call('productions', 'POST', `/productions/${enc(id)}/scenes`, { body: scene, idempotencyKey: idemKey() }),
  updateScene: (id, sceneId, scene, etag) => call('productions', 'PATCH', `/productions/${enc(id)}/scenes/${enc(sceneId)}`, { body: scene, ifMatch: etag }),
  preview: (id, sceneId) => call('productions', 'POST', `/productions/${enc(id)}/preview`, { body: { scene: sceneId } }),
  transition: (id, kind, durationMs) => call('productions', 'POST', `/productions/${enc(id)}/transition`, { body: { type: kind, duration_ms: durationMs } }),
  audio: (id, source, body) => call('productions', 'PATCH', `/productions/${enc(id)}/audio/${enc(source)}`, { body }),
  outputs: (id, body) => call('productions', 'POST', `/productions/${enc(id)}/outputs`, { body, idempotencyKey: idemKey() }),
};
