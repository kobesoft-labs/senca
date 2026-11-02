// docs/api/openapi.yaml の操作を 1 つずつ関数にしたもの。画面はここだけを使う。

import { request, get, post, patch, del } from './client.js';
import { idemKey } from '../lib/format.js';

const enc = encodeURIComponent;

export const system = {
  capabilities: (o) => get('/capabilities', o),
  nodes: () => get('/nodes'),
  node: (id) => get(`/nodes/${enc(id)}`),
  drain: (id) => post(`/nodes/${enc(id)}/drain`, undefined, { idempotencyKey: idemKey() }),
  undrain: (id) => post(`/nodes/${enc(id)}/undrain`, undefined, { idempotencyKey: idemKey() }),
  devices: () => get('/devices'),
};

export const streams = {
  list: ({ limit, cursor } = {}) => get('/streams', { query: { limit, cursor } }),
  get: (id) => get(`/streams/${enc(id)}`),
  create: (body, key = idemKey()) => post('/streams', body, { idempotencyKey: key }),
  update: (id, body, etag) => patch(`/streams/${enc(id)}`, body, etag),
  remove: (id, etag) => del(`/streams/${enc(id)}`, { ifMatch: etag }),
  rotateKey: (id) => post(`/streams/${enc(id)}/keys`, undefined, { idempotencyKey: idemKey() }),
  playbackUrl: (id, ttl) => post(`/streams/${enc(id)}/playback-url`, ttl ? { ttl_secs: ttl } : undefined),
  startRecording: (id, body) => post(`/streams/${enc(id)}/recordings`, body, { idempotencyKey: idemKey() }),
  plan: (id) => post(`/streams/${enc(id)}/plan`),
  snapshot: (id, format = 'jpeg') => request('GET', `/streams/${enc(id)}/snapshot`, { query: { format }, as: 'blob' }),
  snapshots: (id) => get(`/streams/${enc(id)}/snapshots`),
  snapshotImage: (id, sid) => request('GET', `/streams/${enc(id)}/snapshots/${enc(sid)}`, { as: 'blob' }),
  deleteSnapshot: (id, sid) => del(`/streams/${enc(id)}/snapshots/${enc(sid)}`),
};

export const recordings = {
  list: ({ limit, cursor, stream } = {}) => get('/recordings', { query: { limit, cursor, stream } }),
  get: (id) => get(`/recordings/${enc(id)}`),
  update: (id, body, etag) => patch(`/recordings/${enc(id)}`, body, etag),
  remove: (id, etag) => del(`/recordings/${enc(id)}`, { ifMatch: etag }),
  stop: (id) => post(`/recordings/${enc(id)}/stop`, undefined, { idempotencyKey: idemKey() }),
  playbackUrl: (id, ttl) => post(`/recordings/${enc(id)}/playback-url`, ttl ? { ttl_secs: ttl } : undefined),
  thumbnail: (id) => request('GET', `/recordings/${enc(id)}/thumbnail`, { as: 'blob' }),
  exportMp4: (id, signal) => request('GET', `/recordings/${enc(id)}/export.mp4`, { as: 'response', signal }),
};

export const schedules = {
  list: ({ limit, cursor, stream } = {}) => get('/schedules', { query: { limit, cursor, stream } }),
  get: (id) => get(`/schedules/${enc(id)}`),
  create: (body, key = idemKey()) => post('/schedules', body, { idempotencyKey: key }),
  check: (body) => post('/schedules/check', body),
  update: (id, body, etag) => patch(`/schedules/${enc(id)}`, body, etag),
  remove: (id, etag) => del(`/schedules/${enc(id)}`, { ifMatch: etag }),
  runs: (id) => get(`/schedules/${enc(id)}/runs`),
};

export const tokens = {
  list: ({ limit, cursor } = {}) => get('/tokens', { query: { limit, cursor } }),
  create: (body, key = idemKey()) => post('/tokens', body, { idempotencyKey: key }),
  remove: (id) => del(`/tokens/${enc(id)}`),
};

/** 入力（pull）。protocol: rtmp_pull・rtsp_pull・srt_pull・hls_pull。PATCH は url・credential_ref・enabled・options だけ。 */
export const inputs = {
  list: (stream) => get(`/streams/${enc(stream)}/inputs`, { query: { limit: 1000 } }),
  get: (id) => get(`/inputs/${enc(id)}`),
  create: (stream, body, key = idemKey()) => post(`/streams/${enc(stream)}/inputs`, body, { idempotencyKey: key }),
  update: (id, body, etag) => patch(`/inputs/${enc(id)}`, body, etag),
  remove: (id) => del(`/inputs/${enc(id)}`),
};

/** 出力（push・PaaS）。protocol: rtmp_push・srt_push・whip_push・paas（provider が要る）。 */
export const outputs = {
  list: (stream) => get(`/streams/${enc(stream)}/outputs`, { query: { limit: 1000 } }),
  get: (id) => get(`/outputs/${enc(id)}`),
  create: (stream, body, key = idemKey()) => post(`/streams/${enc(stream)}/outputs`, body, { idempotencyKey: key }),
  update: (id, body, etag) => patch(`/outputs/${enc(id)}`, body, etag),
  remove: (id) => del(`/outputs/${enc(id)}`),
};

/** 秘密値（値は書くだけ。読めない）。入力・出力・字幕からは `secret:{name}` で参照する。 */
export const secrets = {
  list: () => get('/secrets'),
  create: (body, key = idemKey()) => post('/secrets', body, { idempotencyKey: key }),
  remove: (id) => del(`/secrets/${enc(id)}`),
};

/** 一覧を最後まで読む（件数の上限はテナントあたり 1000 なので数回で終わる）。 */
export async function listAll(fn, opts = {}, max = 5000) {
  const items = [];
  let cursor;
  do {
    const { data } = await fn({ ...opts, limit: 1000, cursor });
    items.push(...data.items);
    cursor = data.next_cursor;
  } while (cursor && items.length < max);
  return items;
}

export const SCOPES = [
  ['stream:read', '配信の参照'],
  ['stream:write', '配信の作成・変更'],
  ['publish', '送出（publish）'],
  ['subscribe', '視聴（再生 URL）'],
  ['recording:read', '録画の参照'],
  ['recording:delete', '録画の削除'],
  ['tenant:admin', 'テナント管理（node:admin 以外すべて）'],
  ['node:admin', 'ノード運用'],
];
