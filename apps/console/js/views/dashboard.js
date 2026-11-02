// ダッシュボード: 配信・録画・予約の件数、ノード、デバイス、最近の出来事。SSE で自動更新。

import { h, replace, table, badge } from '../lib/dom.js';
import { errorBox } from '../lib/ui.js';
import { streams, recordings, schedules, system, listAll } from '../api/resources.js';
import { fmtTime, fmtRelative, fmtBps } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { on } from '../lib/bus.js';
import { recent, label } from '../lib/activity.js';

function card(labelText, value, sub, href) {
  const inner = [h('div', { class: 'label' }, labelText), h('div', { class: 'value' }, value), sub ? h('div', { class: 'sub' }, sub) : null];
  return href ? h('a', { class: 'card', href, style: { color: 'inherit', textDecoration: 'none' } }, inner) : h('div', { class: 'card' }, inner);
}

async function settle(p) {
  try { return { ok: true, value: await p }; } catch (e) { return { ok: false, error: e }; }
}

export async function render(root, _params, { caps }) {
  const cards = h('div', { class: 'grid cards' });
  const liveBox = h('div');
  const schedBox = h('div');
  const nodeBox = h('div');
  const devBox = h('div');
  const actBox = h('ul', { class: 'events', 'aria-live': 'polite' });
  const capBox = h('div');

  root.append(
    h('div', { class: 'page-head' }, h('h1', null, 'ダッシュボード'), h('span', { class: 'spacer' }),
      h('button', { onClick: () => refresh() }, '再読み込み')),
    cards,
    h('div', { class: 'grid cols-2', style: { marginTop: '16px' } },
      h('section', { class: 'panel', 'aria-labelledby': 'd-live' }, h('h2', { id: 'd-live' }, '配信中'), liveBox),
      h('section', { class: 'panel', 'aria-labelledby': 'd-sched' }, h('h2', { id: 'd-sched' }, 'これからの予約'), schedBox),
      h('section', { class: 'panel', 'aria-labelledby': 'd-node' }, h('h2', { id: 'd-node' }, 'ノード'), nodeBox),
      h('section', { class: 'panel', 'aria-labelledby': 'd-act' }, h('h2', { id: 'd-act' }, '最近の出来事'), actBox)),
    h('section', { class: 'panel', 'aria-labelledby': 'd-dev' }, h('h2', { id: 'd-dev' }, 'デバイス（コーデックのプロバイダー）'), devBox),
    h('section', { class: 'panel', 'aria-labelledby': 'd-cap' }, h('h2', { id: 'd-cap' }, 'このノードの能力'), capBox));

  const renderActivity = () => {
    const items = recent();
    replace(actBox, items.length
      ? items.slice(0, 20).map((a) => h('li', null,
        h('time', { datetime: a.at.toISOString() }, a.at.toLocaleTimeString('ja-JP')),
        h('span', null, label(a.type)),
        a.data && a.data.resource_id ? h('code', { class: 'muted' }, String(a.data.resource_id).slice(0, 10)) : null))
      : h('li', { class: 'muted' }, 'まだありません（SSE で届いた出来事を表示します）'));
  };

  const renderCaps = () => {
    if (!caps) return;
    const yes = (b) => (b ? badge('あり', 'ok') : badge('なし'));
    replace(capBox, h('div', { class: 'grid cols-2' },
      h('div', null, h('h3', null, '取り込み'), h('div', { class: 'row' },
        ...Object.entries(caps.ingest).map(([k, v]) => h('span', null, k, ' ', yes(v))))),
      h('div', null, h('h3', null, '出力'), h('div', { class: 'row' },
        h('span', null, 'HLS ', caps.output.hls.containers.join(' / ')), h('span', null, 'LL-HLS ', yes(caps.output.hls.ll_hls)),
        h('span', null, 'RTMP play ', yes(caps.output.rtmp_play)), h('span', null, 'WHEP ', yes(caps.output.whep)))),
      h('div', null, h('h3', null, 'そのまま通せるコーデック'), h('div', null, caps.codecs.passthrough.join(', '))),
      h('div', null, h('h3', null, '変換'), caps.codecs.transcode.length
        ? h('div', null, caps.codecs.transcode.map((t) => `${t.codec} ${t.direction === 'encode' ? '符号化' : '復号'}（${t.provider}${t.hardware ? '・HW' : ''}）`).join('、'))
        : h('div', { class: 'muted' }, 'コーデック worker が未接続'))));
  };

  const refresh = async () => {
    const [st, rec, sch, nodes, dev] = await Promise.all([
      settle(listAll(streams.list)),
      settle(recordings.list({ limit: 1000 })),
      settle(listAll(schedules.list)),
      settle(system.nodes()),
      settle(system.devices()),
    ]);
    const cardList = [];
    if (st.ok) {
      const live = st.value.filter((s) => s.observed_state === 'live');
      cardList.push(card('配信', st.value.length, `配信中 ${live.length}`, '#/streams'));
      cardList.push(card('配信中', live.length, live.length ? live.map((s) => s.name).slice(0, 3).join(', ') : '—', '#/streams'));
      replace(liveBox, table([
        { label: '名前', render: (s) => h('a', { href: `#/streams/${encodeURIComponent(s.id)}` }, s.name) },
        { label: '経路', render: (s) => s.live.transport },
        { label: 'ビットレート', class: 'num', render: (s) => fmtBps(s.live.bitrate_bps) },
        { label: '開始', render: (s) => fmtRelative(s.live.started_at) },
      ], live.filter((s) => s.live), { empty: '配信中の stream はありません' }));
    } else {
      cardList.push(card('配信', '—', st.error.title));
      replace(liveBox, errorBox(st.error));
    }
    if (rec.ok) {
      const items = rec.value.data.items;
      const more = rec.value.data.next_cursor ? '+' : '';
      cardList.push(card('録画', `${items.length}${more}`, `録画中 ${items.filter((r) => r.state === 'recording').length}`, '#/recordings'));
    } else {
      cardList.push(card('録画', '—', rec.error.title));
    }
    if (sch.ok) {
      const now = Date.now();
      const upcoming = sch.value
        .filter((s) => (s.state === 'pending' || s.state === 'running') && new Date(s.end_at).getTime() > now)
        .sort((a, b) => a.start_at.localeCompare(b.start_at));
      cardList.push(card('予約', upcoming.length, `実行中 ${upcoming.filter((s) => s.state === 'running').length}`, '#/schedules'));
      replace(schedBox, table([
        { label: '名前', render: (s) => h('a', { href: `#/schedules/${encodeURIComponent(s.id)}` }, s.name || s.id.slice(0, 8)) },
        { label: '操作', render: (s) => s.action },
        { label: '開始', render: (s) => h('span', { title: s.start_at }, fmtTime(s.start_at), ' ', h('span', { class: 'muted small' }, fmtRelative(s.start_at))) },
        { label: '状態', render: (s) => badge(s.state, s.state === 'running' ? 'ok' : '') },
      ], upcoming.slice(0, 8), { empty: 'これからの予約はありません' }));
    } else {
      cardList.push(card('予約', '—', sch.error.title));
      replace(schedBox, errorBox(sch.error));
    }
    if (nodes.ok) {
      const items = nodes.value.data.items;
      cardList.push(card('ノード', items.length, `drain 中 ${items.filter((n) => n.draining).length}`, '#/nodes'));
      replace(nodeBox, table([
        { label: '名前', render: (n) => [n.name, n.self ? h('span', { class: 'muted small' }, '（このノード）') : null] },
        { label: '状態', render: (n) => (n.draining ? badge('drain 中', 'warn') : badge('受付中', 'ok')) },
        { label: '配信中', class: 'num', render: (n) => n.live_streams },
        { label: '登録', render: (n) => fmtTime(n.registered_at) },
      ], items));
    } else {
      replace(nodeBox, nodes.error.status === 403
        ? h('p', { class: 'muted' }, 'ノードの情報には node:admin の権限が要ります。')
        : errorBox(nodes.error));
    }
    if (dev.ok) {
      replace(devBox, dev.value.data.nodes.map((n) => h('div', null,
        h('h3', null, 'ノード ', h('code', null, n.node.slice(0, 12))),
        n.devices.sandbox ? h('p', { class: 'small muted' }, 'サンドボックス: ', n.devices.sandbox) : null,
        table([
          { label: 'プロバイダー', render: (p) => h('code', null, p.provider) },
          { label: '状態', render: (p) => (p.available ? badge('使える', 'ok') : badge('使えない', 'danger')) },
          { label: '詳細・理由', render: (p) => h('span', { class: 'small break' }, p.detail) },
        ], n.devices.providers, { empty: 'コーデック worker が動いていません' }))));
    } else {
      replace(devBox, dev.error.status === 403
        ? h('p', { class: 'muted' }, 'デバイスの情報には node:admin の権限が要ります。')
        : errorBox(dev.error));
    }
    replace(cards, cardList);
  };

  renderActivity();
  renderCaps();
  await refresh();
  const offs = [
    refreshOn(['stream.', 'recording.', 'schedule.', 'node.'], refresh, { wait: 700 }),
    on('activity', renderActivity),
  ];
  return () => offs.forEach((off) => off());
}
