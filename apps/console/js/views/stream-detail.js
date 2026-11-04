// 配信の詳細。上から「見出し（状態・主の操作）」「（作成の直後だけ）送り手をつなぐ」「タブ」。
//
// タブ（?tab= に残る）: 概要（プレビューと受信の様子）・接続（送り先の URL と鍵、取り込みの入力）・同時配信（出力）・
// 録画（自動録画・録画の一覧・静止画）・再生と字幕・設定（フォームと取り消せない操作）。
//
// - publish の開始・終了とビットレートは SSE のイベントにならないので、見えている間は 5 秒ごと（入力を待っている案内の間は 2 秒ごと）に読む。
// - ストリームキーは作成と作り直しの応答でだけ届く。メモリ（lib/secrets.js）で受け取り、この画面を離れたら捨てる。
// - ビットレートの線は lib/streaming.js の履歴（このタブで読んだ値）から描く。

import { h, replace } from '../lib/dom.js';
import { load, modal, confirmDialog, reportError, errorBox, toast, describeError, downloadBlob } from '../lib/ui.js';
import { streams, recordings } from '../api/resources.js';
import { fmtTime, fmtRelative, fmtBps, fmtBytes, fmtDuration, fmtNumber, fmtClock } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { streamForm } from './stream-form.js';
import { stateBadge, streamSubline } from './streams.js';
import { recBadge } from './recordings.js';
import { takeSecret } from '../lib/secrets.js';
import { playerWidget } from '../player/widget.js';
import { ioSection, captionsSection, playbackSection, ioStatus } from './stream-io.js';
import { restreamSection } from './stream-restream.js';
import { inputs as inputsApi } from '../api/resources.js';
import { t } from '../i18n/index.js';
import {
  button, iconButton, pageHeader, panel, alert, statusPill, emptyState, helpHint, tabs, badge, copyField, segmented, sparkline, attachMenu, toggle, dataTable, kvList, copyText,
} from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { rememberRecent, forgetRecent } from '../lib/recent.js';
import { invalidatePalette } from '../palette.js';
import {
  streamState, liveSummary, uptimeMs, viewerCount, recordHealth, healthSamples, healthStats, liveThumb, recordingThumb, handOffKey, hashParams, setHashParams,
} from '../lib/streaming.js';

const METHODS = ['rtmp', 'srt', 'whip', 'pull'];

export async function render(root, [id], { caps, navigate }) {
  let stream = null;
  let etag = null;
  let secret = takeSecret(id);
  let setup = METHODS.includes(hashParams().get('setup')) ? hashParams().get('setup') : null;
  let recs = [];
  let activeRec = null;
  let captions = null;
  let restream = null;
  let inputsBox = null;
  let tabsEl = null;
  const blobUrls = new Set();
  const timers = [];
  const autoplay = hashParams().get('play') === '1';
  if (autoplay) setHashParams({ play: null });

  const headBox = h('div');
  const setupBox = h('div');
  const previewBox = h('div', { class: 'preview-box' });
  const healthBox = h('div');
  const connectKeyBox = h('div');
  const connectGuideBox = h('div');
  const recPanelBox = h('div');
  const recListBox = h('div');
  const galleryBox = h('div');
  const settingsBox = h('div');
  const dangerBox = h('div');
  let connectMethod = 'rtmp';
  const grabBtn = button(t('streams.detail.grab'), { size: 'sm', icon: 'image', testid: 'grab-frame', onClick: () => grabFrame() });
  let player = null;

  const fetchStream = async () => {
    const r = await streams.get(id);
    stream = r.data;
    etag = r.etag;
    recordHealth(stream);
    return stream;
  };

  const state = () => streamState(stream);
  const selectTab = (tab) => { if (tabsEl) { tabsEl.select(tab); tabsEl.querySelector(`[role="tab"][data-tab="${tab}"]`)?.focus(); } };

  // ---------------------------------------------------------------- 見出し
  const moreMenu = () => {
    const b = iconButton('more', t('common.more'), { variant: 'secondary', testid: 'stream-more' });
    attachMenu(b, () => [
      { label: t('streams.row.copyRtmp'), icon: 'copy', onSelect: () => copyText(stream.ingest.rtmp_url, t('streams.detail.rtmpUrl')) },
      stream.ingest.whip_url ? { label: t('streams.row.sendBrowser'), icon: 'camera', onSelect: () => goSend() } : null,
      { label: t('streams.detail.grab'), icon: 'image', onSelect: () => grabFrame() },
      { separator: true },
      stream.desired_state === 'running'
        ? { label: t('streams.detail.stop'), icon: 'stop', onSelect: () => setDesired('stopped'), testid: 'stream-menu-stop' }
        : { label: t('streams.detail.resume'), icon: 'play', onSelect: () => setDesired('running') },
      { label: t('streams.detail.rotate'), icon: 'key', onSelect: () => rotate() },
      { separator: true },
      { label: t('streams.detail.deleteMenu'), icon: 'trash', danger: true, onSelect: () => remove(), testid: 'stream-menu-delete' },
    ]);
    return b;
  };

  const renderHead = () => {
    const st = state();
    const actions = [];
    if (st === 'live') {
      actions.push(activeRec
        ? button(t('streams.detail.stopRecording'), { icon: 'stop', testid: 'stream-rec-stop', onClick: () => stopRecording() })
        : button(t('streams.detail.startRecording'), { icon: 'record', testid: 'stream-rec-start', onClick: () => startRecording() }));
    } else if (st === 'stopped') {
      actions.push(button(t('streams.detail.resume'), { icon: 'play', variant: 'primary', testid: 'stream-resume', onClick: () => setDesired('running') }));
    } else if (!setup) {
      actions.push(button(t('streams.detail.howToConnect'), { icon: 'upload', testid: 'stream-how', onClick: () => selectTab('connect') }));
    }
    actions.push(moreMenu());
    const badges = [stateBadge(stream)];
    if (st === 'live') badges.push(h('span', { class: 'uptime-chip num', 'data-live': 'head-uptime', title: t('streams.col.uptime') }, icon('clock', { size: 'sm' }), fmtDuration(uptimeMs(stream))));
    if (activeRec) badges.push(badge(t('streams.rec.now'), 'rec'));
    replace(headBox, pageHeader({
      title: stream.name,
      badges,
      description: h('span', { 'data-live': 'head-sub' }, streamSubline(stream)),
      crumbs: [{ label: t('nav.streams'), href: '#/streams' }, { label: stream.name }],
      actions,
      testid: 'stream-header',
    }));
  };

  // ---------------------------------------------------------------- 送り手のつなぎ方（作成の直後の案内と「接続」タブで同じものを使う）
  const keyCallout = () => (secret
    ? h('div', { class: 'key-callout', role: 'note', 'data-testid': 'key-once' },
      h('div', { class: 'kc-icon' }, icon('key')),
      h('div', { class: 'kc-body' },
        h('strong', null, t('streams.key.onceTitle')),
        h('span', null, t('streams.key.onceBody'))))
    : h('div', { class: 'key-callout muted-callout', 'data-testid': 'key-hidden' },
      h('div', { class: 'kc-icon' }, icon('secrets')),
      h('div', { class: 'kc-body' },
        h('strong', null, t('streams.key.hiddenTitle')),
        h('span', null, t('streams.key.hiddenBody')),
        h('div', null, button(t('streams.detail.rotate'), { size: 'sm', icon: 'refresh', testid: 'key-rotate', onClick: () => rotate() })))));

  const keyField = (testid) => (secret ? copyField(secret, t('streams.detail.streamKey'), { secret: true, testid }) : h('div', { class: 'key-missing' }, icon('secrets', { size: 'sm' }), t('streams.key.notShown')));

  const step = (n, title, body) => h('li', { class: 'g-step' }, h('span', { class: 'g-num', 'aria-hidden': 'true' }, String(n)), h('div', { class: 'g-body' }, h('div', { class: 'g-title' }, title), body));

  const goSend = () => {
    if (secret) handOffKey(id, secret);
    navigate(`/send?stream=${encodeURIComponent(id)}`);
  };

  const guide = (method, { forSetup = false } = {}) => {
    const ig = stream.ingest;
    if (method === 'rtmp') {
      return h('div', { class: 'guide', 'data-method': 'rtmp' },
        h('p', { class: 'g-lead' }, t('streams.guide.rtmpLead')),
        h('ol', { class: 'g-steps' },
          step(1, t('streams.guide.obs1'), h('p', { class: 'hint' }, t('streams.guide.obs1Hint'))),
          step(2, t('streams.guide.obs2'), h('p', { class: 'hint' }, t('streams.guide.obs2Hint'))),
          step(3, t('streams.guide.obs3'), copyField(ig.rtmp_url, t('streams.detail.rtmpUrl'), { testid: forSetup ? 'setup-rtmp-url' : 'connect-rtmp-url' })),
          step(4, t('streams.guide.obs4'), keyField(forSetup ? 'setup-key' : 'connect-key')),
          step(5, t('streams.guide.obs5'), h('p', { class: 'hint' }, t('streams.guide.obs5Hint')))),
        secret ? h('details', { class: 'disclosure small' }, h('summary', null, icon('info'), h('span', null, t('streams.guide.fullUrl'))),
          h('div', { class: 'stack' }, h('p', { class: 'hint' }, t('streams.guide.fullUrlHint')), copyField(`${ig.rtmp_url}/${secret}`, t('streams.detail.fullTargetLabel'), { secret: true }))) : null);
    }
    if (method === 'srt') {
      if (!ig.srt_url) return h('div', { class: 'guide' }, alert('info', t('streams.guide.noSrt'), { title: t('streams.guide.noSrtTitle') }));
      const sep = ig.srt_url.includes('?') ? '&' : '?';
      return h('div', { class: 'guide', 'data-method': 'srt' },
        h('p', { class: 'g-lead' }, t('streams.guide.srtLead')),
        h('ol', { class: 'g-steps' },
          step(1, t('streams.guide.srt1'), secret
            ? copyField(`${ig.srt_url}${sep}streamid=publish:${secret}`, t('streams.guide.srtFull'), { secret: true, testid: forSetup ? 'setup-srt-url' : 'connect-srt-url' })
            : h('div', { class: 'stack' }, copyField(ig.srt_url, t('streams.detail.srtUrl')), h('p', { class: 'hint' }, t('streams.guide.srtNoKey', { format: ig.srt_streamid_format || 'publish:{stream_key}' })))),
          step(2, t('streams.guide.srt2'), h('p', { class: 'hint' }, t('streams.guide.srt2Hint')))));
    }
    if (method === 'whip') {
      if (!ig.whip_url) return h('div', { class: 'guide' }, alert('info', t('send.noWhipUrlLong')));
      return h('div', { class: 'guide', 'data-method': 'whip' },
        h('p', { class: 'g-lead' }, t('streams.guide.whipLead')),
        h('div', { class: 'g-cta' },
          button(t('streams.guide.whipGo'), { variant: forSetup ? 'primary' : 'secondary', icon: 'camera', testid: forSetup ? 'setup-go-send' : 'connect-go-send', onClick: goSend }),
          h('span', { class: 'hint' }, secret ? t('streams.guide.whipGoHint') : t('streams.guide.whipGoHintNoKey'))),
        h('details', { class: 'disclosure small' }, h('summary', null, icon('monitor'), h('span', null, t('streams.guide.whipObs'))),
          h('ol', { class: 'g-steps' },
            step(1, t('streams.guide.whipObs1'), copyField(ig.whip_url, t('streams.detail.whipUrl'))),
            step(2, t('streams.guide.whipObs2'), keyField(null)))));
    }
    // pull: ノードが取りに行く入力の様子
    return h('div', { class: 'guide', 'data-method': 'pull' },
      h('p', { class: 'g-lead' }, t('streams.guide.pullLead')),
      h('div', { 'data-pull-status': '' }, pullStatusNode()),
      h('p', { class: 'hint' }, t('streams.guide.pullHint')));
  };

  let pullInputs = null;
  const pullStatusNode = () => {
    if (!pullInputs) return h('span', { class: 'muted small' }, t('common.loading'));
    if (!pullInputs.length) return h('p', { class: 'muted small' }, t('streams.guide.pullNone'));
    return h('ul', { class: 'pull-list' }, pullInputs.map((x) => h('li', null, h('code', { class: 'break' }, x.url), ioStatus(x.status, x.enabled))));
  };
  const loadPull = async () => {
    try { pullInputs = (await inputsApi.list(id)).data.items; } catch { pullInputs = []; }
    for (const el of root.querySelectorAll('[data-pull-status]')) replace(el, pullStatusNode());
  };

  // ---------------------------------------------------------------- 作成の直後: 送り手をつなぐ
  const signalNode = () => {
    const st = state();
    if (st === 'live') {
      const sum = liveSummary(stream);
      return h('div', { class: 'signal live', 'data-testid': 'setup-signal', 'data-state': 'live', role: 'status', 'aria-live': 'polite' },
        h('div', { class: 'sig-mark' }, icon('check')),
        h('div', { class: 'sig-text' },
          h('strong', null, t('streams.setup.receiving')),
          h('span', null, `${sum.text} · ${fmtBps(stream.live.bitrate_bps)}`)),
        button(t('streams.setup.openOverview'), { variant: 'primary', icon: 'arrow-right', testid: 'setup-done', onClick: () => finishSetup('overview') }));
    }
    if (st === 'stopped') {
      return h('div', { class: 'signal stopped', 'data-testid': 'setup-signal', 'data-state': 'stopped', role: 'status' },
        h('div', { class: 'sig-mark' }, icon('stop')),
        h('div', { class: 'sig-text' }, h('strong', null, t('streams.setup.stoppedTitle')), h('span', null, t('streams.setup.stoppedBody'))),
        button(t('streams.detail.resume'), { icon: 'play', onClick: () => setDesired('running') }));
    }
    return h('div', { class: 'signal waiting', 'data-testid': 'setup-signal', 'data-state': 'waiting', role: 'status', 'aria-live': 'polite' },
      h('div', { class: 'sig-mark' }, h('span', { class: 'sig-pulse' })),
      h('div', { class: 'sig-text' },
        h('strong', null, t('streams.setup.waiting')),
        h('span', null, t(`streams.setup.waitingBody.${setup}`))));
  };

  const finishSetup = (tab) => {
    setup = null;
    setHashParams({ setup: null });
    replace(setupBox);
    renderHead();
    if (tab) selectTab(tab);
  };

  const renderSetup = () => {
    if (!setup) { replace(setupBox); return; }
    const live = state() === 'live';
    replace(setupBox, h('section', { class: ['setup-card', live && 'is-live'].filter(Boolean).join(' '), 'data-testid': 'setup', 'aria-labelledby': 'setup-title' },
      h('div', { class: 'setup-head' },
        h('div', { class: 'setup-title' },
          h('span', { class: 'setup-kicker' }, icon('rocket', { size: 'sm' }), t('streams.setup.kicker')),
          h('h2', { id: 'setup-title' }, t(`streams.setup.title.${setup}`))),
        button(live ? t('common.close') : t('streams.setup.later'), { variant: 'ghost', size: 'sm', testid: 'setup-close', onClick: () => finishSetup(null) })),
      h('ol', { class: 'setup-steps', 'aria-label': t('streams.setup.progress') },
        h('li', { class: 'done' }, h('span', { class: 'ss-mark' }, icon('check')), t('streams.setup.step1')),
        h('li', { class: live ? 'done' : 'current', 'aria-current': live ? null : 'step' }, h('span', { class: 'ss-mark' }, live ? icon('check') : '2'), t(`streams.setup.step2.${setup}`)),
        h('li', { class: live ? 'done' : null, 'aria-current': live ? 'step' : null }, h('span', { class: 'ss-mark' }, live ? icon('check') : '3'), t('streams.setup.step3'))),
      h('div', { class: 'setup-body' },
        h('div', { class: 'setup-guide' }, setup !== 'pull' ? keyCallout() : null, guide(setup, { forSetup: true })),
        h('div', { class: 'setup-signal' }, signalNode()))));
    if (setup === 'pull') loadPull();
  };

  const updateSignal = () => {
    const el = setupBox.querySelector('[data-testid="setup-signal"]');
    if (!el) return;
    if (el.dataset.state !== state()) renderSetup();
    else if (state() === 'live') el.replaceWith(signalNode());
  };

  // ---------------------------------------------------------------- 概要: プレビューと受信の様子
  const renderPreview = () => {
    const st = state();
    grabBtn.hidden = st !== 'live';
    if (st === 'live') {
      if (!player) {
        player = playerWidget({
          getPlayback: async () => (await streams.playbackUrl(id)).data.playback, live: true, label: t('streams.detail.previewLabel'),
          poster: () => liveThumb(id), autostart: autoplay,
        });
      }
      if (!previewBox.contains(player.el)) {
        replace(previewBox, player.el,
          stream.hls.container === 'ts' ? alert('info', t('streams.detail.tsNote'), { small: true }) : h('p', { class: 'small muted mt-2' }, t('streams.detail.autoNote')));
      }
      return;
    }
    if (player) { player.stop(); player = null; }
    replace(previewBox, h('div', { class: 'player player-empty', 'data-testid': 'preview-empty' },
      h('div', { class: 'pe-inner' },
        icon(st === 'stopped' ? 'stop' : 'live'),
        h('strong', null, st === 'stopped' ? t('streams.preview.stoppedTitle') : t('streams.preview.waitingTitle')),
        h('span', null, st === 'stopped' ? t('streams.preview.stoppedBody') : t('streams.preview.waitingBody')),
        st === 'stopped'
          ? button(t('streams.detail.resume'), { size: 'sm', icon: 'play', onClick: () => setDesired('running') })
          : button(t('streams.detail.howToConnect'), { size: 'sm', icon: 'upload', onClick: () => selectTab('connect') }))));
  };

  const stat = (label, value, sub, extra) => h('div', { class: 'h-stat' },
    h('div', { class: 'h-label' }, label),
    h('div', { class: 'h-value num' }, value),
    sub ? h('div', { class: 'h-sub' }, sub) : null,
    extra || null);

  const renderHealth = () => {
    if (state() !== 'live') {
      replace(healthBox, emptyState({
        compact: true, icon: 'activity', testid: 'health-empty',
        title: t('streams.health.noSignal'),
        body: state() === 'stopped' ? t('streams.health.stoppedBody') : t('streams.health.waitingBody'),
      }));
      return;
    }
    const live = stream.live;
    const sum = liveSummary(stream);
    const stats = healthStats(id);
    const v = viewerCount(stream);
    const vb = live.viewers || {};
    const vparts = ['hls', 'webrtc', 'rtmp', 'srt', 'rtsp', 'moq'].filter((k) => vb[k]).map((k) => `${k.toUpperCase()} ${vb[k]}`);
    const trackRow = (kind, ic, main, sub) => h('div', { class: 'track-row' }, h('span', { class: 'tr-icon' }, icon(ic)), h('div', null, h('div', { class: 'tr-kind' }, kind), h('strong', null, main), sub ? h('div', { class: 'tr-sub' }, sub) : null));
    replace(healthBox,
      h('div', { class: 'health-grid', 'data-testid': 'health' },
        stat(t('streams.col.bitrate'), fmtBps(live.bitrate_bps),
          stats ? t('streams.health.range', { min: fmtBps(stats.min), max: fmtBps(stats.max) }) : t('streams.health.collecting'),
          h('div', { class: 'h-spark' }, sparkline({ values: healthSamples(id), width: 240, height: 44, label: t('streams.health.sparkLabel', { now: fmtBps(live.bitrate_bps) }), testid: 'bitrate-spark' }))),
        stat(t('streams.col.viewers'), v !== null ? fmtNumber(v) : String(live.viewers_hint), vparts.length ? vparts.join(' · ') : v !== null ? t('streams.health.viewersNone') : t('streams.health.viewersHint')),
        stat(t('streams.col.uptime'), h('span', { 'data-live': 'health-uptime' }, fmtDuration(uptimeMs(stream))), t('streams.health.since', { time: fmtClock(live.started_at) })),
        stat(t('streams.health.reconnects'), fmtNumber(live.epoch), live.epoch ? t('streams.health.reconnectsSome') : t('streams.health.reconnectsNone'))),
      h('div', { class: 'tracks' },
        sum.video ? trackRow(t('streams.health.video'), 'send', `${sum.video.codec}${sum.video.width ? ` · ${sum.video.width}×${sum.video.height}` : ''}`, sum.video.fps ? `${sum.video.fps} fps` : null) : null,
        sum.audio ? trackRow(t('streams.health.audio'), 'volume', sum.audio.codec, [sum.audio.channels ? t('streams.health.channels', { n: sum.audio.channels }) : null, sum.audio.rate ? `${fmtNumber(sum.audio.rate)} Hz` : null].filter(Boolean).join(' · ')) : null,
        trackRow(t('streams.detail.transport'), 'upload', String(live.transport || '').toUpperCase(), null),
        !sum.video && !sum.audio ? h('p', { class: 'muted small' }, t('streams.detail.noTracks')) : null));
  };

  // ---------------------------------------------------------------- 接続
  const renderConnect = () => {
    replace(connectKeyBox, keyCallout());
    replace(connectGuideBox, guide(connectMethod));
    if (connectMethod === 'pull') loadPull();
  };

  // ---------------------------------------------------------------- 録画
  const loadRecs = async () => {
    try {
      const { data } = await recordings.list({ stream: id, limit: 1000 });
      recs = data.items.sort((a, b) => b.started_at.localeCompare(a.started_at));
    } catch { recs = []; }
    const before = activeRec ? activeRec.id : null;
    activeRec = recs.find((r) => r.state === 'recording') || null;
    if ((activeRec ? activeRec.id : null) !== before) renderHead();
    renderRecPanel();
    renderRecList();
  };

  const renderRecPanel = () => {
    const auto = toggle({
      label: t('streams.form.autoRecord'), description: t('streams.rec.autoHint'), checked: stream.recording.enabled, testid: 'rec-auto-toggle',
      onChange: async (on) => {
        try {
          const r = await streams.update(id, { recording: { ...stream.recording, enabled: on } }, etag);
          stream = r.data; etag = r.etag;
          toast(on ? t('streams.rec.autoOn') : t('streams.rec.autoOff'), 'ok');
        } catch (e) {
          if (!(await on412(e))) reportError(e, t('streams.form.autoRecord'));
          renderRecPanel();
        }
      },
    });
    const live = state() === 'live';
    replace(recPanelBox,
      activeRec
        ? h('div', { class: 'rec-now' },
          badge(t('streams.rec.now'), 'rec', { size: 'lg' }),
          h('span', null, t('streams.rec.nowBody', { time: fmtClock(activeRec.started_at) })),
          h('a', { href: `#/recordings/${encodeURIComponent(activeRec.id)}`, class: 'btn sm' }, t('streams.rec.open')),
          button(t('streams.detail.stopRecording'), { size: 'sm', icon: 'stop', onClick: () => stopRecording() }))
        : h('div', { class: 'rec-now idle' },
          h('span', { class: 'muted' }, live ? t('streams.rec.idleLive') : t('streams.rec.idleWaiting')),
          button(t('streams.detail.startRecording'), { size: 'sm', icon: 'record', attrs: { disabled: !live }, onClick: () => startRecording() })),
      auto.el,
      h('p', { class: 'hint' }, stream.recording.retention_days ? t('streams.rec.keepDays', { n: stream.recording.retention_days }) : t('streams.rec.keepForever')));
  };

  const recThumbCell = (r) => {
    const el = h('div', { class: 'thumb sm', 'aria-hidden': 'true' }, icon('recordings'));
    if (r.thumbnail) recordingThumb(r.id).then((u) => { if (u) { el.prepend(h('img', { src: u, alt: '' })); el.classList.add('has-img'); } });
    return el;
  };
  const renderRecList = () => {
    replace(recListBox, recs.length
      ? dataTable({
        caption: t('streams.detail.recentRecordings'), rowKey: (r) => r.id,
        rows: recs.slice(0, 10),
        columns: [
          { label: t('streams.detail.started'), primary: true, render: (r) => h('div', { class: 'stream-cell' }, recThumbCell(r), h('div', { class: 'sc-text' }, h('a', { class: 'sc-name', href: `#/recordings/${encodeURIComponent(r.id)}` }, r.name || fmtTime(r.started_at)), h('div', { class: 'sc-sub' }, fmtRelative(r.started_at)))) },
          { label: t('common.state'), render: recBadge },
          { label: t('streams.detail.duration'), class: 'num', render: (r) => fmtDuration(r.duration_ms) },
          { label: t('streams.detail.size'), class: 'num', render: (r) => fmtBytes(r.bytes) },
        ],
      })
      : emptyState({ compact: true, icon: 'recordings', title: t('streams.detail.noRecordings'), body: t('streams.rec.emptyBody') }));
  };

  const loadGallery = () => load(galleryBox, async () => {
    const { data } = await streams.snapshots(id);
    const items = data.items.slice(-24).reverse();
    if (!items.length) {
      return emptyState({
        compact: true, icon: 'image', title: t('streams.detail.noSnapshots'), body: t('streams.detail.noSnapshotsBody'),
        actions: [button(t('streams.gallery.enable'), { size: 'sm', icon: 'settings', onClick: () => selectTab('settings') })],
      });
    }
    const grid = h('div', { class: 'gallery' });
    for (const it of items) {
      const img = h('img', { alt: t('streams.detail.snapAt', { time: fmtTime(it.taken_at) }), loading: 'lazy' });
      const fig = h('figure', null, img,
        h('figcaption', null,
          h('span', { title: `${it.width}×${it.height}・${fmtBytes(it.bytes)}・${it.reason}` }, fmtClock(it.taken_at)),
          h('button', { type: 'button', class: 'ghost icon-only sm', 'aria-label': t('streams.detail.snapDelete', { time: fmtTime(it.taken_at) }), onClick: async () => {
            if (!(await confirmDialog(t('streams.detail.snapDeleteTitle'), t('streams.detail.snapDeleteBody', { time: fmtTime(it.taken_at) }), { ok: t('common.delete') }))) return;
            try { await streams.deleteSnapshot(id, it.id); fig.remove(); } catch (e) { reportError(e, t('common.delete')); }
          } }, icon('trash'))));
      grid.appendChild(fig);
      streams.snapshotImage(id, it.id).then(({ data: b }) => { const u = URL.createObjectURL(b); blobUrls.add(u); img.src = u; }).catch(() => { img.alt = t('streams.detail.imgFailed'); });
    }
    return h('div', null, h('p', { class: 'small muted' }, t('streams.detail.snapCount', { n: data.items.length })), grid);
  });

  // ---------------------------------------------------------------- 設定
  const renderSettings = () => {
    const form = streamForm(stream, caps);
    const errBox = h('div');
    const save = button(t('streams.settings.save'), { variant: 'primary', icon: 'save', testid: 'stream-settings-save', onClick: async () => {
      const body = form.value();
      if (!body) return;
      if (!Object.keys(body).length) { toast(t('common.noChanges'), 'info'); return; }
      replace(errBox);
      try {
        const r = await streams.update(id, body, etag);
        stream = r.data; etag = r.etag;
        toast(t('common.saved'), 'ok');
        renderHead();
        renderSettings();
        renderRecPanel();
        invalidatePalette();
      } catch (e) {
        if (e.status === 412) { await reload(); renderSettings(); toast(t('streams.detail.staleEdit'), 'warn', 9000); return; }
        replace(errBox, errorBox(e, { title: t('streams.detail.saveFailed') }));
      }
    } });
    replace(settingsBox, form.el, errBox, h('div', { class: 'actions sticky-actions' }, save));
    const running = stream.desired_state === 'running';
    replace(dangerBox, h('ul', { class: 'danger-list' },
      h('li', null,
        h('div', null, h('strong', null, running ? t('streams.danger.stopTitle') : t('streams.danger.resumeTitle')), h('p', null, running ? t('streams.danger.stopBody') : t('streams.danger.resumeBody'))),
        running ? button(t('streams.detail.stop'), { icon: 'stop', testid: 'stream-stop', onClick: () => setDesired('stopped') }) : button(t('streams.detail.resume'), { icon: 'play', onClick: () => setDesired('running') })),
      h('li', null,
        h('div', null, h('strong', null, t('streams.detail.rotate')), h('p', null, t('streams.danger.rotateBody'))),
        button(t('streams.detail.rotate'), { icon: 'key', testid: 'stream-rotate', onClick: () => rotate() })),
      h('li', null,
        h('div', null, h('strong', null, t('streams.detail.deleteTitle')), h('p', null, t('streams.danger.deleteBody'))),
        button(t('common.delete'), { variant: 'danger', icon: 'trash', testid: 'stream-delete', onClick: () => remove() }))));
  };

  // ---------------------------------------------------------------- 読み直し
  let shapeBefore = '';
  const shapeOf = () => JSON.stringify([stream.name, stream.desired_state, stream.observed_state, stream.live && stream.live.epoch, streamSubline(stream)]);
  const renderAll = () => {
    renderHead();
    renderSetup();
    renderPreview();
    renderHealth();
    renderConnect();
    renderRecPanel();
    renderSettings();
    if (captions) captions.render();
    shapeBefore = shapeOf();
  };

  const reload = async () => {
    try {
      const ingestBefore = stream ? JSON.stringify(stream.ingest) : '';
      await fetchStream();
      const shape = shapeOf();
      if (shape !== shapeBefore) {
        shapeBefore = shape;
        renderHead();
        renderPreview();
        renderRecPanel();
        if (setup) renderSetup();
        if (restream) restream.reload();
        if (captions) captions.render();
      } else {
        // 数だけ差し替える（押そうとしているボタン・フォーカスを奪わない）
        const u = headBox.querySelector('[data-live="head-uptime"]');
        if (u) replace(u, icon('clock', { size: 'sm' }), fmtDuration(uptimeMs(stream)));
        updateSignal();
      }
      renderHealth();
      if (JSON.stringify(stream.ingest) !== ingestBefore) renderConnect();
      if (restream && restream.el.offsetParent) restream.refreshStatus();
    } catch (e) {
      if (e.status === 404) {
        toast(t('streams.detail.gone'), 'warn');
        navigate('/streams');
      } else if (e.status !== 401) reportError(e, t('streams.detail.reload'));
    }
  };

  const on412 = async (e) => {
    if (e.status === 412) {
      await reload();
      renderSettings();
      toast(t('streams.detail.stale'), 'warn', 8000);
      return true;
    }
    return false;
  };

  // ---------------------------------------------------------------- 操作
  const setDesired = async (value) => {
    if (value === 'stopped') {
      const ok = await confirmDialog(t('streams.danger.stopConfirmTitle'), t('streams.danger.stopConfirm', { name: stream.name }), {
        ok: t('streams.detail.stop'), icon: 'stop',
        consequences: [state() === 'live' ? t('streams.danger.stopC1') : null, t('streams.danger.stopC2'), t('streams.danger.stopC3')].filter(Boolean),
      });
      if (!ok) return;
    }
    try {
      const r = await streams.update(id, { desired_state: value }, etag);
      stream = r.data;
      etag = r.etag;
      renderAll();
      toast(value === 'stopped' ? t('streams.detail.stopped') : t('streams.detail.resumed'), 'ok');
    } catch (e) {
      if (!(await on412(e))) reportError(e, t('streams.detail.changeState'));
    }
  };

  const remove = async () => {
    const ok = await confirmDialog(t('streams.detail.deleteTitle'), t('streams.detail.deleteBody', { name: stream.name }), {
      ok: t('common.delete'), requireText: stream.name,
      consequences: [state() === 'live' ? t('streams.detail.deleteC1') : null, t('streams.detail.deleteC2'), t('streams.detail.deleteC4'), t('streams.detail.deleteC3')].filter(Boolean),
    });
    if (!ok) return;
    try {
      await streams.remove(id, etag);
      toast(t('streams.detail.deleted', { name: stream.name }), 'ok');
      forgetRecent(`#/streams/${encodeURIComponent(id)}`);
      invalidatePalette();
      navigate('/streams');
    } catch (e) {
      if (!(await on412(e))) reportError(e, t('common.delete'));
    }
  };

  const rotate = async () => {
    const ok = await confirmDialog(t('streams.detail.rotate'), t('streams.detail.rotateBody'), {
      ok: t('streams.detail.rotateOk'), icon: 'key',
      consequences: [state() === 'live' ? t('streams.danger.rotateC1') : null, t('streams.danger.rotateC2'), t('streams.danger.rotateC3')].filter(Boolean),
    });
    if (!ok) return;
    try {
      const r = await streams.rotateKey(id);
      stream = r.data;
      if (r.etag) etag = r.etag;
      secret = r.data.ingest.stream_key;
      renderAll();
      toast(t('streams.detail.rotated'), 'ok');
      selectTab('connect');
    } catch (e) {
      reportError(e, t('streams.detail.rotate'));
    }
  };

  const startRecording = async () => {
    try {
      const { data } = await streams.startRecording(id);
      toast(t('streams.detail.recStarted'), 'ok', { action: { label: t('streams.rec.open'), onClick: () => navigate(`/recordings/${encodeURIComponent(data.id)}`) } });
      await loadRecs();
    } catch (e) {
      reportError(e, t('streams.detail.startRecording'));
    }
  };

  const stopRecording = async () => {
    if (!activeRec) return;
    try {
      await recordings.stop(activeRec.id);
      toast(t('recordings.stopAccepted'), 'ok');
      setTimeout(loadRecs, 1500);
    } catch (e) {
      reportError(e, t('recordings.stopShort'));
    }
  };

  // 今の画を取って見せる（保存もできる）
  const grabFrame = async () => {
    const body = h('div', { class: 'grab-body' }, h('div', { class: 'player player-loading' }, icon('spinner', { spin: true })));
    let blob = null;
    const p = modal(t('streams.detail.snapshot'), () => body, {
      wide: true, icon: 'image', testid: 'grab-dialog',
      actions: (close) => [
        button(t('streams.grab.again'), { icon: 'refresh', onClick: () => fetchFrame() }),
        button(t('streams.grab.save'), { icon: 'download', testid: 'grab-save', onClick: () => { if (blob) downloadBlob(blob, `${stream.name}-${new Date().toISOString().replace(/[:.]/g, '-')}.jpg`); } }),
        button(t('common.close'), { variant: 'primary', onClick: () => close() }),
      ],
    });
    const fetchFrame = async () => {
      try {
        const { data } = await streams.snapshot(id);
        blob = data;
        const u = URL.createObjectURL(data);
        blobUrls.add(u);
        replace(body, h('img', { class: 'snapshot', src: u, alt: t('streams.detail.snapAlt') }), h('p', { class: 'small muted mt-2' }, t('streams.detail.snapTaken', { time: fmtClock(new Date()) })));
      } catch (e) {
        if (e.status === 401) return;
        let text = describeError(e);
        if (e.status === 409) text = t('streams.detail.snap409', { detail: e.title });
        else if (e.status === 503) text = t(e.retryable ? 'streams.detail.snap503retry' : 'streams.detail.snap503', { detail: e.title });
        else if (e.status === 422) text = t('streams.detail.snap422', { detail: e.title });
        replace(body, alert('warn', [text, e.requestId ? h('div', { class: 'meta' }, `${e.code || e.status} · request_id: ${e.requestId}`) : null], { testid: 'grab-error' }));
      }
    };
    fetchFrame();
    await p;
  };

  const loadPlan = (box) => load(box, async () => {
    const { data } = await streams.plan(id);
    const modeKind = (m) => ({ passthrough: 'ok', remux: 'ok', repacketize: 'ok', transcode: 'warn', unsupported: 'danger' }[m] || '');
    return h('div', null,
      h('p', { class: 'row' }, statusPill(data.state === 'ready' ? 'ready' : 'provisional', { label: data.state === 'ready' ? t('streams.detail.planReady') : t('streams.detail.planProvisional') }),
        data.reason ? h('span', { class: 'muted small' }, ` ${data.reason}`) : null),
      dataTable({
        caption: t('streams.detail.plan'),
        rows: data.outputs,
        empty: t('streams.detail.planNoOutputs'),
        columns: [
          { label: t('streams.detail.planOutput'), primary: true, render: (o) => o.output },
          { label: t('streams.detail.planTrack'), render: (o) => o.track },
          { label: t('streams.detail.planMode'), render: (o) => h('span', { class: `badge ${modeKind(o.mode)}` }, o.mode) },
          { label: t('streams.detail.planTo'), render: (o) => [o.container, o.to].filter(Boolean).join(' → ') || '—' },
          { label: t('streams.detail.planLatency'), class: 'num', render: (o) => (o.added_latency_ms !== undefined ? `${o.added_latency_ms} ms` : '—') },
          { label: t('streams.detail.planReason'), wrap: true, render: (o) => (o.code ? [h('code', null, o.code), ' ', o.reason || ''] : o.reason || '') },
        ],
      }));
  });

  // ---------------------------------------------------------------- 組み立て
  try {
    await fetchStream();
  } catch (e) {
    root.append(pageHeader({ title: t('nav.streams'), crumbs: [{ label: t('nav.streams'), href: '#/streams' }, { label: id.slice(0, 10) }] }),
      errorBox(e, { actions: [h('a', { class: 'btn sm', href: '#/streams' }, icon('arrow-left'), t('common.backToList'))] }));
    return null;
  }
  rememberRecent({ kind: 'stream', id, label: stream.name, href: `#/streams/${encodeURIComponent(id)}` });
  captions = captionsSection(() => stream, () => etag, (r) => { stream = r.data; etag = r.etag; captions.render(); });
  restream = restreamSection(() => stream, caps);
  // 送り手の方式の切り替え（接続タブ）
  const methodSeg = segmented({
    label: t('streams.guide.method'), testid: 'connect-method', value: connectMethod,
    options: [['rtmp', t('streams.guide.m.rtmp'), 'monitor'], ['srt', t('streams.guide.m.srt'), 'activity'], ['whip', t('streams.guide.m.whip'), 'camera'], ['pull', t('streams.guide.m.pull'), 'download']],
    onChange: (v) => { connectMethod = v; renderConnect(); },
  });
  const planBox = h('div');

  tabsEl = tabs({
    label: t('streams.tabs.label'),
    tabs: [
      { id: 'overview', label: t('streams.tabs.overview'), icon: 'activity', render: () => h('div', { class: 'ov-grid' },
        panel({ title: t('streams.detail.preview'), icon: 'play', class: 'ov-preview', actions: [grabBtn] }, previewBox),
        panel({ title: t('streams.health.title'), icon: 'gauge', class: 'ov-health', help: helpHint({ title: t('streams.health.title'), body: t('streams.health.help') }) }, healthBox)) },
      { id: 'connect', label: t('streams.tabs.connect'), icon: 'upload', render: () => {
        inputsBox = ioSection('input', id);
        return h('div', null,
          panel({ title: t('streams.connect.title'), icon: 'upload', description: t('streams.connect.desc'), help: helpHint({ title: t('streams.detail.streamKey'), body: t('streams.detail.streamKeyHelp'), guide: '04-ingest-playback.md' }) },
            connectKeyBox, h('div', { class: 'method-switch' }, methodSeg), connectGuideBox),
          panel({ title: t('streams.detail.inputs'), icon: 'download', description: t('streams.detail.inputsDesc') }, inputsBox.el));
      } },
      { id: 'restream', label: t('streams.tabs.restream'), icon: 'send-arrow', render: () => {
        restream.reload();
        return h('div', null,
          panel({ title: t('streams.detail.outputs'), icon: 'send-arrow', description: t('streams.detail.outputsDesc'), help: helpHint({ title: t('streams.detail.outputs'), body: t('streams.restream.help'), guide: '06-inputs-outputs.md' }) }, restream.el),
          h('details', { class: 'disclosure panel-disclosure', onToggle: (e) => { if (e.target.open && !planBox.dataset.loaded) loadPlan(planBox); } },
            h('summary', null, icon('cpu'), h('span', null, t('streams.detail.plan')), h('small', null, t('streams.detail.planIntro'))),
            planBox));
      } },
      { id: 'recordings', label: t('streams.tabs.recordings'), icon: 'recordings', render: () => {
        loadGallery();
        return h('div', null,
          h('div', { class: 'grid cols-2' },
            panel({ title: t('streams.rec.title'), icon: 'record', help: helpHint({ title: t('streams.rec.title'), body: t('streams.rec.help'), guide: '05-recording.md' }) }, recPanelBox),
            panel({ title: t('streams.detail.recentRecordings'), icon: 'recordings', actions: [h('a', { class: 'btn sm ghost', href: `#/recordings?stream=${encodeURIComponent(id)}` }, t('common.all'), icon('arrow-right'))] }, recListBox)),
          panel({ title: t('streams.detail.gallery'), icon: 'image', actions: [iconButton('refresh', t('common.refresh'), { size: 'sm', onClick: loadGallery })] }, galleryBox));
      } },
      { id: 'playback', label: t('streams.tabs.playback'), icon: 'link', render: () => {
        const pb = playbackSection(id);
        return h('div', { class: 'grid cols-2' },
          panel({ title: t('streams.detail.playbackUrls'), icon: 'link' }, pb.el),
          panel({ title: t('streams.detail.captions'), icon: 'captions' }, captions.el));
      } },
      { id: 'settings', label: t('streams.tabs.settings'), icon: 'settings', render: () => h('div', null,
        panel({ title: t('streams.settings.title'), icon: 'settings' }, settingsBox),
        panel({ title: t('streams.danger.title'), icon: 'warning', class: 'danger-zone', testid: 'danger-zone' }, dangerBox)) },
    ],
  });

  root.append(headBox, setupBox, tabsEl);
  renderAll();
  loadRecs();

  const offs = [
    refreshOn(['stream.'], reload, { filter: (ev) => ev.data && ev.data.resource_id === id }),
    refreshOn(['recording.'], loadRecs),
    refreshOn(['output.'], () => { if (restream && restream.el.isConnected) restream.reload(); }),
    refreshOn(['input.'], () => { if (inputsBox) inputsBox.reload(); if (setup === 'pull' || connectMethod === 'pull') loadPull(); }),
  ];
  // publish の開始・終了とビットレートは SSE のイベントにならないので読み直す（入力を待っている案内の間は短く）
  let last = Date.now();
  timers.push(setInterval(() => {
    if (document.visibilityState !== 'visible') return;
    const every = setup && state() !== 'live' ? 2000 : 5000;
    if (Date.now() - last < every - 100) {
      // 経過時間だけは毎秒進める
      const u = root.querySelector('[data-live="health-uptime"]');
      if (u && stream.live) u.textContent = fmtDuration(uptimeMs(stream));
      const hu = headBox.querySelector('[data-live="head-uptime"]');
      if (hu && stream.live) replace(hu, icon('clock', { size: 'sm' }), fmtDuration(uptimeMs(stream)));
      return;
    }
    last = Date.now();
    reload();
    if (setup === 'pull') loadPull();
  }, 1000));
  return () => {
    offs.forEach((off) => off());
    if (player) player.stop();
    timers.forEach(clearInterval);
    for (const u of blobUrls) URL.revokeObjectURL(u);
    secret = null;
  };
}
