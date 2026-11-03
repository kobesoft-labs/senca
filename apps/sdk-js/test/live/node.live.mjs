// SencaClient を本物の senca-node に対して（tools/interop/sdk_js.sh の B が起動して SENCA_URL・SENCA_TOKEN を渡す）。
//   SENCA_URL=http://127.0.0.1:8080 SENCA_TOKEN=senca_... node --test test/live/node.live.mjs
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ApiError, SencaClient } from '../../src/index.js';

const url = process.env.SENCA_URL;
const token = process.env.SENCA_TOKEN;
const opts = { skip: !url || !token ? 'SENCA_URL・SENCA_TOKEN が無い' : false };

test('stream の作成・一覧・取得・If-Match の更新・412・Idempotency の再送・SSE・削除', opts, async () => {
  const c = new SencaClient({ baseUrl: url, token });
  const caps = await c.capabilities();
  assert.equal(caps.api_version, 'v1');

  // SSE を先に開く
  const ac = new AbortController();
  const seen = [];
  const sse = (async () => {
    for await (const ev of c.events({ types: ['stream.*'], signal: ac.signal })) {
      seen.push(ev);
      if (ev.type === 'stream.created') return ev;
    }
  })();
  await new Promise((r) => setTimeout(r, 300));

  const suffix = Date.now().toString(36);
  const key = SencaClient.newIdempotencyKey();
  const a = await c.streams.create({ name: `sdk-js-a-${suffix}`, metadata: { by: 'sdk-js' } }, { idempotencyKey: key });
  assert.ok(a.id);
  assert.equal(a.generation, 1);
  // 同じ鍵の再送は同じ答え（stream key は伏せる）
  const replay = await c.request('POST', '/streams', { body: { name: `sdk-js-a-${suffix}`, metadata: { by: 'sdk-js' } }, idempotencyKey: key });
  assert.equal(replay.data.id, a.id);
  assert.equal(replay.replayed, true);
  // 同じ鍵で違う本文は 409
  await assert.rejects(c.streams.create({ name: `other-${suffix}` }, { idempotencyKey: key }), (e) => e instanceof ApiError && e.status === 409 && e.code === 'IDEMPOTENCY_CONFLICT');

  const ev = await Promise.race([sse, new Promise((_, rej) => setTimeout(() => rej(new Error('no SSE event')), 5000))]);
  assert.equal(ev.type, 'stream.created');
  assert.ok(ev.id);
  ac.abort();

  const b = await c.streams.create({ name: `sdk-js-b-${suffix}` });
  const d = await c.streams.create({ name: `sdk-js-c-${suffix}` });
  const ids = [];
  for await (const s of c.streams.list({ limit: 1 })) ids.push(s.id);
  for (const x of [a, b, d]) assert.ok(ids.includes(x.id), `list (limit 1) に ${x.id}`);

  const got = await c.streams.get(a.id);
  assert.equal(SencaClient.etagOf(got), `"${got.generation}"`);
  const upd = await c.streams.update(a.id, { metadata: { by: 'sdk-js', step: '2' } }, { ifMatch: got });
  assert.equal(upd.generation, got.generation + 1);
  // 古い版で書くと 412
  await assert.rejects(c.streams.update(a.id, { metadata: {} }, { ifMatch: got }), (e) => e instanceof ApiError && e.status === 412 && e.code === 'PRECONDITION_FAILED' && !!e.requestId);
  // If-Match が無いと 428（SDK は投げるので raw で）
  await assert.rejects(c.request('PATCH', `/streams/${a.id}`, { body: { metadata: {} } }), (e) => e.status === 428 && e.code === 'PRECONDITION_REQUIRED');
  // 読み直して書く
  const mod = await c.streams.modify(a.id, (cur) => ({ metadata: { ...cur.metadata, step: '3' } }));
  assert.equal(mod.metadata.step, '3');
  // 未知のフィールドは 400
  await assert.rejects(c.streams.create({ name: 'x', bogus: 1 }), (e) => e.status === 400);

  for (const x of [a, b, d]) await c.streams.delete(x.id);
  await assert.rejects(c.streams.get(a.id), (e) => e.status === 404 && e.code === 'NOT_FOUND');

  // 他の資源の一覧も読める
  assert.ok(Array.isArray(await c.secrets.list()));
  assert.ok(Array.isArray(await c.nodes.list()));
  await c.tokens.list().toArray();
});

test('認証の失敗は 401 の ApiError（再試行しない）', opts, async () => {
  const c = new SencaClient({ baseUrl: url, token: 'senca_wrong' });
  await assert.rejects(c.streams.list().first(), (e) => e instanceof ApiError && e.status === 401 && !e.retryable);
});

test('部屋: 作成・一覧・If-Match の更新・参加トークン・参加者・統計・削除', opts, async () => {
  const c = new SencaClient({ baseUrl: url, token, retry: { maxAttempts: 8, initialDelayMs: 200, maxDelayMs: 1000 } });
  const id = `sdk-js-${Date.now().toString(36)}`;
  const room = await c.rooms.create({ id, settings: { max_participants: 4, video_codec: 'vp8' }, metadata: { by: 'sdk-js' } });
  assert.equal(room.id, id);
  const ids = (await c.rooms.list({ limit: 1 }).toArray()).map((r) => r.id);
  assert.ok(ids.includes(id));
  const got = await c.rooms.get(id);
  const upd = await c.rooms.update(id, { metadata: { by: 'sdk-js', n: 2 } }, { ifMatch: got });
  assert.equal(upd.generation, got.generation + 1);
  await assert.rejects(c.rooms.update(id, { metadata: {} }, { ifMatch: got }), (e) => e.status === 412);
  const tok = await c.rooms.createToken(id, { role: 'sendrecv', client_id: 'alice', ttl_secs: 60 });
  assert.equal(tok.room_id, id);
  assert.match(tok.token, /^[\w-]+\.[\w-]+\.[\w-]+$/);
  // 参加者・統計は部屋を置いたノードへ（置き場所が決まるまで 503 → SDK が送り直す）
  const parts = await c.rooms.participants(id).list().toArray();
  assert.deepEqual(parts, []);
  const st = await c.rooms.stats(id);
  assert.equal(st.room_id, id);
  await c.rooms.delete(id);
  await assert.rejects(c.rooms.get(id), (e) => e.status === 404);
});
