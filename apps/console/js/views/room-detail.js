// 部屋の詳細: 今の参加者（役割・接続・トラック・ビットレート・往復時間・損失）を生で、切断、メッセージの送信、
// 参加トークンの発行、このブラウザでの試しの参加、部屋の出来事、メディアパイプライン（録画・合成して配信など）への入口、設定。
//
// 参加者と統計は見えている間 3 秒ごとに読み、部屋の中の出来事（SSE。参加者がいるノードにだけ流れる）でもすぐ読み直す。
// 表を描き直してもフォーカスが失われないように、フォーカスのあった要素の data-testid で戻す。

import { h, replace } from '../lib/dom.js';
import { modal, confirmDialog, reportError, errorBox, toast, copyText } from '../lib/ui.js';
import { rooms, mediaPipelines } from '../api/realtime.js';
import { fmtTime, fmtRelative, fmtBps, fmtNumber, fmtDurationText, fmtClock } from '../lib/format.js';
import { on } from '../lib/bus.js';
import { recent as recentActivity } from '../lib/activity.js';
import { t, tn } from '../i18n/index.js';
import {
  button, iconButton, pageHeader, panel, emptyState, statusPill, statTile, badge, alert, kvList, copyField, attachMenu, formField, helpHint,
} from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { rememberRecent } from '../lib/recent.js';
import { debounce } from '../lib/dom.js';
import { participantName, shortId, trackSummary, packetsLost, roomTotals, signalingUrl, roomEventItem, pipelineUse } from '../lib/realtime.js';
import { roomPill, roomBadges, roomTitle, deleteRoom } from './rooms.js';
import { roomForm } from './room-form.js';
import { tokenDialog } from './room-token.js';
import { joinPanel } from './room-join.js';
import { mpStatePill, useCards, USE_ICON } from './media-pipelines.js';

const REASONS = ['normal', 'websocket_closed', 'ping_timeout', 'peer_failed', 'kicked', 'room_deleted', 'error', 'redirected', 'node_shutdown'];
const reasonText = (r) => (REASONS.includes(r) ? t(`rooms.reason.${r}`) : r);
const PLACE = ['running', 'degraded', 'pending', 'unschedulable', 'not_scheduled'];
const ROLE_KIND = { sendrecv: 'accent', sendonly: 'info', recvonly: 'outline' };

/** 参加者のトラックの表示（種類・コーデック・大きさ・層・ビットレート）。 */
function trackChips(summary) {
  if (!summary.length) return h('span', { class: 'muted small' }, t('rooms.p.noTracks'));
  return h('ul', { class: 'track-list' }, summary.map((tr) => h('li', { class: 'track-chip', 'data-kind': tr.kind },
    icon(tr.kind === 'audio' ? 'mic' : 'send', { size: 'sm', label: t(`rooms.kind.${tr.kind}`) }),
    h('span', { class: 'mono' }, (tr.codec || '?').toUpperCase()),
    tr.width ? h('span', null, `${tr.width}×${tr.height}${tr.fps ? ` · ${Math.round(tr.fps)} fps` : ''}`) : null,
    tr.rids && tr.rids.length > 1 ? h('span', { class: 'tag', title: t('rooms.p.simulcast') }, icon('layers', { size: 'sm' }), tn('rooms.p.layers', tr.rids.length)) : null,
    tr.bitrate_bps !== null ? h('span', { class: 'num muted' }, fmtBps(tr.bitrate_bps)) : null,
    tr.layers.length > 1 ? h('span', { class: 'layer-rates', 'data-testid': 'layer-rates' }, tr.layers.map((l) => h('span', null, h('span', { class: 'mono' }, l.rid || '—'), ' ', fmtBps(l.bitrate_bps)))) : null)));
}

/** 受け取っている映像と層（参加者の受信の枠 ＋ 統計の今の層）。 */
function receiving(p, s, nameOf) {
  const slots = (p.subscriptions || []).filter((x) => x.kind === 'video' && x.state !== 'empty' && x.track_id);
  if (!slots.length) return null;
  const ridOf = (mid) => { const x = s && Array.isArray(s.subscriptions) ? s.subscriptions.find((y) => y.mid === mid) : null; return x ? x.rid : null; };
  return h('div', { class: 'pcard-recv' }, icon('download', { size: 'sm' }), h('span', null, t('rooms.p.receiving')),
    slots.map((x) => {
      const rid = ridOf(x.mid);
      return h('span', { class: 'chip' }, nameOf(x.connection_id || String(x.track_id).split('.')[0], x.client_id),
        x.state === 'paused' ? h('span', { class: 'muted' }, t('rooms.p.pausedLayer')) : rid ? h('span', { class: 'mono muted' }, rid) : null);
    }));
}

export async function render(root, [id], { navigate }) {
  let room = null;
  let etag = null;
  let participants = [];
  let pError = null;
  let stats = null;
  let mps = [];
  let events = [];
  const names = new Map(); // connection_id → 呼び名（退出の後の出来事にも使う）
  const timers = [];

  const head = h('div');
  const tiles = h('div', { class: 'grid cards stat-grid', 'data-testid': 'room-stats' });
  const pBox = h('div', { 'data-testid': 'participants' });
  const mpBox = h('div', { 'data-testid': 'room-mps' });
  const evBox = h('ol', { class: 'timeline', 'data-testid': 'room-events', 'aria-live': 'polite', 'aria-relevant': 'additions' });
  const infoBox = h('div');
  const settingsBox = h('div');
  const updatedAt = h('span', { class: 'small muted live-note' }, h('span', { class: 'pulse', 'aria-hidden': 'true' }), t('rooms.p.live'));

  const nameOf = (conn, client) => names.get(conn) || client || shortId(conn);

  const fetchRoom = async () => {
    const r = await rooms.get(id);
    room = r.data;
    etag = r.etag;
  };

  // ---------------------------------------------------------------- 見出し
  const renderHead = () => {
    const more = iconButton('more', t('common.more'), { variant: 'secondary', testid: 'room-more' });
    attachMenu(more, () => [
      { label: t('rooms.copyId'), icon: 'copy', onSelect: () => copyText(room.id, t('rooms.id')) },
      { label: t('rooms.newMediaPipeline'), icon: 'media-pipelines', onSelect: () => navigate(`/media-pipelines/new?room=${encodeURIComponent(id)}`) },
      { separator: true },
      { label: t('rooms.delete'), icon: 'trash', danger: true, testid: 'room-delete', onSelect: async () => { if (await deleteRoom(room, participants.length)) navigate('/rooms'); } },
    ]);
    replace(head, pageHeader({
      title: roomTitle(room) || room.id,
      badges: [roomPill(room, pError ? null : participants.length, 'room-state')],
      description: roomTitle(room) ? h('span', { class: 'mono' }, room.id) : null,
      crumbs: [{ label: t('nav.rooms'), href: '#/rooms' }, { label: roomTitle(room) || room.id }],
      actions: [
        button(t('rooms.sendMessage'), { icon: 'message', testid: 'room-message', onClick: () => sendMessage() }),
        button(t('rooms.editSettings'), { icon: 'settings', testid: 'room-edit', onClick: () => edit() }),
        more,
        button(t('rooms.issueToken'), { variant: 'primary', icon: 'user-plus', testid: 'room-issue-token', onClick: () => tokenDialog(room) }),
      ],
    }));
  };

  // ---------------------------------------------------------------- 数のタイル
  const renderTiles = () => {
    const tracks = participants.flatMap((p) => p.tracks || []);
    const tot = roomTotals(stats);
    const running = mps.filter((m) => m.desired_state === 'running').length;
    replace(tiles,
      statTile({ label: t('rooms.tile.participants'), value: pError ? '—' : fmtNumber(participants.length), sub: t('rooms.tile.ofMax', { max: fmtNumber(room.settings.max_participants) }), icon: 'users', testid: 'room-tile-participants' }),
      statTile({ label: t('rooms.tile.tracks'), value: fmtNumber(tracks.length), sub: t('rooms.tile.tracksSplit', { v: tracks.filter((x) => x.kind === 'video').length, a: tracks.filter((x) => x.kind === 'audio').length }), icon: 'live' }),
      statTile({ label: t('rooms.tile.in'), value: stats ? fmtBps(tot.inBps) : '—', sub: t('rooms.tile.out', { bps: stats ? fmtBps(tot.outBps) : '—' }), icon: 'activity' }),
      statTile({ label: t('rooms.tile.mp'), value: fmtNumber(running), sub: tn('rooms.tile.mpSub', mps.length), icon: 'media-pipelines' }));
  };

  // ---------------------------------------------------------------- 参加者
  const statsOf = (conn) => (stats && stats.participants ? stats.participants.find((s) => s.connection_id === conn) || null : null);
  // 参加者は 1 人 1 枚のカード（狭い列・携帯でも読めるように表にしない）。参加の順に並べる。
  const metric = (label, value, { warn, title } = {}) => h('span', { class: 'metric', title }, h('span', { class: 'metric-label' }, label), h('strong', { class: ['num', warn && 'text-warn'].filter(Boolean).join(' ') }, value));
  const initials = (name) => [...String(name || '?').trim()][0].toUpperCase();
  const participantCard = (p) => {
    const name = participantName(p);
    const s = statsOf(p.connection_id);
    const lost = packetsLost(s);
    const self = join.connectionId() === p.connection_id;
    const meta = [
      p.client_id && p.client_id !== name ? h('span', { class: 'mono' }, p.client_id) : null,
      h('span', { class: 'mono', title: p.connection_id }, shortId(p.connection_id)),
      p.joined_at ? h('span', { title: fmtTime(p.joined_at) }, t('rooms.p.joinedAgo', { rel: fmtRelative(p.joined_at) })) : null,
      h('span', null, p.signaling === 'remote' ? t('rooms.p.remote', { node: p.node || '?' }) : (p.signaling === 'datachannel' ? t('rooms.p.sig.datachannel') : t('rooms.p.sig.websocket'))),
    ].filter(Boolean);
    return h('li', { class: ['pcard', self && 'self'].filter(Boolean).join(' '), 'data-testid': `participant-${p.connection_id}`, 'data-name': name },
      h('div', { class: 'pcard-avatar', 'aria-hidden': 'true' }, initials(name)),
      h('div', { class: 'pcard-main' },
        h('div', { class: 'pcard-head' },
          h('strong', { class: 'pcard-name' }, name),
          self ? join.badge() : null,
          badge(t(`rooms.role.${p.role}`), ROLE_KIND[p.role], { title: p.role }),
          statusPill(p.peer_connected ? 'ok' : 'starting', { label: p.peer_connected ? t('rooms.p.connected') : t('rooms.p.connecting') })),
        h('div', { class: 'pcard-meta' }, meta),
        trackChips(trackSummary(p, s, room)),
        receiving(p, s, nameOf),
        h('div', { class: 'pcard-net' },
          metric(t('rooms.p.rtt'), s ? `${fmtNumber(s.rtt_ms)} ms` : '—'),
          metric(t('rooms.p.down'), s ? fmtBps(s.send_bitrate_bps) : '—'),
          metric(t('rooms.p.loss'), lost === null ? '—' : fmtNumber(lost), {
            warn: lost > 0, title: lost === null ? t('rooms.p.lossUnknown') : t('rooms.p.nack', { n: fmtNumber(s.nack_requested), r: fmtNumber(s.retransmitted_packets) }),
          }))),
      h('div', { class: 'pcard-actions' },
        iconButton('message', t('rooms.p.messageTo', { who: name }), { testid: `p-msg-${p.connection_id}`, onClick: () => sendMessage(p.connection_id) }),
        iconButton('user-x', t('rooms.p.kick', { who: name }), { testid: `p-kick-${p.connection_id}`, onClick: () => kick(p) })));
  };
  const renderParticipants = () => {
    if (pError) {
      const pending = pError.status === 503 || pError.status === 404;
      replace(pBox, alert(pending ? 'info' : 'error', pending ? t('rooms.p.notOpen') : t('rooms.p.loadFailed'), { small: true, testid: 'participants-error' }));
      return;
    }
    if (!participants.length) {
      replace(pBox, emptyState({
        compact: true, icon: 'users', title: t('rooms.p.emptyTitle'), body: t('rooms.p.emptyBody'), testid: 'participants-empty',
        actions: [
          button(t('rooms.issueToken'), { icon: 'user-plus', onClick: () => tokenDialog(room) }),
          button(t('rooms.join.tryHere'), { icon: 'camera', onClick: () => joinSection.scrollIntoView({ behavior: 'smooth', block: 'start' }) }),
        ],
      }));
      return;
    }
    // 描き直してもフォーカスを失わない（同じ data-testid の要素へ戻す）
    const focused = document.activeElement && pBox.contains(document.activeElement) ? document.activeElement.dataset.testid : null;
    const sorted = participants.slice().sort((a, b) => String(a.joined_at || '').localeCompare(String(b.joined_at || '')));
    replace(pBox, h('ul', { class: 'pcards', 'aria-label': t('rooms.p.caption'), 'data-testid': 'participants-list' }, sorted.map(participantCard)));
    if (focused) { const el = pBox.querySelector(`[data-testid="${CSS.escape(focused)}"]`); if (el) el.focus({ preventScroll: true }); }
  };

  const readLive = async () => {
    try {
      const [p, s] = await Promise.all([rooms.participants(id), rooms.stats(id).catch(() => null)]);
      participants = p.data.items;
      stats = s ? s.data : null;
      pError = null;
      for (const x of participants) names.set(x.connection_id, participantName(x));
    } catch (e) {
      if (e.status === 401) return;
      pError = e;
      participants = [];
      stats = null;
    }
    replace(updatedAt, h('span', { class: 'pulse', 'aria-hidden': 'true' }), t('rooms.p.updated', { time: fmtClock(new Date()) }));
  };
  let lastHeadKey = '';
  const refreshLive = async () => {
    await readLive();
    renderParticipants();
    renderTiles();
    // 見出しは状態が変わったときだけ作り直す（押そうとしているボタンを奪わない）
    const k = JSON.stringify([pError ? null : participants.length, room.generation]);
    if (k !== lastHeadKey) { lastHeadKey = k; renderHead(); }
  };

  // ---------------------------------------------------------------- 操作
  const kick = async (p) => {
    const who = participantName(p);
    const ok = await confirmDialog(t('rooms.kickTitle', { who }), t('rooms.kickBody', { who }), {
      ok: t('rooms.kickOk'), testid: 'kick-dialog', icon: 'user-x',
      consequences: [t('rooms.kickC1'), t('rooms.kickC2')],
    });
    if (!ok) return;
    try {
      await rooms.kick(id, p.connection_id);
      toast(t('rooms.kicked', { who }), 'ok');
      refreshLive();
    } catch (e) {
      reportError(e, t('rooms.kickOk'));
    }
  };

  const sendMessage = async (to = null) => {
    const target = h('select', { 'data-testid': 'msg-to' },
      h('option', { value: '' }, t('rooms.msg.everyone', { n: participants.length })),
      participants.map((p) => h('option', { value: p.connection_id, selected: p.connection_id === to }, `${participantName(p)} (${shortId(p.connection_id)})`)));
    const label = h('input', { type: 'text', placeholder: '#chat', spellcheck: 'false', 'data-testid': 'msg-label' });
    const data = h('textarea', { rows: 4, spellcheck: 'false', placeholder: '{"notice":"5 分後に終わります"}', 'data-testid': 'msg-data' });
    const dataField = formField({ label: t('rooms.msg.data'), input: data, required: true, hint: t('rooms.msg.dataHint') });
    const errBox = h('div');
    await modal(t('rooms.msg.title'), () => [h('div', { class: 'form' },
      formField({ label: t('rooms.msg.to'), input: target }),
      formField({ label: t('rooms.msg.label'), input: label, optional: true, hint: t('rooms.msg.labelHint'),
        help: helpHint({ title: t('rooms.msg.label'), body: t('rooms.msg.labelHelp'), guide: '10-rooms.md' }) }),
      dataField, errBox)], {
      icon: 'message', wide: true, testid: 'msg-dialog', initialFocus: '[data-testid="msg-data"]',
      actions: (close) => {
        const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'msg-send', onClick: async () => {
          dataField.setError(null);
          const raw = data.value;
          if (!raw.trim()) { dataField.setError(t('rooms.msg.empty')); data.focus(); return; }
          const lab = label.value.trim();
          if (lab && !lab.startsWith('#')) { replace(errBox, alert('error', t('rooms.msg.badLabel'), { small: true })); return; }
          let payload = raw;
          if (!lab) { try { payload = JSON.parse(raw); } catch { payload = raw; } }
          const body = { data: payload };
          if (lab) body.label = lab;
          if (target.value) body.to = [target.value];
          ok.disabled = true;
          replace(errBox);
          try {
            const { data: r } = await rooms.message(id, body);
            toast(tn('rooms.msg.delivered', r.delivered), r.delivered ? 'ok' : 'warn');
            close(true);
          } catch (e) {
            replace(errBox, errorBox(e, { title: t('rooms.msg.failed') }));
          } finally {
            ok.disabled = false;
          }
        } }, t('rooms.msg.send'));
        return [h('button', { type: 'button', onClick: () => close(false) }, t('common.cancel')), ok];
      },
    });
  };

  const edit = async () => {
    const form = roomForm(room, { hasParticipants: participants.length > 0 });
    const errBox = h('div');
    const updated = await modal(t('rooms.editTitle', { id: room.id }), () => [form.el, errBox], {
      icon: 'settings', wide: true, testid: 'room-edit-dialog',
      actions: (close) => {
        const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'room-edit-save', onClick: async () => {
          const body = form.value();
          if (!body) return;
          if (!Object.keys(body).length) { close(null); toast(t('common.noChanges'), 'info'); return; }
          ok.disabled = true;
          try {
            const r = await rooms.update(id, body, etag);
            etag = r.etag;
            close(r.data);
          } catch (e) {
            if (e.status === 412) {
              await fetchRoom();
              close(null);
              toast(t('rooms.staleEdit'), 'warn', 9000);
              renderAll();
              return;
            }
            replace(errBox, errorBox(e, { title: t('rooms.saveFailed') }));
          } finally {
            ok.disabled = false;
          }
        } }, t('common.save'));
        return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
      },
    });
    if (updated) {
      room = updated;
      renderAll();
      toast(t('common.saved'), 'ok');
    }
  };

  // ---------------------------------------------------------------- 接続先と設定
  const renderInfo = () => {
    const ps = room.placement_status || {};
    const url = signalingUrl(room.signaling_url);
    replace(infoBox, h('div', { class: 'form' },
      h('div', { class: 'field' }, h('div', { class: 'label' }, t('rooms.id')), copyField(room.id, t('rooms.id'))),
      h('div', { class: 'field' }, h('div', { class: 'label' }, t('rooms.info.signaling')), copyField(url || '', t('rooms.info.signaling'), { testid: 'room-signaling' }),
        h('div', { class: 'hint' }, room.signaling_url ? t('rooms.info.signalingHint') : t('rooms.info.signalingGuess'))),
      kvList([
        [t('rooms.info.placement'), h('span', { class: 'row' }, statusPill(PLACE.includes(ps.state) ? (ps.state === 'not_scheduled' ? 'pending' : ps.state === 'unschedulable' ? 'failed' : ps.state) : 'unknown', { label: PLACE.includes(ps.state) ? t(`rooms.place.${ps.state}`) : String(ps.state || '—') }),
          ps.node ? h('span', { class: 'small muted' }, ps.node_name || shortId(ps.node)) : null)],
        ps.since ? [t('rooms.info.since'), fmtTime(ps.since)] : null,
        ps.reasons && ps.reasons.length ? [t('rooms.info.reasons'), h('span', { class: 'small' }, ps.reasons.map((r) => `${r.reason || r.code || ''} ×${r.nodes ?? r.count ?? ''}`).join(', '))] : null,
        [t('rooms.info.created'), `${fmtTime(room.created_at)}${room.auto ? ` · ${t('rooms.feature.auto')}` : ''}`],
        [t('common.updated'), t('rooms.info.updatedGen', { time: fmtTime(room.updated_at), gen: room.generation })],
      ])));
  };
  const renderSettings = () => {
    const s = room.settings;
    replace(settingsBox,
      roomBadges(room),
      kvList([
        [t('rooms.form.max'), fmtNumber(s.max_participants)],
        [t('rooms.kv.codec'), `${(s.video_codec || '').toUpperCase()} / ${(s.audio_codec || 'opus').toUpperCase()}`],
        [t('rooms.form.recording'), t(`rooms.recMode.${s.recording || 'none'}`)],
        [t('rooms.feature.e2ee'), s.e2ee ? t('common.yes') : t('common.no')],
        [t('rooms.kv.bitrate'), s.max_video_bitrate_kbps ? `${fmtNumber(s.max_video_bitrate_kbps)} kbps` : t('rooms.form.noLimit')],
        [t('rooms.form.spotlight'), s.spotlight ? t('rooms.spotValue', { n: s.spotlight.number, mode: t(`rooms.form.unfocused${s.spotlight.unfocused === 'off' ? 'Off' : 'Lowest'}`) }) : t('common.no')],
        [t('rooms.feature.cascade'), s.cascade ? t('common.yes') : t('common.no')],
        [t('rooms.feature.filter'), s.forwarding_filter ? h('code', { class: 'small break' }, JSON.stringify(s.forwarding_filter)) : t('common.no')],
        [t('rooms.kv.labels'), (s.data_labels || []).join(', ')],
        room.auto ? [t('rooms.kv.emptyTimeout'), t('rooms.secs', { n: s.empty_timeout_secs })] : null,
        s.auth_webhook_url ? [t('rooms.feature.webhook'), h('code', { class: 'small break' }, s.auth_webhook_url)] : null,
      ]));
  };

  // ---------------------------------------------------------------- メディアパイプライン
  const newMp = (use) => `#/media-pipelines/new?room=${encodeURIComponent(id)}&use=${use}`;
  const renderMps = () => {
    const cards = useCards((u) => newMp(u), { compact: true, disabled: room.settings.e2ee ? ['broadcast', 'captions'] : [], disabledNote: t('rooms.mp.e2eeOff') });
    replace(mpBox,
      mps.length ? h('ul', { class: 'mp-mini' }, mps.map((m) => h('li', null,
        icon(USE_ICON[pipelineUse(m)] || 'media-pipelines'),
        h('a', { href: `#/media-pipelines/${encodeURIComponent(m.id)}`, class: 'truncate' }, m.name),
        m.managed ? h('span', { class: 'tag' }, t('mp.managed')) : null,
        h('span', { class: 'spacer' }),
        mpStatePill(m)))) : null,
      h('div', { class: 'small muted mt-2 mb-2' }, mps.length ? t('rooms.mp.more') : t('rooms.mp.none')),
      cards);
  };
  const readMps = async () => {
    try { mps = (await mediaPipelines.list({ room: id, limit: 100 })).data.items; } catch { mps = []; }
  };

  // ---------------------------------------------------------------- 出来事
  const renderEvents = () => {
    if (!events.length) {
      replace(evBox, h('li', { class: 'timeline-empty muted small' }, t('rooms.ev.empty')));
      return;
    }
    replace(evBox, events.slice(0, 60).map((x) => h('li', { class: ['timeline-item', x.tone].filter(Boolean).join(' ') },
      h('span', { class: 'timeline-icon' }, icon(x.icon, { size: 'sm' })),
      h('span', { class: 'timeline-text' }, t(x.key, { ...x.params, reason: x.params.reason ? reasonText(x.params.reason) : '', dur: x.params.ms !== undefined ? fmtDurationText(x.params.ms) : '' })),
      h('time', { datetime: x.at || '', title: x.at ? fmtTime(x.at) : '' }, x.at ? fmtClock(x.at) : ''))));
  };
  const addEvent = (ev) => {
    if (ev.type === 'room.connection.created' && ev.data && ev.data.data && ev.data.data.participant) {
      const p = ev.data.data.participant;
      names.set(p.connection_id, participantName(p));
    }
    const item = roomEventItem(ev, nameOf);
    if (!item) return;
    if (!item.at) item.at = new Date().toISOString();
    events.unshift(item);
    renderEvents();
  };

  const renderAll = () => {
    renderHead();
    renderTiles();
    renderInfo();
    renderSettings();
    renderParticipants();
    renderMps();
  };

  // ---------------------------------------------------------------- 組み立て
  try {
    await fetchRoom();
  } catch (e) {
    root.append(pageHeader({ title: t('nav.rooms'), crumbs: [{ label: t('nav.rooms'), href: '#/rooms' }, { label: id }] }),
      errorBox(e, { title: e.status === 404 ? t('rooms.notFound', { id }) : null, actions: [h('a', { class: 'btn sm', href: '#/rooms' }, icon('arrow-left'), t('common.backToList'))] }));
    return null;
  }
  rememberRecent({ kind: 'room', id, label: roomTitle(room) || id, href: `#/rooms/${encodeURIComponent(id)}` });
  const join = joinPanel(() => room, { onChange: () => setTimeout(refreshLive, 300) });
  const joinSection = panel({ title: t('rooms.join.title'), icon: 'camera', testid: 'join-panel',
    help: helpHint({ title: t('rooms.join.title'), body: t('rooms.join.help'), guide: '10-rooms.md' }) }, join.el);

  // これまでに届いていたこの部屋の出来事（ダッシュボードと同じ直近の分）
  for (const a of recentActivity().slice().reverse()) {
    if (a.data && a.data.resource_id === id) addEvent({ type: a.type, data: a.data });
  }

  root.append(
    head,
    tiles,
    h('div', { class: 'room-layout' },
      h('div', { class: 'room-main' },
        panel({ title: t('rooms.p.title'), icon: 'users', actions: [updatedAt], flush: false, testid: 'participants-panel' }, pBox),
        joinSection,
        panel({ title: t('rooms.ev.title'), icon: 'history', description: t('rooms.ev.desc') }, evBox)),
      h('div', { class: 'room-side' },
        panel({ title: t('rooms.mp.title'), icon: 'media-pipelines', description: t('rooms.mp.desc'),
          actions: [h('a', { class: 'btn sm ghost', href: `#/media-pipelines?room=${encodeURIComponent(id)}` }, t('common.all'), icon('arrow-right'))] }, mpBox),
        panel({ title: t('rooms.info.title'), icon: 'link' }, infoBox),
        panel({ title: t('rooms.settingsTitle'), icon: 'settings', actions: [button(t('common.edit'), { size: 'sm', icon: 'edit', onClick: () => edit() })] }, settingsBox))));

  await Promise.all([readLive(), readMps()]);
  lastHeadKey = JSON.stringify([pError ? null : participants.length, room.generation]);
  renderAll();
  renderEvents();

  const live = debounce(refreshLive, 250);
  const reloadRoom = debounce(async () => {
    try {
      await fetchRoom();
      renderHead();
      renderInfo();
      renderSettings();
      renderTiles();
    } catch (e) {
      if (e.status === 404) { toast(t('rooms.gone', { id }), 'warn'); navigate('/rooms'); }
    }
  }, 300);
  const reloadMps = debounce(async () => { await readMps(); renderMps(); renderTiles(); }, 300);
  const offs = [
    on('sse', (ev) => {
      const d = ev.data || {};
      if (ev.type.startsWith('media_pipeline.')) { if (d.data && d.data.room === id) reloadMps(); else if (ev.type === 'media_pipeline.deleted') reloadMps(); return; }
      if (!ev.type.startsWith('room.') || d.resource_id !== id) return;
      if (ev.type === 'room.deleted') { toast(t('rooms.gone', { id }), 'warn'); navigate('/rooms'); return; }
      addEvent(ev);
      if (ev.type === 'room.updated') reloadRoom();
      else live();
    }),
    on('resync', () => { reloadRoom(); live(); reloadMps(); }),
  ];
  timers.push(setInterval(() => { if (document.visibilityState === 'visible') refreshLive(); }, 3000));
  timers.push(setInterval(() => { if (document.visibilityState === 'visible') reloadMps(); }, 10000));
  return () => {
    offs.forEach((off) => off());
    timers.forEach(clearInterval);
    live.cancel();
    reloadRoom.cancel();
    reloadMps.cancel();
    join.stop();
  };
}
