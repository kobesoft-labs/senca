// ダッシュボード: 「全部大丈夫か・今なにが流れているか・何を見ればよいか」を 3 秒で答える。
//
//   はじめの手順（新しいノード。実際の状態から進み具合を決める。閉じても見出しのボタンで開き直せる）
//   様子の帯（全体・ノード・配信中・録画中・変換）→ 確認が要ること（それぞれに直接の操作）
//   配信中（今の画・ビットレート・視聴者・経過）→ 録画中・これからの予約 → 最近の出来事（SSE）・このノードでできること
//
// 失敗した一覧（権限が無いなど）は、その部分だけを出さない・理由を出す。SSE で読み直す。

import { h, replace } from '../lib/dom.js';
import { errorBox, confirmDialog, reportError, toast } from '../lib/ui.js';
import { streams, recordings, schedules, system, tokens, inputs, outputs, listAll } from '../api/resources.js';
import { grid as gridApi } from '../api/ops.js';
import { fmtTime, fmtRelative, fmtBps, fmtClock, fmtDuration, fmtNumber } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { on } from '../lib/bus.js';
import { recent, label as activityLabel } from '../lib/activity.js';
import { t, tn, has } from '../i18n/index.js';
import { icon } from '../lib/icons.js';
import { guideUrl } from '../lib/guide.js';
import { button, pageHeader, panel, statusPill, badge, skeleton, emptyState, checklist, resetChecklist } from '../ui/index.js';
import { HW_PROVIDERS } from './nodes.js';

const CHECKLIST = 'getting-started';
const WATCHED = 'senca.console.onboarding.watched';
const SNAP_EVERY = 15000;
const IO_SCAN_MAX = 40; // 入力・出力の状態を見る配信の数（多すぎるときは配信中のものを先に）
const EXPIRY_SOON = 7 * 86400000;

async function settle(p) {
  try { return { ok: true, value: await p }; } catch (e) { return { ok: false, error: e }; }
}

const viewersOf = (s) => (s.live && s.live.viewers && typeof s.live.viewers.total === 'number' ? s.live.viewers.total : null);

function flag(key) {
  try { return localStorage.getItem(key) === '1'; } catch { return false; }
}
function setFlag(key) {
  try { localStorage.setItem(key, '1'); } catch { /* 覚えなくても困らない */ }
}

export async function render(root, _params, { caps }) {
  root = root.appendChild(h('div', { class: 'ops-view' }));
  const checklistBox = h('div');
  const healthBox = h('div', { class: 'health' }, skeleton({ kind: 'cards', rows: 5 }));
  const attentionBox = h('div');
  const onAirBox = h('div', null, skeleton({ kind: 'block' }));
  const recBox = h('div', null, skeleton({ kind: 'lines', rows: 2 }));
  const schedBox = h('div', null, skeleton({ kind: 'lines', rows: 2 }));
  const actBox = h('ul', { class: 'activity-list', 'aria-live': 'polite', 'data-testid': 'dashboard-activity' });
  const capBox = h('div');
  let forceShow = false; // 全部終わった後に開き直したとき
  const reopenBtn = button(t('dashboard.onboarding.reopen'), {
    variant: 'ghost', icon: 'rocket', testid: 'onboarding-reopen',
    onClick: () => { resetChecklist(CHECKLIST); forceShow = true; drawChecklist(); },
  });
  reopenBtn.hidden = true;
  // コマンドパレットの「はじめの手順を開く」（#/?onboarding=1）
  if (new URLSearchParams(location.hash.split('?')[1] || '').get('onboarding') === '1') {
    resetChecklist(CHECKLIST);
    forceShow = true;
    history.replaceState(history.state, '', `${location.pathname}${location.search}#/`);
  }

  const thumbs = new Map(); // stream id → {url, at}
  let last = null; // 最後に読んだもの
  let ioProblems = [];
  let disposed = false;

  root.append(
    pageHeader({
      title: t('nav.dashboard'),
      description: t('dashboard.lead'),
      actions: [reopenBtn, button(t('common.refresh'), { variant: 'secondary', icon: 'refresh', iconOnly: true, onClick: () => refresh() })],
    }),
    checklistBox,
    healthBox,
    attentionBox,
    panel({ title: t('dashboard.onAir'), icon: 'live', testid: 'dashboard-onair', actions: [h('a', { class: 'btn sm ghost', href: '#/streams' }, t('dashboard.allStreams'), icon('chevron-right'))] }, onAirBox),
    h('div', { class: 'grid cols-2' },
      panel({ title: t('dashboard.recordingNow'), icon: 'record', actions: [h('a', { class: 'btn sm ghost', href: '#/recordings' }, t('dashboard.allRecordings'), icon('chevron-right'))] }, recBox),
      panel({ title: t('dashboard.upcoming'), icon: 'schedules', actions: [h('a', { class: 'btn sm ghost', href: '#/schedules' }, t('dashboard.allSchedules'), icon('chevron-right'))] }, schedBox)),
    h('div', { class: 'grid cols-2' },
      panel({ title: t('dashboard.activity'), icon: 'activity', description: t('dashboard.activityDesc'), actions: [h('a', { class: 'btn sm ghost', href: '#/audit' }, t('dashboard.toAudit'), icon('chevron-right'))] }, actBox),
      panel({ title: t('dashboard.caps'), icon: 'zap', actions: [h('a', { class: 'btn sm ghost', href: '#/nodes' }, t('dashboard.capsMore'), icon('chevron-right'))] }, capBox)));

  // ---------------------------------------------------------------- はじめの手順
  const drawChecklist = () => {
    if (!last) return;
    const st = last.streams.ok ? last.streams.value : [];
    const recs = last.recs.ok ? last.recs.value.data.items : [];
    const toks = last.tokens.ok ? last.tokens.value : null;
    const live = st.filter((s) => s.observed_state === 'live');
    const first = live[0] || st[0] || null;
    const gpu = last.devices.ok && last.devices.value.data.nodes.some((n) => n.devices.providers.some((p) => p.available && HW_PROVIDERS.includes(p.provider)));
    const steps = [
      { id: 'stream', title: t('dashboard.onboarding.stream'), description: t('dashboard.onboarding.streamBody'), done: st.length > 0, action: { label: t('dashboard.onboarding.streamAction'), href: '#/streams?new=1' } },
      { id: 'send', title: t('dashboard.onboarding.send'), description: t('dashboard.onboarding.sendBody'), done: live.length > 0 || recs.length > 0,
        action: first ? { label: t('dashboard.onboarding.sendAction'), href: `#/streams/${encodeURIComponent(first.id)}` } : { label: t('dashboard.onboarding.sendBrowser'), href: '#/send' } },
      { id: 'watch', title: t('dashboard.onboarding.watch'), description: t('dashboard.onboarding.watchBody'), done: flag(WATCHED) || live.some((s) => (viewersOf(s) || 0) > 0),
        action: first ? { label: t('dashboard.onboarding.watchAction'), href: `#/streams/${encodeURIComponent(first.id)}`, onClick: () => setFlag(WATCHED) } : undefined },
      { id: 'record', title: t('dashboard.onboarding.record'), description: t('dashboard.onboarding.recordBody'), done: recs.length > 0,
        action: { label: t('dashboard.onboarding.recordAction'), href: first ? `#/streams/${encodeURIComponent(first.id)}` : '#/recordings' } },
      { id: 'team', title: t('dashboard.onboarding.team'), description: t('dashboard.onboarding.teamBody'), done: !!toks && toks.length > 1, action: { label: t('dashboard.onboarding.teamAction'), href: '#/users' } },
      { id: 'scale', optional: true, title: t('dashboard.onboarding.scale'), description: t('dashboard.onboarding.scaleBody'), done: !!(caps && caps.cluster) || gpu, action: { label: t('dashboard.onboarding.scaleAction'), href: '#/nodes' } },
    ];
    const allDone = steps.filter((s) => !s.optional).every((s) => s.done);
    // 全部終わったら出さない（見出しの「はじめの手順」で開き直せる）
    const el = !allDone || forceShow ? checklist({
      id: CHECKLIST,
      title: allDone ? t('dashboard.onboarding.doneTitle') : t('dashboard.onboarding.title'),
      description: allDone ? t('dashboard.onboarding.doneBody') : t('dashboard.onboarding.body'),
      steps,
      onDismiss: () => { forceShow = false; reopenBtn.hidden = false; },
    }) : null;
    replace(checklistBox, el);
    reopenBtn.hidden = !!el;
  };

  // ---------------------------------------------------------------- 様子の帯
  const healthItem = ({ icon: ic, label, value, sub, kind, href, testid }) => h(href ? 'a' : 'div', {
    class: ['health-item', kind].filter(Boolean).join(' '), href, 'data-testid': testid,
  },
  h('span', { class: 'hi-icon' }, icon(ic)),
  h('span', { class: 'hi-text' },
    h('span', { class: 'hi-label' }, label),
    h('span', { class: 'hi-value' }, value),
    sub ? h('span', { class: 'hi-sub' }, sub) : null));

  const drawHealth = (attention) => {
    const d = last;
    const items = [];
    const worst = attention.some((a) => a.kind === 'danger') ? 'danger' : attention.length ? 'warn' : 'ok';
    items.push(h('div', { class: `health-summary ${worst}`, 'data-testid': 'health-summary', role: 'status' },
      icon(worst === 'ok' ? 'success' : 'warning', { size: 'lg' }),
      h('div', null,
        h('strong', null, worst === 'ok' ? t('dashboard.health.allGood') : tn('dashboard.health.needs', attention.length)),
        h('span', null, worst === 'ok' ? t('dashboard.health.allGoodSub') : t('dashboard.health.needsSub')))));
    // ノード
    if (d.grid.ok || d.nodes.ok) {
      const gNodes = d.grid.ok ? d.grid.value.data.nodes : null;
      const list = gNodes || d.nodes.value.data.items;
      const up = gNodes ? gNodes.filter((n) => n.up).length : list.length;
      const draining = list.filter((n) => n.draining).length;
      const leader = d.grid.ok && d.grid.value.data.scheduler ? (gNodes.find((n) => n.id === d.grid.value.data.scheduler) || {}).name : null;
      const cluster = !!(caps && caps.cluster) || list.length > 1;
      items.push(healthItem({
        icon: 'nodes', label: t('dashboard.health.nodes'), href: '#/nodes', testid: 'health-nodes',
        value: t('dashboard.health.nodesUp', { up, total: list.length }),
        sub: [cluster ? (leader ? t('dashboard.health.clusterLeader', { name: leader }) : t('dashboard.health.cluster')) : t('dashboard.health.single'),
          draining ? t('dashboard.health.draining', { n: draining }) : null].filter(Boolean).join(' · '),
        kind: up < list.length ? 'danger' : draining ? 'warn' : null,
      }));
    } else {
      items.push(healthItem({ icon: 'nodes', label: t('dashboard.health.nodes'), value: '—', sub: t('dashboard.health.needNodeAdmin') }));
    }
    // 配信中・録画中
    const st = d.streams.ok ? d.streams.value : [];
    const live = st.filter((s) => s.observed_state === 'live');
    items.push(healthItem({
      icon: 'live', label: t('dashboard.health.onAir'), href: '#/streams', testid: 'health-onair',
      value: d.streams.ok ? fmtNumber(live.length) : '—',
      sub: d.streams.ok ? (live.length ? live.map((s) => s.name).slice(0, 2).join(', ') : t('dashboard.health.ofStreams', { n: st.length })) : d.streams.error.title,
      kind: live.length ? 'live' : null,
    }));
    const recNow = d.recs.ok ? d.recs.value.data.items.filter((r) => r.state === 'recording') : [];
    items.push(healthItem({
      icon: 'record', label: t('dashboard.health.recording'), href: '#/recordings', testid: 'health-recording',
      value: d.recs.ok ? fmtNumber(recNow.length) : '—',
      sub: d.recs.ok ? (recNow.length ? t('dashboard.health.recordingSub') : t('dashboard.health.notRecording')) : d.recs.error.title,
      kind: recNow.length ? 'rec' : null,
    }));
    // 変換（GPU・ソフトウェア）
    const tc = caps ? caps.codecs.transcode : [];
    const hw = tc.filter((x) => x.hardware);
    const codecs = [...new Set(tc.map((x) => x.codec.toUpperCase()))];
    items.push(healthItem({
      icon: 'gpu', label: t('dashboard.health.transcode'), href: '#/nodes', testid: 'health-transcode',
      value: !tc.length ? t('dashboard.health.passthroughOnly') : hw.length ? t('dashboard.health.gpu') : t('dashboard.health.software'),
      sub: tc.length ? codecs.join(' · ') : t('dashboard.health.noWorker'),
    }));
    replace(healthBox, h('div', { class: 'health-strip', 'data-testid': 'health-strip' }, items));
  };

  // ---------------------------------------------------------------- 確認が要ること
  const undrain = async (n) => {
    const ok = await confirmDialog(t('nodes.undrainTitle'), t('nodes.undrainBody', { name: n.name || n.id }), { ok: t('nodes.undrainOk'), danger: false, icon: 'play', consequences: [t('nodes.undrainC1')] });
    if (!ok) return;
    try {
      await system.undrain(n.id);
      toast(t('nodes.undrained'), 'ok');
      refresh();
    } catch (e) {
      reportError(e, t('nodes.undrainTitle'));
    }
  };

  const collectAttention = () => {
    const d = last;
    const out = [];
    const nameOf = new Map((d.streams.ok ? d.streams.value : []).map((s) => [s.id, s.name]));
    if (d.grid.ok) {
      const g = d.grid.value.data;
      for (const n of g.nodes.filter((x) => !x.up)) {
        out.push({ kind: 'danger', icon: 'nodes', title: t('dashboard.att.nodeDown', { name: n.name || n.id.slice(0, 8) }), body: t('dashboard.att.nodeDownBody'), action: { label: t('dashboard.att.openNodes'), href: '#/nodes' } });
      }
      for (const n of g.nodes.filter((x) => x.up && x.draining)) {
        out.push({ kind: 'warn', icon: 'pause', title: t('dashboard.att.draining', { name: n.name || n.id.slice(0, 8) }), body: t('dashboard.att.drainingBody'), action: { label: t('nodes.undrain'), onClick: () => undrain(n) } });
      }
      const stuck = g.workloads.filter((w) => w.state === 'unschedulable');
      if (stuck.length) out.push({ kind: 'danger', icon: 'grid', title: tn('dashboard.att.unschedulable', stuck.length), body: t('dashboard.att.unschedulableBody'), action: { label: t('dashboard.att.openGrid'), href: '#/grid?state=unschedulable' } });
      const deg = g.workloads.filter((w) => w.state === 'degraded');
      if (deg.length) out.push({ kind: 'warn', icon: 'grid', title: tn('dashboard.att.degraded', deg.length), body: t('dashboard.att.degradedBody'), action: { label: t('dashboard.att.openGrid'), href: '#/grid?state=degraded' } });
      for (const n of g.nodes) {
        for (const gp of n.gpus.filter((x) => !x.healthy || !x.enabled)) {
          out.push({ kind: 'warn', icon: 'gpu', title: t(gp.enabled ? 'dashboard.att.gpuDown' : 'dashboard.att.gpuDisabled', { id: gp.id, node: n.name || n.id.slice(0, 8) }), body: t(gp.enabled ? 'dashboard.att.gpuDownBody' : 'dashboard.att.gpuDisabledBody'), action: { label: t('dashboard.att.openNodes'), href: '#/nodes' } });
        }
        for (const c of n.capture.filter((x) => !x.healthy || x.disabled)) {
          out.push({ kind: 'warn', icon: 'camera', title: t(c.disabled ? 'dashboard.att.captureDisabled' : 'dashboard.att.captureDown', { id: c.id, node: n.name || n.id.slice(0, 8) }), body: t('dashboard.att.captureBody'), action: { label: t('dashboard.att.openNodes'), href: '#/nodes' } });
        }
      }
    } else if (d.nodes.ok) {
      for (const n of d.nodes.value.data.items.filter((x) => x.draining)) {
        out.push({ kind: 'warn', icon: 'pause', title: t('dashboard.att.draining', { name: n.name }), body: t('dashboard.att.drainingBody'), action: { label: t('nodes.undrain'), onClick: () => undrain(n) } });
      }
    }
    for (const p of ioProblems) {
      out.push({
        kind: 'danger', icon: p.kind === 'output' ? 'upload' : 'download',
        title: t(p.kind === 'output' ? 'dashboard.att.outputFailed' : 'dashboard.att.inputFailed', { stream: nameOf.get(p.stream) || p.stream.slice(0, 8), protocol: has(`grid.proto.${p.protocol}`) ? t(`grid.proto.${p.protocol}`) : p.protocol }),
        body: p.error || t('dashboard.att.retrying'),
        action: { label: t('dashboard.att.openStream'), href: `#/streams/${encodeURIComponent(p.stream)}` },
      });
    }
    if (d.scheds.ok) {
      const dayAgo = Date.now() - 86400000;
      for (const s of d.scheds.value.filter((x) => (x.state === 'failed' || x.state === 'missed') && new Date(x.end_at).getTime() > dayAgo)) {
        out.push({ kind: 'warn', icon: 'schedules', title: t(s.state === 'failed' ? 'dashboard.att.scheduleFailed' : 'dashboard.att.scheduleMissed', { name: s.name || s.id.slice(0, 8) }), body: (s.last_run && s.last_run.reason) || t('dashboard.att.scheduleBody'), action: { label: t('dashboard.att.openSchedule'), href: `#/schedules/${encodeURIComponent(s.id)}` } });
      }
    }
    if (d.recs.ok) {
      const dayAgo = Date.now() - 86400000;
      const bad = d.recs.value.data.items.filter((r) => r.state === 'interrupted' && new Date(r.ended_at || r.started_at).getTime() > dayAgo);
      if (bad.length) out.push({ kind: 'warn', icon: 'recordings', title: tn('dashboard.att.recInterrupted', bad.length), body: t('dashboard.att.recInterruptedBody'), action: { label: t('dashboard.allRecordings'), href: '#/recordings' } });
    }
    if (d.tokens.ok) {
      const now = Date.now();
      const soon = d.tokens.value.filter((x) => x.expires_at && new Date(x.expires_at).getTime() > now && new Date(x.expires_at).getTime() - now < EXPIRY_SOON);
      const gone = d.tokens.value.filter((x) => x.expires_at && new Date(x.expires_at).getTime() <= now);
      if (soon.length) out.push({ kind: 'warn', icon: 'key', title: tn('dashboard.att.tokensExpiring', soon.length), body: soon.slice(0, 3).map((x) => t('dashboard.att.tokenItem', { name: x.name, rel: fmtRelative(x.expires_at) })).join(t('dashboard.listSep')), action: { label: t('dashboard.att.openTokens'), href: '#/tokens' } });
      if (gone.length) out.push({ kind: 'info', icon: 'key', title: tn('dashboard.att.tokensExpired', gone.length), body: t('dashboard.att.tokensExpiredBody'), action: { label: t('dashboard.att.openTokens'), href: '#/tokens?filter=expired' } });
    }
    if (caps && !caps.codecs.transcode.length && d.devices.ok) {
      out.push({ kind: 'info', icon: 'cpu', title: t('dashboard.att.noWorker'), body: t('dashboard.att.noWorkerBody'), action: { label: t('dashboard.att.readGuide'), href: guideUrl('14-codecs-and-patents.md'), external: true } });
    }
    return out;
  };

  const drawAttention = (items) => {
    if (!items.length) { replace(attentionBox); return; }
    const order = { danger: 0, warn: 1, info: 2 };
    const rows = items.slice().sort((a, b) => order[a.kind] - order[b.kind]).map((a) => h('li', { class: `att-item ${a.kind}` },
      h('span', { class: 'att-icon' }, icon(a.icon)),
      h('div', { class: 'att-text' }, h('strong', null, a.title), a.body ? h('span', null, a.body) : null),
      a.action ? (a.action.href
        ? h('a', { class: 'btn sm', href: a.action.href, target: a.action.external ? '_blank' : null, rel: a.action.external ? 'noopener noreferrer' : null }, a.action.label, a.action.external ? icon('external') : null)
        : button(a.action.label, { size: 'sm', onClick: a.action.onClick })) : null));
    replace(attentionBox, panel({ title: t('dashboard.attention'), icon: 'bell', testid: 'dashboard-attention' }, h('ul', { class: 'att-list' }, rows)));
  };

  // ---------------------------------------------------------------- 配信中
  const thumb = (s) => {
    const cur = thumbs.get(s.id);
    const box = h('div', { class: 'onair-thumb' });
    if (cur && cur.url) box.append(h('img', { src: cur.url, alt: t('dashboard.thumbAlt', { name: s.name }), loading: 'lazy' }));
    else box.append(h('div', { class: 'onair-thumb-empty' }, icon('live', { size: 'xl' })));
    box.append(badge(t('status.live'), 'live', { size: null }));
    return box;
  };

  const loadThumbs = async (live) => {
    const now = Date.now();
    await Promise.all(live.map(async (s) => {
      const cur = thumbs.get(s.id);
      if (cur && now - cur.at < SNAP_EVERY - 500) return;
      if (cur && cur.failUntil && now < cur.failUntil) return;
      thumbs.set(s.id, { url: cur ? cur.url : null, at: now });
      try {
        const { data } = await streams.snapshot(s.id);
        if (disposed) return;
        if (cur && cur.url) URL.revokeObjectURL(cur.url);
        const url = URL.createObjectURL(data);
        thumbs.set(s.id, { url, at: now });
        const img = onAirBox.querySelector(`[data-stream="${CSS.escape(s.id)}"] .onair-thumb`);
        if (img) replace(img, h('img', { src: url, alt: t('dashboard.thumbAlt', { name: s.name }) }), badge(t('status.live'), 'live'));
      } catch {
        // 今の画が無い・作れない（コーデック worker が無い・デコーダーが無いなど）: 絵の代わりのアイコンのまま。しばらく問い合わせない
        thumbs.set(s.id, { url: cur ? cur.url : null, at: now, failUntil: now + 5 * 60000 });
      }
    }));
  };

  const drawOnAir = () => {
    const d = last;
    if (!d.streams.ok) { replace(onAirBox, errorBox(d.streams.error)); return; }
    const st = d.streams.value;
    const live = st.filter((s) => s.observed_state === 'live' && s.live);
    const recBy = new Set(d.recs.ok ? d.recs.value.data.items.filter((r) => r.state === 'recording').map((r) => r.stream) : []);
    if (!live.length) {
      replace(onAirBox, st.length
        ? emptyState({ compact: true, icon: 'live', title: t('dashboard.onAirEmpty'), body: t('dashboard.onAirEmptyBody'), actions: [h('a', { class: 'btn sm', href: '#/send' }, icon('camera'), t('dashboard.goLive')), h('a', { class: 'btn sm ghost', href: guideUrl('04-ingest-playback.md'), target: '_blank', rel: 'noopener noreferrer' }, t('dashboard.howToSend'), icon('external'))] })
        : emptyState({ compact: true, icon: 'streams', title: t('dashboard.noStreams'), body: t('dashboard.noStreamsBody'), actions: [h('a', { class: 'btn sm', href: '#/streams?new=1' }, icon('plus'), t('dashboard.onboarding.streamAction'))] }));
      return;
    }
    replace(onAirBox, h('ul', { class: 'onair-grid', 'data-testid': 'onair-list' }, live.map((s) => {
      const v = viewersOf(s);
      return h('li', { 'data-stream': s.id },
        h('a', { class: 'onair-card', href: `#/streams/${encodeURIComponent(s.id)}` },
          thumb(s),
          h('div', { class: 'onair-body' },
            h('div', { class: 'onair-title' }, h('strong', { class: 'truncate' }, s.name), recBy.has(s.id) ? badge(t('status.recording'), 'rec') : null),
            h('dl', { class: 'onair-stats' },
              h('div', null, h('dt', null, t('dashboard.stat.bitrate')), h('dd', { class: 'num' }, fmtBps(s.live.bitrate_bps))),
              h('div', null, h('dt', null, t('dashboard.stat.viewers')), h('dd', { class: 'num' }, v === null ? '—' : fmtNumber(v))),
              h('div', null, h('dt', null, t('dashboard.stat.uptime')), h('dd', { class: 'num js-since', 'data-since': s.live.started_at }, fmtDuration(Date.now() - new Date(s.live.started_at).getTime())))),
            h('div', { class: 'onair-meta' }, h('span', { class: 'tag' }, String(s.live.transport || '').toUpperCase()), h('span', { class: 'muted small' }, t('dashboard.since', { time: fmtClock(s.live.started_at) }))))));
    })));
    loadThumbs(live);
  };

  // ---------------------------------------------------------------- 録画中・予約
  const drawRecording = () => {
    const d = last;
    if (!d.recs.ok) { replace(recBox, errorBox(d.recs.error)); return; }
    const nameOf = new Map((d.streams.ok ? d.streams.value : []).map((s) => [s.id, s.name]));
    const now = d.recs.value.data.items.filter((r) => r.state === 'recording');
    replace(recBox, now.length
      ? h('ul', { class: 'mini-list' }, now.slice(0, 6).map((r) => h('li', null,
        h('a', { href: `#/recordings/${encodeURIComponent(r.id)}` }, badge(t('status.recording'), 'rec'), h('span', { class: 'truncate' }, r.name || nameOf.get(r.stream) || r.id.slice(0, 8))),
        h('span', { class: 'num muted js-since', 'data-since': r.started_at }, fmtDuration(Date.now() - new Date(r.started_at).getTime())))))
      : h('p', { class: 'muted small quiet' }, t('dashboard.recordingEmpty')));
  };

  const drawUpcoming = () => {
    const d = last;
    if (!d.scheds.ok) { replace(schedBox, errorBox(d.scheds.error)); return; }
    const now = Date.now();
    const up = d.scheds.value
      .filter((s) => (s.state === 'pending' || s.state === 'running') && new Date(s.end_at).getTime() > now)
      .sort((a, b) => a.start_at.localeCompare(b.start_at));
    const actionText = (a) => (has(`schedules.action.${a}`) ? t(`schedules.action.${a}`) : a);
    replace(schedBox, up.length
      ? h('ul', { class: 'mini-list' }, up.slice(0, 6).map((s) => h('li', null,
        h('a', { href: `#/schedules/${encodeURIComponent(s.id)}` },
          s.state === 'running' ? statusPill('running') : h('span', { class: 'tag' }, actionText(s.action)),
          h('span', { class: 'truncate' }, s.name || s.id.slice(0, 8))),
        h('span', { class: 'muted small nowrap', title: fmtTime(s.start_at) }, s.state === 'running' ? t('dashboard.endsIn', { rel: fmtRelative(s.end_at) }) : fmtRelative(s.start_at)))))
      : h('div', { class: 'quiet' }, h('p', { class: 'muted small' }, t('dashboard.upcomingEmpty')), h('a', { class: 'btn sm', href: '#/schedules?new=1' }, icon('plus'), t('palette.newSchedule'))));
  };

  // ---------------------------------------------------------------- 最近の出来事・能力
  const drawActivity = () => {
    const items = recent().filter((a) => a.type !== 'grid.decisions' || (a.data && a.data.data && a.data.data.decisions && a.data.data.decisions.length));
    const nameOf = new Map(last && last.streams.ok ? last.streams.value.map((s) => [s.id, s.name]) : []);
    replace(actBox, items.length
      ? items.slice(0, 12).map((a) => {
        const rid = a.data && a.data.resource_id;
        const name = rid && nameOf.get(rid);
        const href = rid && /^stream\./.test(a.type) && a.type !== 'stream.deleted' ? `#/streams/${encodeURIComponent(rid)}` : null;
        return h('li', null,
          h('time', { datetime: a.at.toISOString() }, fmtClock(a.at)),
          h('span', { class: 'act-label' }, activityLabel(a.type)),
          name ? (href ? h('a', { href }, name) : h('span', null, name)) : rid ? h('code', { class: 'muted' }, String(rid).slice(0, 8)) : null);
      })
      : h('li', { class: 'muted small' }, t('dashboard.activityEmpty')));
  };

  const drawCaps = () => {
    if (!caps) return;
    const ingest = Object.entries(caps.ingest).filter(([, v]) => v).map(([k]) => k.toUpperCase().replace('_PULL', ' pull'));
    const out = [`HLS (${caps.output.hls.containers.map((c) => (c === 'fmp4' ? 'fMP4' : 'TS')).join(' · ')})`];
    if (caps.output.hls.ll_hls) out.push('LL-HLS');
    if (caps.output.whep) out.push('WebRTC (WHEP)');
    if (caps.output.rtmp_play) out.push('RTMP');
    if (caps.output.moq) out.push('MoQ');
    const tc = caps.codecs.transcode;
    const codecs = [...new Set(tc.map((x) => x.codec))].map((c) => {
      const enc = tc.some((x) => x.codec === c && x.direction === 'encode');
      const dec = tc.some((x) => x.codec === c && x.direction === 'decode');
      const hw = tc.some((x) => x.codec === c && x.hardware);
      return h('span', { class: 'cap-chip' }, c.toUpperCase(), h('small', null, [enc && t('nodes.encodeShort'), dec && t('nodes.decodeShort')].filter(Boolean).join('/')), hw ? badge('GPU', 'ok') : null);
    });
    replace(capBox, h('dl', { class: 'cap-summary' },
      h('div', null, h('dt', null, icon('download'), t('dashboard.capIngest')), h('dd', null, ingest.join(' · ') || '—')),
      h('div', null, h('dt', null, icon('upload'), t('dashboard.capOutput')), h('dd', null, out.join(' · '))),
      h('div', null, h('dt', null, icon('zap'), t('dashboard.capTranscode')), h('dd', null, codecs.length ? h('div', { class: 'row' }, codecs) : h('span', { class: 'muted' }, t('dashboard.capPassthroughOnly'), ' ', caps.codecs.passthrough.join(', ')))),
      caps.pipelines ? null : h('div', null, h('dt', null, icon('pipelines'), t('nav.pipelines')), h('dd', { class: 'muted' }, t('dashboard.capNoPipelines')))));
  };

  // ---------------------------------------------------------------- 入力・出力の状態（後から）
  const scanIo = async () => {
    if (!last || !last.streams.ok) return;
    const st = last.streams.value.slice().sort((a, b) => (b.observed_state === 'live') - (a.observed_state === 'live')).slice(0, IO_SCAN_MAX);
    const found = [];
    await Promise.all(st.map(async (s) => {
      const [i, o] = await Promise.all([settle(inputs.list(s.id)), settle(outputs.list(s.id))]);
      for (const [kind, r] of [['input', i], ['output', o]]) {
        if (!r.ok) continue;
        for (const x of r.value.data.items) {
          if (!x.enabled || !x.status) continue;
          if (x.status.state === 'error' || (x.status.state === 'backoff' && x.status.last_error)) {
            found.push({ kind, stream: s.id, protocol: x.provider || x.protocol, error: x.status.last_error });
          }
        }
      }
    }));
    if (disposed) return;
    ioProblems = found;
    const att = collectAttention();
    drawHealth(att);
    drawAttention(att);
  };

  let seq = 0;
  const refresh = async () => {
    const my = ++seq;
    const [st, rec, sch, nodes, gr, dev, tok] = await Promise.all([
      settle(listAll(streams.list)),
      settle(recordings.list({ limit: 200 })),
      settle(listAll(schedules.list)),
      settle(system.nodes()),
      settle(gridApi.get()),
      settle(system.devices()),
      settle(listAll(tokens.list)),
    ]);
    if (my !== seq || disposed) return;
    last = { streams: st, recs: rec, scheds: sch, nodes, grid: gr, devices: dev, tokens: tok };
    const att = collectAttention();
    drawChecklist();
    drawHealth(att);
    drawAttention(att);
    drawOnAir();
    drawRecording();
    drawUpcoming();
    drawActivity();
    scanIo();
  };

  drawCaps();
  drawActivity();
  await refresh();

  // 経過時間を毎秒、今の画を 15 秒ごとに
  const tick = setInterval(() => {
    for (const el of root.querySelectorAll('.js-since')) el.textContent = fmtDuration(Date.now() - new Date(el.dataset.since).getTime());
  }, 1000);
  const snapTimer = setInterval(() => {
    if (last && last.streams.ok && document.visibilityState === 'visible') loadThumbs(last.streams.value.filter((s) => s.observed_state === 'live' && s.live));
  }, SNAP_EVERY);
  const offs = [
    refreshOn(['stream.', 'recording.', 'schedule.', 'node.', 'device.', 'input.', 'output.', 'grid.'], refresh, { wait: 800 }),
    on('activity', drawActivity),
  ];
  return () => {
    disposed = true;
    clearInterval(tick);
    clearInterval(snapTimer);
    offs.forEach((off) => off());
    for (const v of thumbs.values()) if (v.url) URL.revokeObjectURL(v.url);
  };
}
