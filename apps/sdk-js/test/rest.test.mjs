// SencaClient（管理 API）の試験。fetch を差し替えて、要求の形・再試行・頁・If-Match・problem・SSE を確かめる。
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { ApiError, EventStreamParser, SencaClient } from '../src/index.js';
import { OPERATIONS } from '../src/rest/operations.gen.js';

/** 台本の fetch。`routes` は (req) => Response を返す関数の列（順に使う）か、1 つの関数。 */
function fakeFetch(handler) {
  const calls = [];
  const f = async (url, init = {}) => {
    const req = { url: new URL(url), method: init.method ?? 'GET', headers: new Headers(init.headers), body: init.body ? JSON.parse(init.body) : undefined, signal: init.signal };
    calls.push(req);
    return handler(req, calls.length);
  };
  return { fetch: f, calls };
}

const json = (status, body, headers = {}) => new Response(body === undefined ? null : JSON.stringify(body), { status, headers: { 'Content-Type': status >= 400 ? 'application/problem+json' : 'application/json', 'X-Request-Id': 'req_1', ...headers } });
const problem = (status, code, extra = {}) => json(status, { type: `urn:senca:error:${code.toLowerCase().replace(/_/g, '-')}`, title: code, status, code, retryable: false, request_id: 'req_p', ...extra });
const stream = (id, gen = 1) => ({ id, name: `s-${id}`, generation: gen, metadata: {} });

test('作成: Idempotency-Key を付け、503（Retry-After）なら同じ鍵で送り直す', async () => {
  const { fetch, calls } = fakeFetch((req, n) => (n === 1 ? problem(503, 'CAPACITY_UNAVAILABLE', { retryable: true }) : json(201, stream('a'), { ETag: '"1"', Location: '/api/v1/streams/a' })));
  const c = new SencaClient({ baseUrl: 'http://node.test', token: 'senca_abc', fetch, retry: { initialDelayMs: 1, maxDelayMs: 2 } });
  const s = await c.streams.create({ name: 's-a' });
  assert.equal(s.id, 'a');
  assert.equal(calls.length, 2);
  assert.equal(calls[0].url.href, 'http://node.test/api/v1/streams');
  assert.equal(calls[0].method, 'POST');
  assert.equal(calls[0].headers.get('authorization'), 'Bearer senca_abc');
  assert.equal(calls[0].headers.get('content-type'), 'application/json');
  const key = calls[0].headers.get('idempotency-key');
  assert.match(key, /^[0-9a-f-]{36}$/);
  assert.equal(calls[1].headers.get('idempotency-key'), key, '再試行は同じ鍵');
  assert.deepEqual(calls[0].body, { name: 's-a' });
  assert.equal(SencaClient.etagOf(s), '"1"');
});

test('自分の Idempotency-Key・付けない指定、トークンの関数', async () => {
  const { fetch, calls } = fakeFetch(() => json(201, stream('b')));
  let n = 0;
  const c = new SencaClient({ baseUrl: 'http://node.test/api/v1/', token: () => `t${++n}`, fetch });
  await c.streams.create({ name: 'b' }, { idempotencyKey: 'my-key-1' });
  await c.request('POST', '/streams', { body: { name: 'c' }, idempotencyKey: false });
  assert.equal(calls[0].headers.get('idempotency-key'), 'my-key-1');
  assert.equal(calls[1].headers.get('idempotency-key'), null);
  assert.equal(calls[0].url.href, 'http://node.test/api/v1/streams');
  assert.equal(calls[1].headers.get('authorization'), 'Bearer t2');
});

test('一覧: cursor をたどる（for await・pages・toArray・first）', async () => {
  const all = ['a', 'b', 'c', 'd', 'e'].map((id) => stream(id));
  const { fetch, calls } = fakeFetch((req) => {
    const limit = Number(req.url.searchParams.get('limit') ?? 100);
    const cursor = req.url.searchParams.get('cursor');
    const start = cursor ? all.findIndex((x) => x.id === cursor) + 1 : 0;
    const items = all.slice(start, start + limit);
    return json(200, { items, next_cursor: start + limit < all.length ? items.at(-1).id : null });
  });
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch });
  const ids = [];
  for await (const s of c.streams.list({ limit: 2 })) ids.push(s.id);
  assert.deepEqual(ids, ['a', 'b', 'c', 'd', 'e']);
  assert.deepEqual(calls.map((r) => r.url.searchParams.get('cursor')), [null, 'b', 'd']);
  const pages = [];
  for await (const p of c.streams.list({ limit: 3 }).pages()) pages.push(p.items.length);
  assert.deepEqual(pages, [3, 2]);
  assert.equal((await c.streams.list({ limit: 2 }).toArray({ max: 3 })).length, 3);
  assert.equal((await c.streams.list().first()).id, 'a');
  // 絞り込みのクエリ
  calls.length = 0;
  await c.recordings.list({ stream: 'a', limit: 5 }).first();
  assert.equal(calls[0].url.search, '?limit=5&stream=a');
});

test('更新: If-Match（ETag・generation・資源）、無ければ投げる、412 は ApiError', async () => {
  const { fetch, calls } = fakeFetch((req) => {
    if (req.method === 'GET') return json(200, stream('a', 7), { ETag: '"7"' });
    if (req.headers.get('if-match') !== '"7"') return problem(412, 'PRECONDITION_FAILED');
    return json(200, stream('a', 8), { ETag: '"8"' });
  });
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch });
  const cur = await c.streams.get('a');
  const next = await c.streams.update('a', { name: 'x' }, { ifMatch: cur });
  assert.equal(next.generation, 8);
  assert.equal(calls[1].method, 'PATCH');
  assert.equal(calls[1].headers.get('if-match'), '"7"');
  await c.streams.update('a', { name: 'x' }, { ifMatch: 7 });
  await c.streams.update('a', { name: 'x' }, { ifMatch: 'W/"7"' }).catch(() => {});
  assert.equal(calls.at(-1).headers.get('if-match'), 'W/"7"');
  const e = await c.streams.update('a', { name: 'x' }, { ifMatch: { generation: 3 } }).catch((x) => x);
  assert.ok(e instanceof ApiError);
  assert.equal(e.status, 412);
  assert.equal(e.code, 'PRECONDITION_FAILED');
  assert.equal(e.requestId, 'req_p');
  assert.equal(e.retryable, false);
  assert.equal(e.fatal, true);
  await assert.rejects(c.streams.update('a', {}), TypeError);
});

test('modify: 412 なら読み直してやり直す', async () => {
  let gen = 1;
  let races = 1;
  const { fetch, calls } = fakeFetch((req) => {
    if (req.method === 'GET') return json(200, { ...stream('a', gen), metadata: { n: String(gen) } }, { ETag: `"${gen}"` });
    if (races-- > 0) { gen++; return problem(412, 'PRECONDITION_FAILED'); } // 他の人が先に書いた
    gen++;
    return json(200, { ...stream('a', gen), metadata: req.body.metadata });
  });
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch });
  const r = await c.streams.modify('a', (cur) => ({ metadata: { ...cur.metadata, k: 'v' } }));
  assert.deepEqual(r.metadata, { n: '2', k: 'v' });
  assert.deepEqual(calls.map((x) => x.method), ['GET', 'PATCH', 'GET', 'PATCH']);
});

test('再試行の方針: GET は通信の失敗で送り直す、PATCH の 500 は送り直さない、400 は送り直さない', async () => {
  let n = 0;
  const { fetch, calls } = fakeFetch((req) => {
    if (req.method === 'GET' && ++n === 1) throw new TypeError('fetch failed');
    if (req.method === 'PATCH') return problem(500, 'INTERNAL_ERROR', { retryable: true });
    if (req.method === 'DELETE') return problem(400, 'INVALID_REQUEST');
    return json(200, stream('a'));
  });
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch, retry: { initialDelayMs: 1 } });
  assert.equal((await c.streams.get('a')).id, 'a');
  assert.equal(calls.length, 2);
  calls.length = 0;
  await assert.rejects(c.streams.update('a', {}, { ifMatch: 1 }), (e) => e.status === 500 && e.retryable);
  assert.equal(calls.length, 1);
  calls.length = 0;
  await assert.rejects(c.streams.delete('a'), (e) => e.code === 'INVALID_REQUEST');
  assert.equal(calls.length, 1);
  // 通信の失敗は status 0
  const bad = new SencaClient({ baseUrl: 'http://node.test', fetch: async () => { throw new TypeError('ECONNREFUSED'); }, retry: false });
  await assert.rejects(bad.capabilities(), (e) => e instanceof ApiError && e.status === 0 && e.code === 'NETWORK_ERROR');
});

test('取り消しと時間切れ', async () => {
  const hang = (req) => new Promise((_, rej) => (req.signal.aborted ? rej(req.signal.reason) : req.signal.addEventListener('abort', () => rej(req.signal.reason))));
  const { fetch } = fakeFetch(hang);
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch, retry: false, timeoutMs: 20 });
  await assert.rejects(c.streams.get('a'), (e) => e.code === 'TIMEOUT' && e.retryable);
  const ac = new AbortController();
  const p = c.streams.get('a', { signal: ac.signal });
  ac.abort();
  await assert.rejects(p, (e) => e.name === 'AbortError');
});

test('problem でない失敗の応答も ApiError', async () => {
  const { fetch } = fakeFetch(() => new Response('too large', { status: 413, headers: { 'Content-Type': 'text/plain' } }));
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch });
  await assert.rejects(c.streams.create({ name: 'x' }), (e) => e.status === 413 && e.code === 'PAYLOAD_TOO_LARGE' && e.title === 'too large');
});

test('SSE の解析器', () => {
  const p = new EventStreamParser();
  assert.deepEqual(p.feed(': keepalive\n\nevent: stream.created\nid: 4'), []);
  assert.deepEqual(p.feed('2\ndata: {"a":\ndata: 1}\r\n\r\n'), [{ type: 'stream.created', id: '42', data: '{"a":\n1}' }]);
  assert.deepEqual(p.feed('retry: 1500\ndata: x\n\n'), [{ type: 'message', id: '42', data: 'x' }]);
  assert.equal(p.retry, 1500);
});

test('events(): 種類で絞る、resync は通す、切れたら Last-Event-ID で付け直す、401 で終わる', async () => {
  let n = 0;
  const { fetch, calls } = fakeFetch(() => {
    n++;
    if (n === 1) {
      return new Response('event: stream.created\nid: 1\ndata: {"type":"stream.created","resource_id":"a","occurred_at":"t","data":{}}\n\n'
        + 'event: recording.updated\nid: 2\ndata: {"type":"recording.updated","resource_id":"r","occurred_at":"t","data":{}}\n\n', { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
    }
    if (n === 2) return new Response('event: resync\nid: 9\ndata: {}\n\nevent: stream.deleted\nid: 10\ndata: {"type":"stream.deleted","resource_id":"a","occurred_at":"t","data":{}}\n\n', { status: 200 });
    return problem(401, 'AUTH_FAILED');
  });
  const c = new SencaClient({ baseUrl: 'http://node.test', token: 'senca_x', fetch });
  const got = [];
  await assert.rejects(async () => {
    for await (const ev of c.events({ types: ['stream.*'], reconnect: { initialDelayMs: 1, maxDelayMs: 2 } })) got.push([ev.type, ev.id]);
  }, (e) => e instanceof ApiError && e.status === 401);
  assert.deepEqual(got, [['stream.created', '1'], ['resync', '9'], ['stream.deleted', '10']]);
  assert.equal(calls[0].headers.get('accept'), 'text/event-stream');
  assert.equal(calls[0].headers.get('last-event-id'), null);
  assert.equal(calls[1].headers.get('last-event-id'), '2');
  assert.equal(calls[2].headers.get('last-event-id'), '10');
});

test('events(): signal で終わる', async () => {
  const ac = new AbortController();
  const { fetch } = fakeFetch((req) => new Response(new ReadableStream({
    start(ctl) {
      ctl.enqueue(new TextEncoder().encode('event: stream.updated\nid: 5\ndata: {"type":"stream.updated","resource_id":"a","occurred_at":"t","data":{}}\n\n'));
      req.signal.addEventListener('abort', () => ctl.error(req.signal.reason));
    },
  }), { status: 200 }));
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch });
  const got = [];
  for await (const ev of c.events({ signal: ac.signal })) { got.push(ev.data.resource_id); ac.abort(); }
  assert.deepEqual(got, ['a']);
});

// 仕様のすべての操作に SDK のメソッドがある（仕様が増えたら、ここと SDK を足す）
const COVERAGE = {
  getCapabilities: (c) => c.capabilities,
  listNodes: (c) => c.nodes.list, getNode: (c) => c.nodes.get, drainNode: (c) => c.nodes.drain, undrainNode: (c) => c.nodes.undrain,
  disableDevice: (c) => c.nodes.devices.disable, enableDevice: (c) => c.nodes.devices.enable,
  listDevices: (c) => c.devices, getGrid: (c) => c.grid, streamEvents: (c) => c.events,
  createStream: (c) => c.streams.create, listStreams: (c) => c.streams.list, getStream: (c) => c.streams.get, patchStream: (c) => c.streams.update,
  deleteStream: (c) => c.streams.delete, rotateStreamKey: (c) => c.streams.rotateKey, createStreamPlaybackUrl: (c) => c.streams.playbackUrl,
  startRecording: (c) => c.streams.startRecording, planStream: (c) => c.streams.plan, getStreamSnapshot: (c) => c.streams.snapshot,
  listStreamSnapshots: (c) => c.streams.snapshots.list, getStreamSnapshotById: (c) => c.streams.snapshots.get, deleteStreamSnapshot: (c) => c.streams.snapshots.delete,
  listRecordings: (c) => c.recordings.list, getRecording: (c) => c.recordings.get, patchRecording: (c) => c.recordings.update, deleteRecording: (c) => c.recordings.delete,
  stopRecording: (c) => c.recordings.stop, createRecordingPlaybackUrl: (c) => c.recordings.playbackUrl, getRecordingThumbnail: (c) => c.recordings.thumbnail,
  exportRecordingMp4: (c) => c.recordings.exportMp4,
  createSchedule: (c) => c.schedules.create, listSchedules: (c) => c.schedules.list, checkSchedule: (c) => c.schedules.check, getSchedule: (c) => c.schedules.get,
  patchSchedule: (c) => c.schedules.update, deleteSchedule: (c) => c.schedules.delete, listScheduleRuns: (c) => c.schedules.runs,
  listAudit: (c) => c.audit.list,
  createToken: (c) => c.tokens.create, listTokens: (c) => c.tokens.list, getToken: (c) => c.tokens.get, deleteToken: (c) => c.tokens.delete,
  createInput: (c) => c.inputs.create, listInputs: (c) => c.inputs.list, getInput: (c) => c.inputs.get, patchInput: (c) => c.inputs.update, deleteInput: (c) => c.inputs.delete,
  createOutput: (c) => c.outputs.create, listOutputs: (c) => c.outputs.list, getOutput: (c) => c.outputs.get, patchOutput: (c) => c.outputs.update, deleteOutput: (c) => c.outputs.delete,
  createSecret: (c) => c.secrets.create, listSecrets: (c) => c.secrets.list, deleteSecret: (c) => c.secrets.delete,
  createPipeline: (c) => c.pipelines.create, listPipelines: (c) => c.pipelines.list, getPipeline: (c) => c.pipelines.get, patchPipeline: (c) => c.pipelines.update,
  deletePipeline: (c) => c.pipelines.delete, validatePipeline: (c) => c.pipelines.validate, controlPipeline: (c) => c.pipelines.control,
  listPipelineSnapshots: (c) => c.pipelines.snapshots.list, getPipelineSnapshotById: (c) => c.pipelines.snapshots.get, deletePipelineSnapshot: (c) => c.pipelines.snapshots.delete,
  createRoom: (c) => c.rooms.create, listRooms: (c) => c.rooms.list, getRoom: (c) => c.rooms.get, patchRoom: (c) => c.rooms.update, deleteRoom: (c) => c.rooms.delete,
  issueRoomToken: (c) => c.rooms.createToken, postRoomMessage: (c) => c.rooms.sendMessage, getRoomStats: (c) => c.rooms.stats,
  listRoomParticipants: (c) => c.rooms.participants('r').list, getRoomParticipant: (c) => c.rooms.participants('r').get,
  patchRoomParticipant: (c) => c.rooms.participants('r').updateMetadata, disconnectRoomParticipant: (c) => c.rooms.participants('r').disconnect,
  createMediaPipeline: (c) => c.mediaPipelines.create, listMediaPipelines: (c) => c.mediaPipelines.list, getMediaPipeline: (c) => c.mediaPipelines.get,
  patchMediaPipeline: (c) => c.mediaPipelines.update, deleteMediaPipeline: (c) => c.mediaPipelines.delete,
  getAuthProviders: (c) => c.auth.providers, login: (c) => c.auth.login, loginMfa: (c) => c.auth.loginMfa, exchangeLoginCode: (c) => c.auth.exchange,
  startOidcLogin: (c) => c.auth.oidcStartUrl, oidcCallback: (c) => c.auth.oidcCallbackUrl, getMe: (c) => c.auth.me, logout: (c) => c.auth.logout,
  changePassword: (c) => c.auth.changePassword, setupTotp: (c) => c.auth.setupTotp, confirmTotp: (c) => c.auth.confirmTotp, disableMfa: (c) => c.auth.disableMfa,
  regenerateRecoveryCodes: (c) => c.auth.regenerateRecoveryCodes, listMySessions: (c) => c.auth.sessions, revokeMySession: (c) => c.auth.revokeSession,
  createUser: (c) => c.users.create, listUsers: (c) => c.users.list, getUser: (c) => c.users.get, updateUser: (c) => c.users.update, deleteUser: (c) => c.users.delete,
  resetUserPassword: (c) => c.users.resetPassword, resetUserMfa: (c) => c.users.resetMfa, listUserSessions: (c) => c.users.sessions,
  revokeUserSessions: (c) => c.users.revokeSessions,
};

test('openapi.yaml のすべての操作に SDK のメソッドがある', () => {
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch: async () => json(200, {}) });
  const missing = Object.keys(OPERATIONS).filter((id) => !COVERAGE[id]);
  assert.deepEqual(missing, [], `SDK に無い操作（scripts/gen_types.py で生成した表）: ${missing.join(', ')}`);
  for (const [id, f] of Object.entries(COVERAGE)) {
    assert.ok(OPERATIONS[id], `仕様に無い操作 ${id}`);
    assert.equal(typeof f(c), 'function', id);
  }
});

test('操作のパスと SDK の要求のパスが一致する（代表）', async () => {
  const { fetch, calls } = fakeFetch((req) => json(req.method === 'POST' ? 201 : 200, { items: [], next_cursor: null, id: 'x', generation: 1 }));
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch });
  const expect = async (opId, run, params) => {
    calls.length = 0;
    await run();
    const op = OPERATIONS[opId];
    const path = op.path.replace(/\{(\w+)\}/g, (_, k) => params[k]);
    assert.equal(calls[0].method, op.method, opId);
    assert.equal(calls[0].url.pathname, `/api/v1${path}`, opId);
  };
  await expect('createInput', () => c.inputs.create('s1', { protocol: 'rtsp', url: 'rtsp://h/x' }), { id: 's1' });
  await expect('listOutputs', () => c.outputs.list('s1').first(), { id: 's1' });
  await expect('patchInput', () => c.inputs.update('i1', {}, { ifMatch: 1 }), { id: 'i1' });
  await expect('controlPipeline', () => c.pipelines.control('p1', { op: 'take', node: 'mix' }), { id: 'p1' });
  await expect('deletePipelineSnapshot', () => c.pipelines.snapshots.delete('p1', 'sn1'), { id: 'p1', sid: 'sn1' });
  await expect('disableDevice', () => c.nodes.devices.disable('n1', 'nvidia:0'), { id: 'n1', dev: 'nvidia%3A0' });
  await expect('listScheduleRuns', () => c.schedules.runs('sc1'), { id: 'sc1' });
  await expect('deleteStreamSnapshot', () => c.streams.snapshots.delete('s1', 'sn1'), { id: 's1', sid: 'sn1' });
  await expect('patchRoomParticipant', () => c.rooms.participants('daily').updateMetadata('c1', { a: 1 }), { id: 'daily', c: 'c1' });
  await expect('issueRoomToken', () => c.rooms.createToken('daily', { role: 'sendrecv' }), { id: 'daily' });
  await expect('createMediaPipeline', () => c.mediaPipelines.create({ name: 'm', room: 'daily', composite: {} }), {});
  await expect('getMediaPipeline', () => c.mediaPipelines.get('m1'), { id: 'm1' });
  await expect('patchMediaPipeline', () => c.mediaPipelines.update('m1', { egress: [] }, { ifMatch: 1 }), { id: 'm1' });
  await expect('deleteMediaPipeline', () => c.mediaPipelines.delete('m1'), { id: 'm1' });
  await expect('loginMfa', () => c.auth.loginMfa({ mfa_token: 't', code: '123456' }), {});
  await expect('revokeMySession', () => c.auth.revokeSession('s1'), { id: 's1' });
  await expect('updateUser', () => c.users.update('u1', { disabled: true }, { ifMatch: 1 }), { id: 'u1' });
  await expect('resetUserPassword', () => c.users.resetPassword('u1'), { id: 'u1' });
  await expect('resetUserMfa', () => c.users.resetMfa('u1'), { id: 'u1' });
  await expect('listUserSessions', () => c.users.sessions('u1').first(), { id: 'u1' });
  await expect('revokeUserSessions', () => c.users.revokeSessions('u1'), { id: 'u1' });
});

test('auth: ログインは token なし・Idempotency-Key なし、exchange はクッキーを送る、setToken でセッションに替える', async () => {
  const { fetch, calls } = fakeFetch((req) => json(200, req.url.pathname === '/api/v1/auth/login'
    ? { status: 'ok', token: 'sencas_abc', pending: [] }
    : { status: 'ok', token: 'sencas_def', pending: [], return_to: '/console/' }));
  const seen = [];
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch: (u, init) => { seen.push(init.credentials); return fetch(u, init); } });
  const r = await c.auth.login({ username: 'alice', password: 'pw' });
  assert.equal(r.status, 'ok');
  assert.equal(calls[0].headers.get('authorization'), null);
  assert.equal(calls[0].headers.get('idempotency-key'), null);
  await c.auth.exchange('code1', { cookie: 'senca_oidc=x' });
  assert.equal(calls[1].url.pathname, '/api/v1/auth/exchange');
  assert.deepEqual(calls[1].body, { code: 'code1' });
  assert.deepEqual(seen, ['omit', 'same-origin']);
  c.setToken(r.token);
  await c.auth.me();
  assert.equal(calls[2].headers.get('authorization'), 'Bearer sencas_abc');
  assert.equal(c.auth.oidcStartUrl('kc', '/console/#/x'), 'http://node.test/api/v1/auth/oidc/kc/start?return_to=%2Fconsole%2F%23%2Fx');
  assert.equal(c.auth.oidcCallbackUrl('kc'), 'http://node.test/api/v1/auth/oidc/kc/callback');
});

test('mediaPipelines: list の room、stop は読んで If-Match で desired_state を書く', async () => {
  const { fetch, calls } = fakeFetch((req) => (req.method === 'GET' && req.url.pathname === '/api/v1/media-pipelines'
    ? json(200, { items: [{ id: 'm1', room: 'daily', generation: 1 }], next_cursor: null })
    : json(200, { id: 'm1', generation: 3, desired_state: 'stopped' }, { ETag: '"3"' })));
  const c = new SencaClient({ baseUrl: 'http://node.test', fetch });
  const items = [];
  for await (const m of c.mediaPipelines.list({ room: 'daily', limit: 10 })) items.push(m.id);
  assert.deepEqual(items, ['m1']);
  assert.equal(calls[0].url.searchParams.get('room'), 'daily');
  assert.equal(calls[0].url.searchParams.get('limit'), '10');
  calls.length = 0;
  await c.mediaPipelines.stop('m1');
  assert.deepEqual(calls.map((x) => x.method), ['GET', 'PATCH']);
  assert.equal(calls[1].headers.get('if-match'), '"3"');
  assert.deepEqual(calls[1].body, { desired_state: 'stopped' });
  calls.length = 0;
  await c.mediaPipelines.start('m1', { ifMatch: 2 });
  assert.deepEqual(calls.map((x) => x.method), ['PATCH']);
  assert.equal(calls[0].headers.get('if-match'), '"2"');
  assert.deepEqual(calls[0].body, { desired_state: 'running' });
});
