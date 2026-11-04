// 資源の ID → 名前（監査ログ・グリッド・ダッシュボードで「studio-a」と出すため）。
// 一覧を読んで 30 秒使い回す。読めない一覧（権限が無いなど）は飛ばし、名前が分からなければ ID の頭を出す。

import { streams, recordings, schedules, tokens, secrets, system, inputs, outputs, listAll } from '../api/resources.js';
import { pipelines } from '../api/pipelines.js';

const TTL = 30000;
const tables = new Map(); // kind → {at, map: Map<id, name>, promise}
const io = new Map(); // `input:{id}` → {stream, protocol} | null

const LOADERS = {
  stream: async () => (await listAll(streams.list)).map((x) => [x.id, x.name]),
  recording: async () => (await recordings.list({ limit: 500 })).data.items.map((x) => [x.id, x.name || null]),
  schedule: async () => (await listAll(schedules.list)).map((x) => [x.id, x.name || null]),
  token: async () => (await listAll(tokens.list)).map((x) => [x.id, x.name]),
  secret: async () => (await secrets.list()).data.items.map((x) => [x.id, x.name]),
  pipeline: async () => (await pipelines.list()).data.items.map((x) => [x.id, x.name]),
  node: async () => (await system.nodes()).data.items.map((x) => [x.id, x.name]),
};

async function loadKind(kind) {
  const cur = tables.get(kind);
  if (cur && Date.now() - cur.at < TTL) return cur.promise;
  const promise = LOADERS[kind]().then((pairs) => new Map(pairs.filter(([, n]) => n))).catch(() => new Map());
  tables.set(kind, { at: Date.now(), promise });
  return promise;
}

/** 名前の表を読む（kinds: 'stream'・'token' など）。戻り値の関数で引く。 */
export async function loadNames(kinds) {
  const maps = Object.fromEntries(await Promise.all(kinds.map(async (k) => [k, await loadKind(k)])));
  return (kind, id) => (maps[kind] && maps[kind].get(id)) || null;
}

/** 名前の表を捨てる（作成・削除の後に）。 */
export function forgetNames(kind) {
  if (kind) tables.delete(kind);
  else tables.clear();
}

/** 入力・出力の持ち主の配信と種類（`GET /inputs/{id}`・`/outputs/{id}`）。分からなければ null。 */
export async function ioInfo(kind, id) {
  const k = `${kind}:${id}`;
  if (io.has(k)) return io.get(k);
  const p = (kind === 'input' ? inputs.get(id) : outputs.get(id))
    .then(({ data }) => ({ stream: data.stream, protocol: data.protocol, provider: data.provider || null }))
    .catch(() => null);
  io.set(k, p);
  return p;
}

/** ID を短くして見せる（名前が分からないとき）。 */
export function shortId(id) {
  const s = String(id ?? '');
  return s.length > 12 ? `${s.slice(0, 8)}…` : s;
}
