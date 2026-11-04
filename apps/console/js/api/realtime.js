// リアルタイムの部屋（/rooms…）とメディアパイプライン（/media-pipelines…）の API。docs/api/openapi.yaml の操作を 1 つずつ関数に。

import { get, post, patch, del } from './client.js';
import { idemKey } from '../lib/format.js';

const enc = encodeURIComponent;

export const rooms = {
  list: ({ limit, cursor } = {}) => get('/rooms', { query: { limit, cursor } }),
  get: (id) => get(`/rooms/${enc(id)}`),
  create: (body, key = idemKey()) => post('/rooms', body, { idempotencyKey: key }),
  /** settings は JSON Merge Patch（書いた項目だけ変わる。null で既定値に戻る）。If-Match が要る。 */
  update: (id, body, etag) => patch(`/rooms/${enc(id)}`, body, etag),
  remove: (id) => del(`/rooms/${enc(id)}`),
  /** 参加トークン。部屋が無ければ自動で作られる（auto: true）ので、画面からは在る部屋にだけ使う。 */
  token: (id, body) => post(`/rooms/${enc(id)}/tokens`, body),
  participants: (id) => get(`/rooms/${enc(id)}/participants`, { query: { limit: 1000 } }),
  kick: (id, conn) => del(`/rooms/${enc(id)}/participants/${enc(conn)}`),
  updateParticipant: (id, conn, metadata) => patch(`/rooms/${enc(id)}/participants/${enc(conn)}`, { metadata }),
  message: (id, body) => post(`/rooms/${enc(id)}/messages`, body),
  stats: (id) => get(`/rooms/${enc(id)}/stats`),
};

export const mediaPipelines = {
  list: ({ room, limit, cursor } = {}) => get('/media-pipelines', { query: { room, limit, cursor } }),
  get: (id) => get(`/media-pipelines/${enc(id)}`),
  create: (body, key = idemKey()) => post('/media-pipelines', body, { idempotencyKey: key }),
  update: (id, body, etag) => patch(`/media-pipelines/${enc(id)}`, body, etag),
  remove: (id) => del(`/media-pipelines/${enc(id)}`),
};
