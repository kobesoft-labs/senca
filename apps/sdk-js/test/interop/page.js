// tools/interop/sdk_js.sh が Chrome で開くページ。@senca/sdk だけで部屋に入り、公開・受信・#chat・層の切り替えを行う。
// 結果は window.__result に置く。driver が window.__close() を呼ぶと閉じる。
//   ?room=demo&name=alice&signal=ws://127.0.0.1:1234/signaling&simulcast=1&peers=2&secs=60&switch=1&target=alice
import { Senca } from '../../src/index.js';

const q = new URLSearchParams(location.search);
const room = q.get('room') || 'demo';
const name = q.get('name') || 'anon';
const peers = Number(q.get('peers') || '2');
const secs = Number(q.get('secs') || '60');
const simulcast = q.get('simulcast') === '1';
const switchLayers = q.get('switch') === '1';
const target = q.get('target') || 'alice';
const R = { ok: false, name, decoded: {}, widths: {}, chatFrom: [], directFrom: [], events: {}, layer: {}, left: [], errors: [] };
const log = (...a) => {
  const s = a.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join(' ');
  document.getElementById('log').textContent += s + '\n';
  console.log(`${name}: ${s}`);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** @type {import('../../src/index.js').Session} */
let session;

async function main() {
  const size = simulcast ? { width: 1280, height: 720 } : { width: 640, height: 480 };
  const stream = await navigator.mediaDevices.getUserMedia({ video: { ...size, frameRate: 30 }, audio: true });
  const t0 = Date.now();
  session = await Senca.connect({
    url: q.get('signal'),
    room,
    token: async () => (await (await fetch(`/token?room=${room}&name=${name}`)).json()).token,
    clientId: name,
    dataChannels: ['#chat'],
    publish: { stream, simulcast },
    video: { codec: 'vp8' },
    reportStats: { intervalMs: 2000 },
  });
  R.connectMs = Date.now() - t0;
  R.connectionId = session.connectionId;
  R.room = session.room;
  log('connected', session.connectionId, 'in', R.connectMs, 'ms; participants', [...session.participants.values()].map((p) => p.clientId));

  // 出来事を数える
  (async () => {
    for await (const e of session.events()) {
      R.events[e.type] = (R.events[e.type] ?? 0) + 1;
      if (e.type === 'participant.left') R.left.push(e.participant.clientId);
      if (e.type === 'error') R.errors.push({ code: e.error.code, message: e.error.message });
      if (e.type === 'layer.changed') log('layer.changed', e.track.clientId, e.rid);
    }
  })();
  // 受けたトラックを <video> に
  (async () => {
    for await (const t of session.remoteTracks({ kind: 'video' })) {
      const v = document.createElement('video');
      v.autoplay = true; v.muted = true; v.playsInline = true;
      t.attach(v);
      document.getElementById('videos').appendChild(v);
      t.ended.then(() => v.remove());
    }
  })();
  // #chat: ping には pong を返す（宛先つき = シグナリング経由で from が付く）
  (async () => {
    for await (const m of session.messages('#chat')) {
      const body = m.json();
      if (body.kind === 'ping' && body.from !== name) {
        if (!R.chatFrom.includes(body.from)) R.chatFrom.push(body.from);
        const to = [...session.participants.values()].find((p) => p.clientId === body.from);
        if (to) session.send('#chat', { kind: 'pong', from: name, to: body.from }, { to }).catch((e) => R.errors.push(String(e)));
      } else if (body.kind === 'pong' && body.to === name) {
        const from = m.sender?.clientId ?? body.from;
        if (m.via === 'signaling' && !R.directFrom.includes(from)) R.directFrom.push(from);
      }
    }
  })();

  const deadline = Date.now() + secs * 1000;
  let doneAt = 0;
  let switched = 0;
  while (Date.now() < deadline) {
    await sleep(500);
    session.send('#chat', { kind: 'ping', from: name, at: Date.now() }).catch(() => {});
    const st = await session.stats();
    for (const i of st.inbound) {
      if (i.kind !== 'video' || !i.clientId) continue;
      R.decoded[i.clientId] = i.framesDecoded ?? 0;
      R.widths[i.clientId] = i.frameWidth ?? 0;
    }
    const good = Object.values(R.decoded).filter((d) => d > 0).length;
    if (!doneAt && good >= peers && R.chatFrom.length >= peers && R.directFrom.length >= peers) {
      doneAt = Date.now();
      log('all decoded', R.decoded, 'chat', R.chatFrom, 'direct', R.directFrom);
    }
    if (switchLayers && doneAt && switched === 0) {
      switched = 1;
      const t = [...session.tracks.values()].find((x) => x.kind === 'video' && x.clientId === target);
      if (!t) { R.errors.push('no track of ' + target); break; }
      R.layer.rids = [...t.rids];
      try {
        await t.setLayer('r2', { timeoutMs: 15000 });
        R.layer.r2 = t.layer;
        await sleep(3000);
        R.widthHigh = (await t.stats())?.frameWidth ?? 0;
        await t.setLayer('r0', { timeoutMs: 15000 });
        R.layer.r0 = t.layer;
        await sleep(3000);
        R.widthLow = (await t.stats())?.frameWidth ?? 0;
        await t.setLayer('auto');
      } catch (e) {
        R.errors.push('setLayer: ' + e.code + ' ' + e.message);
      }
      switched = 2;
    }
    if (doneAt && Date.now() - doneAt > 2000 && (!switchLayers || switched === 2)) break;
  }
  const st = await session.stats();
  R.outbound = st.outbound.filter((o) => o.kind === 'video').map((o) => ({ rid: o.rid, trackId: o.trackId, width: o.frameWidth, sent: o.framesSent }));
  R.transport = { rttMs: st.transport.rttMs, availableOutgoingBitrateBps: st.transport.availableOutgoingBitrateBps };
  R.publication = session.publications[0]?.tracks.map((t) => ({ kind: t.kind, id: t.id, rids: [...t.rids] }));
  R.ok = Object.values(R.decoded).filter((d) => d > 0).length >= peers && R.chatFrom.length >= peers && R.directFrom.length >= peers;
  window.__result = R;
}

window.__close = async () => {
  const info = await session.close();
  return { reason: info.reason, state: session.state, left: R.left };
};
window.__left = () => R.left;

main().catch((e) => { R.error = `${e?.name}: ${e?.code ?? ''} ${e?.message ?? e}`; window.__result = R; });
