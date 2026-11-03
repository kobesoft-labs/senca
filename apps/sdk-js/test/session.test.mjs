// Session（シグナリング v1 の状態機械）の試験。偽の WebSocket・RTCPeerConnection と台本のサーバーで動かす。
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { Senca, SencaError } from '../src/index.js';
import { FakeServer, FakeTrack, FakeMediaStream, makeEnv, participant, parseFakeSdp, tick, until } from './fakes.mjs';

const URL = 'wss://node.test/signaling';

function setup(opts = {}) {
  const server = new FakeServer(opts);
  const { environment, pcs } = makeEnv(server);
  const connect = (o = {}) => Senca.connect({ url: URL, room: 'daily', token: 't0k', clientId: 'alice', environment, reconnect: { initialDelayMs: 5, maxDelayMs: 20 }, ...o });
  return { server, pcs, environment, connect };
}

/** AbortSignal.timeout は Node.js でイベントループを保たないので、普通のタイマーで */
function abortAfter(ms) {
  const ac = new AbortController();
  setTimeout(() => ac.abort(), ms);
  return ac.signal;
}

const camera = () => new FakeMediaStream([new FakeTrack('video'), new FakeTrack('audio')]);

test('参加: connect の中身、最初の offer の受ける枠、answer の枠から RemoteTrack', async () => {
  const bob = participant('b1', 'bob', ['audio', 'video'], ['r0', 'r1']);
  const { server, connect, pcs } = setup({ participants: [bob] });
  const s = await connect({ dataChannels: ['#chat', { label: '#game', ordered: false, maxRetransmits: 0 }], publish: { stream: camera(), simulcast: true, maxBitrateKbps: 1200 } });
  const c = server.last.connect;
  assert.equal(c.version, 1);
  assert.equal(c.room_id, 'daily');
  assert.equal(c.token, 't0k');
  assert.equal(c.role, 'sendrecv');
  assert.equal(c.sdk.name, '@senca/sdk');
  assert.deepEqual(c.simulcast, { rids: ['r0', 'r1', 'r2'] });
  assert.deepEqual(c.video, { enabled: true, bitrate_kbps: 1200 });
  assert.deepEqual(c.data_channels, [{ label: '#chat', direction: 'sendrecv', ordered: true }, { label: '#game', direction: 'sendrecv', ordered: false, max_retransmits: 0 }]);
  // 未知のフィールドは送らない（サーバーは bad_message にする）
  for (const k of Object.keys(c)) assert.ok(['type', 'version', 'room_id', 'token', 'role', 'client_id', 'sdk', 'simulcast', 'video', 'data_channels'].includes(k), k);

  assert.equal(s.state, 'connected');
  assert.equal(s.connectionId, 'c1');
  assert.equal(s.room.videoCodec, 'vp8');
  assert.equal(server.last.offers.length, 1, '最初の offer は 1 つ');
  const ms = parseFakeSdp(server.last.offers[0]);
  assert.deepEqual(ms.filter((m) => m.direction === 'sendonly').map((m) => [m.kind, m.rids.join(';')]), [['video', 'r0;r1;r2'], ['audio', '']]);
  assert.deepEqual(ms.filter((m) => m.direction === 'recvonly').map((m) => m.kind).sort(), ['audio', 'video']);
  assert.ok(server.last.offers[0].includes('m=application'));
  // コーデックの優先順は部屋の VP8（＋rtx）
  const vt = pcs[0].transceivers.find((t) => t.kind === 'video' && t.direction === 'recvonly');
  assert.deepEqual(vt.codecPrefs.map((x) => x.mimeType), ['video/VP8', 'video/rtx']);

  assert.equal(s.participants.size, 1);
  const p = s.participants.get('b1');
  assert.equal(p.clientId, 'bob');
  assert.deepEqual(p.tracks.get('b1.1').rids, ['r0', 'r1']);
  assert.equal(s.tracks.size, 2);
  const got = [];
  for await (const t of s.remoteTracks({ kind: 'video', signal: abortAfter(50) })) got.push(t);
  assert.equal(got.length, 1);
  assert.equal(got[0].participant, p);
  assert.equal(got[0].kind, 'video');
  assert.ok(got[0].stream instanceof FakeMediaStream);
  assert.deepEqual(p.subscriptions.map((t) => t.id).sort(), ['b1.0', 'b1.1']);
  // 公開したトラックにサーバーの ID
  const pub = s.publications[0];
  assert.match(pub.video.id, /^c1\.\d+$/);
  assert.deepEqual([...pub.video.rids], ['r0', 'r1', 'r2']);
  await s.close();
  assert.deepEqual(server.last.received.at(-1), { type: 'disconnect', reason: 'normal' });
  assert.deepEqual(await s.closed, { reason: 'normal', error: undefined });
});

test('renegotiate: offer は 1 つずつ。answer を待つ間の renegotiate は answer の後で 1 回にまとめる', async () => {
  const { server, connect, pcs } = setup({ recvSlots: { audio: 1, video: 1 } });
  /** @type {any} */
  let held = null;
  server.script.onOffer = (c, m) => {
    if (c.offers.length === 2 && !held) { held = m; return; } // 2 つ目の answer を遅らせる
    server.answer(c, m.sdp);
  };
  const s = await connect();
  server.push(server.last, { type: 'renegotiate', audio: 2, video: 2 });
  await until(() => server.last.offers.length === 2, 1000, '2nd offer');
  // 答える前にさらに 2 回
  server.push(server.last, { type: 'renegotiate', audio: 3, video: 3 });
  server.push(server.last, { type: 'renegotiate', audio: 3, video: 4 });
  await tick(20);
  assert.equal(server.last.offers.length, 2, 'answer の前に次の offer を出さない');
  server.answer(server.last, held.sdp);
  await until(() => server.last.offers.length === 3, 1000, '3rd offer');
  await tick(20);
  assert.equal(server.last.offers.length, 3);
  const recv = parseFakeSdp(server.last.offers[2]).filter((m) => m.direction === 'recvonly');
  assert.equal(recv.filter((m) => m.kind === 'audio').length, 3);
  assert.equal(recv.filter((m) => m.kind === 'video').length, 4);
  assert.equal(pcs[0].signalingState, 'stable');
  await s.close();
});

test('枠の割り当ての変化: 一時停止・別の送り手へ・空き', async () => {
  const bob = participant('b1', 'bob', ['video']);
  const { server, connect } = setup({ participants: [bob], recvSlots: { audio: 0, video: 1 } });
  const s = await connect();
  const events = s.events();
  const [t1] = [...s.tracks.values()].filter((t) => t.kind === 'video');
  const mid = t1.mid;
  const trackEvents = t1.events();
  server.push(server.last, { type: 'slots', slots: [{ mid, kind: 'video', state: 'paused', track_id: 'b1.0', connection_id: 'b1', client_id: 'bob' }] });
  await until(() => t1.state === 'paused');
  // carol が入り、枠が carol へ
  server.push(server.last, { type: 'notify', event: 'connection.created', participant: participant('c9', 'carol', ['video']) });
  server.push(server.last, { type: 'slots', slots: [{ mid, kind: 'video', state: 'active', track_id: 'c9.0', connection_id: 'c9', client_id: 'carol' }] });
  await until(() => s.tracks.has('c9.0'));
  assert.equal(t1.state, 'ended');
  assert.deepEqual(await t1.ended, { reason: 'unsubscribed' });
  const t2 = s.tracks.get('c9.0');
  assert.equal(t2.participant.clientId, 'carol');
  assert.equal(t2.mediaStreamTrack, t1.mediaStreamTrack, '枠（m 行）は使い回す');
  server.push(server.last, { type: 'slots', slots: [{ mid, kind: 'video', state: 'empty' }] });
  await until(() => s.tracks.size === 0);
  const seen = [];
  for await (const e of trackEvents) seen.push(e.type);
  assert.deepEqual(seen, ['paused', 'ended']);
  await s.close();
  const types = [];
  for await (const e of events) types.push(e.type);
  assert.deepEqual(types, ['track.paused', 'participant.joined', 'track.unsubscribed', 'track.subscribed', 'track.unsubscribed', 'closed']);
});

test('層の選択: layer を送り、layer.changed で解決する。無い層・誤りは失敗', async () => {
  const bob = participant('b1', 'bob', ['video'], ['r0', 'r1', 'r2']);
  const { server, connect } = setup({ participants: [bob], recvSlots: { audio: 0, video: 1 } });
  const s = await connect();
  const t = s.tracks.get('b1.0');
  assert.deepEqual([...t.rids], ['r0', 'r1', 'r2']);
  server.script.onMessage = (c, m) => {
    if (m.type === 'layer' && m.rid === 'r2') setTimeout(() => server.push(c, { type: 'notify', event: 'layer.changed', track_id: m.track_id, mid: t.mid, rid: 'r2' }), 10);
    if (m.type === 'layer' && m.rid === 'r1') server.push(c, { type: 'error', code: 'unknown_track', message: 'the track is not subscribed', fatal: false, request: 'layer' });
  };
  await t.setLayer('r2');
  assert.equal(t.layer, 'r2');
  assert.deepEqual(server.last.received.filter((m) => m.type === 'layer'), [{ type: 'layer', track_id: 'b1.0', rid: 'r2' }]);
  await assert.rejects(t.setLayer('r1'), (e) => e instanceof SencaError && e.code === 'unknown_track' && !e.fatal);
  await assert.rejects(t.setLayer('r9'), (e) => e.code === 'invalid_argument');
  await t.setLayer('auto');
  assert.deepEqual(server.last.received.at(-1), { type: 'layer', track_id: 'b1.0', rid: null });
  // 届かなければ時間切れ
  await assert.rejects(t.setLayer('r0', { timeoutMs: 50 }), (e) => e.code === 'timeout');
  // 取り消し
  const ac = new AbortController();
  const p = t.setLayer('r0', { signal: ac.signal, timeoutMs: 5000 });
  ac.abort();
  await assert.rejects(p, (e) => e.name === 'AbortError');
  // receiveLayer
  await s.close();
});

test('message: DataChannel と シグナリング、宣言・大きさの検査', async () => {
  const { server, connect, pcs } = setup({ recvSlots: { audio: 0, video: 0 } });
  const s = await connect({ dataChannels: ['#chat', { label: '#in', direction: 'recvonly' }] });
  const dc = pcs[0].dcs.find((d) => d.label === '#chat');
  await until(() => dc.readyState === 'open');
  await s.send('#chat', 'hello');
  await s.send('#chat', { n: 1 });
  await s.send('#chat', new Uint8Array([1, 2, 3]));
  assert.deepEqual(dc.sent.slice(0, 2), ['hello', '{"n":1}']);
  assert.deepEqual([...dc.sent[2]], [1, 2, 3]);
  // 宛先つきはシグナリング
  await s.send('#chat', new Uint8Array([255, 0]), { to: ['b1'] });
  assert.deepEqual(server.last.received.at(-1), { type: 'message', label: '#chat', data_base64: '/wA=', to: ['b1'] });
  await assert.rejects(s.send('#nope', 'x'), (e) => e.code === 'unknown_label');
  await assert.rejects(s.send('#in', 'x'), (e) => e.code === 'unknown_label');
  await assert.rejects(s.send('#chat', 'x'.repeat(64 * 1024 + 1)), (e) => e.code === 'message_too_large');

  const it = s.messages('#chat');
  dc._receive('from dc');
  server.push(server.last, { type: 'message', label: '#chat', from: 'b1', data: '{"a":2}' });
  server.push(server.last, { type: 'message', label: '#other', from: 'b1', data: 'skip' });
  server.push(server.last, { type: 'message', label: '#chat', from: 'b1', data_base64: 'AQI=' });
  const a = (await it.next()).value;
  assert.equal(a.text(), 'from dc');
  assert.equal(a.from, null);
  assert.equal(a.via, 'datachannel');
  const b = (await it.next()).value;
  assert.deepEqual(b.json(), { a: 2 });
  assert.equal(b.from, 'b1');
  const c = (await it.next()).value;
  assert.ok(c.binary);
  assert.deepEqual([...c.data], [1, 2]);
  await s.close();
  assert.equal((await it.next()).done, true, '閉じると反復は終わる');
});

test('fatal な誤りで参加に失敗する（再接続しない）', async () => {
  const { server, connect } = setup();
  server.script.onConnect = (c) => {
    server.push(c, { type: 'error', code: 'auth_failed', message: 'token expired', fatal: true, request: 'connect' });
    server.push(c, { type: 'disconnect', reason: 'error' });
    setTimeout(() => c.ws._serverClose(4401, 'auth_failed'), 1);
  };
  await assert.rejects(connect(), (e) => e instanceof SencaError && e.code === 'auth_failed' && e.fatal && e.request === 'connect');
  await tick(30);
  assert.equal(server.conns.length, 1);
});

test('close code だけで閉じられた（4403）→ forbidden', async () => {
  const { server, connect } = setup();
  server.script.onConnect = (c) => c.ws._serverClose(4403, 'forbidden');
  await assert.rejects(connect(), (e) => e.code === 'forbidden' && e.closeCode === 4403);
});

test('再接続: シグナリングが切れたら入り直し、公開を張り直す。トークンは取り直す', async () => {
  const bob = participant('b1', 'bob', ['video']);
  const { server, connect } = setup({ participants: [bob], recvSlots: { audio: 0, video: 1 } });
  let calls = 0;
  const s = await connect({ token: async () => `tok${++calls}`, publish: camera() });
  const events = s.events();
  const oldTrack = s.tracks.get('b1.0');
  const pub = s.publications[0];
  const firstId = pub.video.id;
  server.last.ws._serverClose(1006, '');
  await until(() => s.state === 'connected' && s.connectionId === 'c2', 2000, 'reconnected');
  assert.equal(server.conns.length, 2);
  assert.equal(server.conns[1].connect.token, 'tok2');
  assert.equal(oldTrack.state, 'ended');
  assert.deepEqual(await oldTrack.ended, { reason: 'reconnecting' });
  assert.ok(s.tracks.get('b1.0') !== oldTrack && s.tracks.get('b1.0').state === 'active');
  const ms = parseFakeSdp(server.conns[1].offers[0]);
  assert.equal(ms.filter((m) => m.direction === 'sendonly').length, 2, '公開を最初の offer に入れ直す');
  assert.notEqual(pub.video.id, firstId);
  assert.match(pub.video.id, /^c2\./);
  const types = [];
  for await (const e of events) { types.push(e.type); if (e.type === 'reconnected') break; }
  assert.deepEqual(types, ['track.unsubscribed', 'reconnecting', 'track.subscribed', 'reconnected']);
  await s.close();
});

test('再接続: あきらめたら connection_failed で閉じる', async () => {
  const { server, connect } = setup({ recvSlots: { audio: 0, video: 0 } });
  const s = await connect({ reconnect: { maxAttempts: 2, initialDelayMs: 1, maxDelayMs: 2 } });
  server.refuse = true;
  server.last.ws._serverClose(1006, '');
  const info = await s.closed;
  assert.equal(info.reason, 'connection_failed');
  assert.equal(info.error.code, 'connection_failed');
  assert.equal(server.conns.length, 3);
});

test('kicked・room_deleted は再接続しない', async () => {
  for (const reason of ['kicked', 'room_deleted']) {
    const { server, connect } = setup({ recvSlots: { audio: 0, video: 0 } });
    const s = await connect();
    server.push(server.last, { type: 'disconnect', reason });
    setTimeout(() => server.last.ws._serverClose(1000), 1);
    const info = await s.closed;
    assert.equal(info.reason, reason);
    assert.equal(info.error.code, reason);
    assert.equal(server.conns.length, 1);
  }
});

test('redirect: 参加の前に別のノードへ、参加の後なら待たずに入り直す', async () => {
  const { server, connect } = setup({ recvSlots: { audio: 0, video: 0 } });
  server.script.onConnect = (c) => {
    if (c.url.includes('node.test')) {
      server.push(c, { type: 'redirect', location: 'https://node-b.test' });
      server.push(c, { type: 'disconnect', reason: 'redirected' });
      setTimeout(() => c.ws._serverClose(1000), 1);
      return;
    }
    server.sendConnected(c);
  };
  const s = await connect();
  assert.equal(server.last.url, 'wss://node-b.test/signaling');
  assert.equal(s.connectionId, 'c2');
  // 参加の後の redirect
  server.script.onConnect = undefined;
  server.push(server.last, { type: 'redirect', location: 'wss://node-c.test/signaling' });
  server.push(server.last, { type: 'disconnect', reason: 'redirected' });
  setTimeout(() => server.conns[1].ws._serverClose(1000), 1);
  const ev = await s.waitFor('reconnected', { timeoutMs: 2000 });
  assert.equal(ev.connectionId, 'c3');
  assert.equal(server.last.url, 'wss://node-c.test/signaling');
  await s.close();
});

test('ping に pong（reportStats なら統計を載せる）、stats() の要約', async () => {
  const { server, connect } = setup({ recvSlots: { audio: 0, video: 1 }, participants: [participant('b1', 'bob', ['video'])] });
  const s = await connect({ reportStats: { intervalMs: 10 } });
  await tick(40);
  server.push(server.last, { type: 'ping' });
  await until(() => server.last.received.some((m) => m.type === 'pong'));
  const pong = server.last.received.find((m) => m.type === 'pong');
  assert.ok(pong.stats && Array.isArray(pong.stats.inbound), 'pong に統計');
  const st = await s.stats();
  assert.equal(st.inbound[0].trackId, 'b1.0');
  assert.equal(st.inbound[0].framesDecoded, 42);
  assert.equal(st.inbound[0].clientId, 'bob');
  assert.equal(st.transport.rttMs, 12);
  assert.equal((await s.tracks.get('b1.0').stats()).frameWidth, 640);
  await s.close();
});

test('シグナリングを DataChannel へ切り替え、WebSocket を閉じる', async () => {
  const { server, connect, pcs } = setup({ recvSlots: { audio: 0, video: 0 } });
  const sigOut = [];
  const s = await connect({ dataChannels: ['#chat'], signaling: { dataChannel: true, closeWebSocket: true } });
  assert.deepEqual(server.last.connect.signaling, { data_channel: true, close_websocket: true });
  assert.ok(server.last.connect.data_channels.some((d) => d.label === 'signaling'));
  const sig = pcs[0].dcs.find((d) => d.label === 'signaling');
  await until(() => sig.readyState === 'open');
  pcs[0].onDcSend = (dc, d) => { if (dc.label === 'signaling') sigOut.push(JSON.parse(d)); };
  server.push(server.last, { type: 'switched' });
  await s.waitFor('signaling.switched', { timeoutMs: 500 });
  await until(() => server.last.ws.readyState === 3);
  assert.equal(s.state, 'connected', 'WebSocket を閉じても続く');
  sig._receive(JSON.stringify({ type: 'ping' }));
  await until(() => sigOut.some((m) => m.type === 'pong'));
  await s.close();
  assert.deepEqual(sigOut.at(-1), { type: 'disconnect', reason: 'normal' });
});

test('ICE が failed → ICE restart の offer（iceRestart）を出す', async () => {
  const { server, connect, pcs } = setup({ recvSlots: { audio: 0, video: 1 } });
  const s = await connect();
  const restarted = s.waitFor('ice.restarted', { timeoutMs: 1000 });
  pcs[0]._setState('failed');
  await until(() => server.last.offers.length === 2);
  assert.ok(server.last.offers[1].includes('a=ice-restart'));
  await restarted;
  assert.equal(s.state, 'connected');
  await s.close();
});

test('publish と unpublish（参加の後）', async () => {
  const { server, connect, pcs } = setup({ recvSlots: { audio: 0, video: 0 }, room: { max_video_bitrate_kbps: 800 } });
  const s = await connect();
  assert.equal(server.last.offers.length, 1);
  await assert.rejects(s.publish([]), (e) => e.code === 'invalid_argument');
  await assert.rejects(s.publish(camera(), { simulcast: [{ rid: 'a-b' }] }), RangeError);
  const pub = await s.publish(camera(), { simulcast: true });
  assert.equal(server.last.offers.length, 2);
  const v = pcs[0].transceivers.find((t) => t.direction === 'sendonly' && t.kind === 'video');
  assert.deepEqual(v.encodings.map((e) => [e.rid, e.scaleResolutionDownBy, e.maxBitrate]), [['r0', 4, 150000], ['r1', 2, 500000], ['r2', 1, 800000]]);
  assert.ok(pub.video.id && pub.audio.id);
  await pub.setMaxBitrate(400);
  assert.deepEqual(v.sender.params.encodings.map((e) => e.maxBitrate), [75000, 250000, 400000]);
  await assert.rejects(s.publish(camera(), { codec: 'h264' }), (e) => e.code === 'unsupported_codec');
  await pub.unpublish();
  assert.equal(v.direction, 'inactive');
  assert.equal(server.last.offers.length, 3);
  assert.equal(parseFakeSdp(server.last.offers[2]).filter((m) => m.direction === 'sendonly').length, 0);
  assert.equal(s.publications.length, 0);
  await s.close();
});

test('recvonly は公開できない、引数の誤り', async () => {
  const { connect } = setup({ recvSlots: { audio: 0, video: 0 } });
  const s = await connect({ role: 'recvonly' });
  await assert.rejects(s.publish(camera()), (e) => e.code === 'role_violation');
  await s.close();
  await assert.rejects(s.send('#x', 'y'), (e) => e.code === 'closed');
  await assert.rejects(Senca.connect({ url: 'ftp://x', room: 'r', environment: {} }), (e) => e.code === 'unsupported' || e.code === 'invalid_argument');
});

test('参加中の取り消し（AbortSignal）と、参加の後の取り消しで閉じる', async () => {
  const { server, connect } = setup({ recvSlots: { audio: 0, video: 0 } });
  server.script.onConnect = () => {}; // 答えない
  const ac = new AbortController();
  const p = connect({ signal: ac.signal });
  await tick(10);
  ac.abort();
  await assert.rejects(p, (e) => e.name === 'AbortError');
  server.script.onConnect = undefined;
  const ac2 = new AbortController();
  const s = await connect({ signal: ac2.signal });
  ac2.abort();
  assert.equal((await s.closed).reason, 'aborted');
});

test('参加者と spotlight・network・push の出来事', async () => {
  const { server, connect } = setup({ recvSlots: { audio: 0, video: 0 }, participants: [participant('b1', 'bob', [])] });
  const s = await connect();
  const it = s.events({ types: ['participant.updated', 'participant.left', 'spotlight.changed', 'network', 'push', 'track.published', 'track.unpublished'] });
  server.push(server.last, { type: 'notify', event: 'connection.updated', connection_id: 'b1', metadata: { name: 'Bobby' } });
  server.push(server.last, { type: 'notify', event: 'track.added', track: { track_id: 'b1.0', connection_id: 'b1', kind: 'video', rids: ['r0'] } });
  server.push(server.last, { type: 'notify', event: 'spotlight.changed', focused: ['b1'] });
  server.push(server.last, { type: 'notify', event: 'network.status', level: 2, bandwidth_bps: 300000, rtt_ms: 80 });
  server.push(server.last, { type: 'push', data: { hello: 1 } });
  server.push(server.last, { type: 'notify', event: 'track.removed', track_id: 'b1.0', connection_id: 'b1' });
  server.push(server.last, { type: 'notify', event: 'connection.destroyed', connection_id: 'b1', client_id: 'bob', reason: 'normal' });
  const got = [];
  for await (const e of it) { got.push(e); if (e.type === 'participant.left') break; }
  assert.deepEqual(got.map((e) => e.type), ['participant.updated', 'track.published', 'spotlight.changed', 'network', 'push', 'track.unpublished', 'participant.left']);
  assert.deepEqual(got[0].participant.metadata, { name: 'Bobby' });
  assert.deepEqual(s.spotlight.focused, ['b1']);
  assert.deepEqual(s.network, { level: 2, bandwidthBps: 300000, rttMs: 80 });
  assert.equal(s.participants.size, 0);
  await s.close();
});

test('URL の正規化', async () => {
  const { normalizeUrl } = await import('../src/realtime/session.js');
  assert.equal(normalizeUrl('https://n.example'), 'wss://n.example/signaling');
  assert.equal(normalizeUrl('http://127.0.0.1:8080/'), 'ws://127.0.0.1:8080/signaling');
  assert.equal(normalizeUrl('wss://n.example/rooms/signaling'), 'wss://n.example/rooms/signaling');
  assert.throws(() => normalizeUrl('ftp://x'), SencaError);
});
