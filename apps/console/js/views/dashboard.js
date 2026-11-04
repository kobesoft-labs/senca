// ダッシュボード: 配信・録画・予約の件数、ノード、デバイス、最近の出来事。SSE で自動更新。

import { h, replace } from '../lib/dom.js';
import { errorBox } from '../lib/ui.js';
import { streams, recordings, schedules, system, listAll } from '../api/resources.js';
import { fmtTime, fmtRelative, fmtBps, fmtClock } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { on } from '../lib/bus.js';
import { recent, label } from '../lib/activity.js';
import { t } from '../i18n/index.js';
import { iconButton, pageHeader, panel, statTile, dataTable, statusPill, badge, alert, skeleton } from '../ui/index.js';

async function settle(p) {
  try { return { ok: true, value: await p }; } catch (e) { return { ok: false, error: e }; }
}

export async function render(root, _params, { caps }) {
  const cards = h('div', { class: 'grid stat-grid' }, skeleton({ kind: 'cards', rows: 5 }));
  const liveBox = h('div', null, skeleton({ kind: 'table', rows: 2 }));
  const schedBox = h('div', null, skeleton({ kind: 'table', rows: 2 }));
  const nodeBox = h('div', null, skeleton({ kind: 'table', rows: 1 }));
  const devBox = h('div', null, skeleton({ kind: 'table', rows: 3 }));
  const actBox = h('ul', { class: 'events', 'aria-live': 'polite' });
  const capBox = h('div');

  root.append(
    pageHeader({
      title: t('nav.dashboard'),
      description: t('dashboard.lead'),
      actions: [iconButton('refresh', t('common.refresh'), { variant: 'secondary', onClick: () => refresh() })],
    }),
    cards,
    h('div', { class: 'grid cols-2 section-gap' },
      panel({ title: t('dashboard.live'), icon: 'live' }, liveBox),
      panel({ title: t('dashboard.upcoming'), icon: 'schedules' }, schedBox),
      panel({ title: t('dashboard.nodes'), icon: 'nodes' }, nodeBox),
      panel({ title: t('dashboard.activity'), icon: 'activity', description: t('dashboard.activityDesc') }, actBox)),
    h('div', { class: 'section-gap' }),
    panel({ title: t('dashboard.devices'), icon: 'gpu' }, devBox),
    panel({ title: t('dashboard.caps'), icon: 'zap' }, capBox));

  const renderActivity = () => {
    const items = recent();
    replace(actBox, items.length
      ? items.slice(0, 20).map((a) => h('li', null,
        h('time', { datetime: a.at.toISOString() }, fmtClock(a.at)),
        h('span', null, label(a.type)),
        a.data && a.data.resource_id ? h('code', { class: 'muted' }, String(a.data.resource_id).slice(0, 10)) : null))
      : h('li', { class: 'muted' }, t('dashboard.activityEmpty')));
  };

  const renderCaps = () => {
    if (!caps) return;
    const yes = (b) => (b ? badge(t('common.yes'), 'ok') : badge(t('common.no')));
    const item = (k, v) => h('span', { class: 'row' }, k, yes(v));
    replace(capBox, h('div', { class: 'grid cols-2' },
      h('div', null, h('h3', null, t('dashboard.capIngest')), h('div', { class: 'row' },
        ...Object.entries(caps.ingest).map(([k, v]) => item(k, v)))),
      h('div', null, h('h3', null, t('dashboard.capOutput')), h('div', { class: 'row' },
        h('span', null, 'HLS ', caps.output.hls.containers.join(' / ')), item('LL-HLS', caps.output.hls.ll_hls),
        item('RTMP play', caps.output.rtmp_play), item('WHEP', caps.output.whep))),
      h('div', null, h('h3', null, t('dashboard.capPassthrough')), h('div', null, caps.codecs.passthrough.join(', '))),
      h('div', null, h('h3', null, t('dashboard.capTranscode')), caps.codecs.transcode.length
        ? h('div', { class: 'row' }, caps.codecs.transcode.map((x) => h('span', { class: 'tag' },
          t(x.direction === 'encode' ? 'dashboard.encode' : 'dashboard.decode', { codec: x.codec, provider: `${x.provider}${x.hardware ? '・HW' : ''}` }))))
        : h('div', { class: 'muted' }, t('dashboard.noWorker')))));
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
      cardList.push(statTile({ label: t('dashboard.card.streams'), value: st.value.length, sub: t('dashboard.card.liveCount', { n: live.length }), icon: 'streams', href: '#/streams' }));
      cardList.push(statTile({ label: t('dashboard.card.live'), value: live.length, sub: live.length ? live.map((s) => s.name).slice(0, 3).join(', ') : '—', icon: 'live', href: '#/streams', kind: live.length ? 'live' : undefined }));
      replace(liveBox, dataTable({
        columns: [
          { key: 'name', label: t('common.name'), primary: true, sort: (s) => s.name, render: (s) => h('a', { href: `#/streams/${encodeURIComponent(s.id)}` }, s.name) },
          { label: t('dashboard.col.transport'), render: (s) => s.live.transport },
          { key: 'br', label: t('dashboard.col.bitrate'), class: 'num', sort: (s) => s.live.bitrate_bps, render: (s) => fmtBps(s.live.bitrate_bps) },
          { label: t('dashboard.col.started'), render: (s) => fmtRelative(s.live.started_at) },
        ],
        rows: live.filter((s) => s.live),
        empty: { icon: 'live', title: t('dashboard.liveEmpty'), body: t('dashboard.liveEmptyBody'), actions: [h('a', { class: 'btn sm', href: '#/streams' }, t('dashboard.toStreams'))] },
      }));
    } else {
      cardList.push(statTile({ label: t('dashboard.card.streams'), value: '—', sub: st.error.title, icon: 'streams' }));
      replace(liveBox, errorBox(st.error));
    }
    if (rec.ok) {
      const items = rec.value.data.items;
      const more = rec.value.data.next_cursor ? '+' : '';
      cardList.push(statTile({ label: t('dashboard.card.recordings'), value: `${items.length}${more}`, sub: t('dashboard.card.recordingCount', { n: items.filter((r) => r.state === 'recording').length }), icon: 'recordings', href: '#/recordings' }));
    } else {
      cardList.push(statTile({ label: t('dashboard.card.recordings'), value: '—', sub: rec.error.title, icon: 'recordings' }));
    }
    if (sch.ok) {
      const now = Date.now();
      const upcoming = sch.value
        .filter((s) => (s.state === 'pending' || s.state === 'running') && new Date(s.end_at).getTime() > now)
        .sort((a, b) => a.start_at.localeCompare(b.start_at));
      cardList.push(statTile({ label: t('dashboard.card.schedules'), value: upcoming.length, sub: t('dashboard.card.runningCount', { n: upcoming.filter((s) => s.state === 'running').length }), icon: 'schedules', href: '#/schedules' }));
      replace(schedBox, dataTable({
        columns: [
          { label: t('common.name'), primary: true, render: (s) => h('a', { href: `#/schedules/${encodeURIComponent(s.id)}` }, s.name || s.id.slice(0, 8)) },
          { label: t('dashboard.col.action'), render: (s) => s.action },
          { label: t('dashboard.col.start'), render: (s) => h('span', { title: s.start_at }, fmtTime(s.start_at), ' ', h('span', { class: 'muted small' }, fmtRelative(s.start_at))) },
          { label: t('common.state'), render: (s) => statusPill(s.state) },
        ],
        rows: upcoming.slice(0, 8),
        empty: { icon: 'calendar', title: t('dashboard.upcomingEmpty'), actions: [h('a', { class: 'btn sm', href: '#/schedules?new=1' }, t('palette.newSchedule'))] },
      }));
    } else {
      cardList.push(statTile({ label: t('dashboard.card.schedules'), value: '—', sub: sch.error.title, icon: 'schedules' }));
      replace(schedBox, errorBox(sch.error));
    }
    if (nodes.ok) {
      const items = nodes.value.data.items;
      cardList.push(statTile({ label: t('dashboard.card.nodes'), value: items.length, sub: t('dashboard.card.drainingCount', { n: items.filter((n) => n.draining).length }), icon: 'nodes', href: '#/nodes' }));
      replace(nodeBox, dataTable({
        columns: [
          { label: t('common.name'), primary: true, render: (n) => [n.name, n.self ? h('span', { class: 'muted small' }, t('common.thisNode')) : null] },
          { label: t('common.state'), render: (n) => statusPill(n.draining ? 'draining' : 'accepting') },
          { label: t('dashboard.col.liveStreams'), class: 'num', render: (n) => n.live_streams },
          { label: t('dashboard.col.registered'), render: (n) => fmtTime(n.registered_at) },
        ],
        rows: items,
      }));
    } else {
      replace(nodeBox, nodes.error.status === 403
        ? alert('info', t('dashboard.nodesForbidden'))
        : errorBox(nodes.error));
    }
    if (dev.ok) {
      replace(devBox, dev.value.data.nodes.map((n) => h('div', null,
        h('h3', null, t('dashboard.nodeHeading'), ' ', h('code', null, n.node.slice(0, 12))),
        n.devices.sandbox ? h('p', { class: 'small muted' }, t('dashboard.sandbox'), ': ', n.devices.sandbox) : null,
        dataTable({
          columns: [
            { label: t('dashboard.col.provider'), primary: true, render: (p) => h('code', null, p.provider) },
            { label: t('common.state'), render: (p) => statusPill(p.available ? 'available' : 'unavailable') },
            { label: t('dashboard.col.detail'), wrap: true, render: (p) => h('span', { class: 'small break' }, p.detail) },
          ],
          rows: n.devices.providers,
          empty: { icon: 'gpu', title: t('dashboard.noWorker'), body: t('dashboard.noWorkerBody') },
        }))));
    } else {
      replace(devBox, dev.error.status === 403
        ? alert('info', t('dashboard.devicesForbidden'))
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
