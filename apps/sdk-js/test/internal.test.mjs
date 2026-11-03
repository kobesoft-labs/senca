// 小道具の試験: サイマルキャストの層、コーデックの優先順、バックオフ、出来事の配り。
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { simulcastLayers, encodingsFor, preferCodec } from '../src/realtime/media.js';
import { backoffDelay, sleep } from '../src/internal/async.js';
import { Broadcaster } from '../src/internal/broadcast.js';
import { normalizeDataChannels } from '../src/realtime/session.js';
import { CAPS } from './fakes.mjs';

test('サイマルキャストの層: 既定・上限・検査', () => {
  assert.deepEqual(simulcastLayers(true).map((l) => l.rid), ['r0', 'r1', 'r2']);
  assert.equal(simulcastLayers(false), null);
  const capped = simulcastLayers(true, 400);
  assert.deepEqual(capped.map((l) => l.maxBitrateKbps), [150, 400, 400]);
  assert.deepEqual(encodingsFor([{ rid: 'lo' }, { rid: 'hi', maxBitrateKbps: 900, maxFramerate: 30 }]),
    [{ rid: 'lo', active: true, scaleResolutionDownBy: 2 }, { rid: 'hi', active: true, scaleResolutionDownBy: 1, maxBitrate: 900000, maxFramerate: 30 }]);
  assert.throws(() => simulcastLayers([]), RangeError);
  assert.throws(() => simulcastLayers([{ rid: 'a' }, { rid: 'a' }]), RangeError);
  assert.throws(() => simulcastLayers([{ rid: 'x'.repeat(17) }]), RangeError);
  assert.throws(() => simulcastLayers(Array.from({ length: 5 }, (_, i) => ({ rid: `r${i}` }))), RangeError);
});

test('コーデックの優先順: 部屋のコーデックと rtx・red', () => {
  assert.deepEqual(preferCodec(CAPS.video.codecs, 'h264').map((c) => c.sdpFmtpLine ?? c.mimeType), ['packetization-mode=1;profile-level-id=42e01f', 'packetization-mode=0;profile-level-id=42e01f', 'video/rtx']);
  assert.deepEqual(preferCodec(CAPS.audio.codecs, 'opus').map((c) => c.mimeType), ['audio/opus', 'audio/red']);
  assert.equal(preferCodec(CAPS.video.codecs, 'av1'), null);
});

test('バックオフ: 倍々で上限まで、ゆらぎは ±jitter', () => {
  const mid = () => 0.5;
  assert.deepEqual([0, 1, 2, 3, 10].map((a) => backoffDelay(a, { initialDelayMs: 100, maxDelayMs: 1000 }, mid)), [100, 200, 400, 800, 1000]);
  assert.equal(backoffDelay(0, { initialDelayMs: 100, jitter: 0.2 }, () => 1), 120);
  assert.equal(backoffDelay(0, { initialDelayMs: 100, jitter: 0.2 }, () => 0), 80);
});

test('Broadcaster: 購読者ごとに配る、初めのもの、絞り込み、取り消し、閉じる', async () => {
  const b = new Broadcaster();
  const ac = new AbortController();
  const all = b.subscribe({ initial: () => [0] });
  const odd = b.subscribe({ filter: (x) => x % 2 === 1, signal: ac.signal });
  b.emit(1); b.emit(2); b.emit(3);
  assert.deepEqual([(await all.next()).value, (await all.next()).value, (await all.next()).value], [0, 1, 2]);
  assert.equal((await odd.next()).value, 1);
  assert.equal((await odd.next()).value, 3);
  const pending = odd.next();
  ac.abort();
  assert.equal((await pending).done, true);
  b.close(new Error('boom'));
  assert.equal((await all.next()).value, 3, '溜まっていたものは読める');
  await assert.rejects(all.next(), /boom/);
  assert.equal((await all.next()).done, true);
  // 閉じた後の購読はすぐ終わる（誤りを 1 度投げる）
  const late = b.subscribe();
  await assert.rejects(late.next(), /boom/);
});

test('Broadcaster: break で購読を外す、溜まりすぎは古いものから捨てる', async () => {
  const b = new Broadcaster();
  const it = b.subscribe({ maxBuffer: 2 });
  b.emit(1); b.emit(2); b.emit(3);
  for await (const v of it) { assert.equal(v, 2); break; }
  assert.equal(b.subs.size, 0);
});

test('sleep は取り消せる', async () => {
  const ac = new AbortController();
  const p = sleep(10000, ac.signal);
  ac.abort();
  await assert.rejects(p, (e) => e.name === 'AbortError');
});

test('DataChannel の宣言の検査', () => {
  assert.deepEqual(normalizeDataChannels(['#a', { label: '#b', direction: 'recvonly', ordered: false, maxPacketLifeTime: 500 }]),
    [{ label: '#a', direction: 'sendrecv', ordered: true }, { label: '#b', direction: 'recvonly', ordered: false, max_packet_life_time: 500 }]);
  assert.throws(() => normalizeDataChannels(['chat']), /# で始まる/);
  assert.throws(() => normalizeDataChannels(['#a', '#a']), /重複/);
  assert.throws(() => normalizeDataChannels([{ label: '#a', maxRetransmits: 1, maxPacketLifeTime: 1 }]), /片方/);
});
