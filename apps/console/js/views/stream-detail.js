// 配信の詳細: 状態・取り込み先・プレビュー・今の画・保存した静止画・変換計画・編集・削除・鍵の作り直し。

import { h, replace, table, kv } from '../lib/dom.js';
import { load, modal, confirmDialog, reportError, errorBox, toast, copyField, describeError } from '../lib/ui.js';
import { streams, recordings } from '../api/resources.js';
import { fmtTime, fmtRelative, fmtBps, fmtBytes, fmtDuration } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { streamForm } from './stream-form.js';
import { stateBadge } from './streams.js';
import { recBadge } from './recordings.js';
import { takeSecret } from '../lib/secrets.js';
import { playerWidget } from '../player/widget.js';
import { ioSection, captionsSection, playbackSection } from './stream-io.js';
import { t } from '../i18n/index.js';
import { button, pageHeader, panel, alert, statusPill, emptyState, helpHint } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { rememberRecent, forgetRecent } from '../lib/recent.js';
import { fmtClock } from '../lib/format.js';
import { invalidatePalette } from '../palette.js';

export async function render(root, [id], { caps, navigate }) {
  let stream = null;
  let etag = null;
  let secret = takeSecret(id);
  let captions = null;
  const blobUrls = new Set();
  const timers = [];

  const head = h('div');
  const overview = h('div');
  const ingest = h('div');
  const snapNow = h('div');
  const gallery = h('div');
  const planBox = h('div', { class: 'muted small' }, t('streams.detail.planIntro'));
  const recBox = h('div');

  const player = playerWidget({ getPlayback: async () => (await streams.playbackUrl(id)).data.playback, live: true, label: t('streams.detail.previewLabel') });

  const blobUrl = (blob) => {
    const u = URL.createObjectURL(blob);
    blobUrls.add(u);
    return u;
  };

  const fetchStream = async () => {
    const r = await streams.get(id);
    stream = r.data;
    etag = r.etag;
    return stream;
  };

  const renderHead = () => {
    const toggle = stream.desired_state === 'running'
      ? button(t('streams.detail.stop'), { icon: 'stop', onClick: () => setDesired('stopped') })
      : button(t('streams.detail.resume'), { icon: 'play', onClick: () => setDesired('running') });
    replace(head, pageHeader({
      title: stream.name,
      badges: [stateBadge(stream)],
      crumbs: [{ label: t('nav.streams'), href: '#/streams' }, { label: stream.name }],
      actions: [
        button(t('streams.detail.startRecording'), { icon: 'record', attrs: { disabled: stream.observed_state !== 'live' && stream.desired_state === 'stopped' }, onClick: () => startRecording() }),
        toggle,
        button(t('common.edit'), { icon: 'edit', onClick: () => edit() }),
        button(t('common.delete'), { variant: 'danger', icon: 'trash', onClick: () => remove() }),
      ],
    }));
  };

  const renderOverview = () => {
    const s = stream;
    const live = s.live;
    replace(overview,
      kv([
        ['ID', h('code', null, s.id)],
        [t('common.state'), h('span', { class: 'row' }, stateBadge(s), h('span', { class: 'muted small' }, `desired: ${s.desired_state} / observed: ${s.observed_state}`))],
        [t('streams.detail.latencyCodec'), `${s.latency_profile} / ${s.codec_profile}`],
        ['HLS', t('streams.detail.hlsValue', { container: s.hls.container, ms: s.hls.segment_ms, n: s.hls.window })],
        [t('streams.col.recording'), s.recording.enabled ? t('streams.detail.recAuto', { keep: s.recording.retention_days ? t('streams.detail.days', { n: s.recording.retention_days }) : t('streams.detail.forever') }) : t('streams.detail.recManual')],
        [t('streams.form.snapshotsLegend'), s.snapshots ? t('streams.detail.snapValue', { s: s.snapshots.interval_secs, n: s.snapshots.keep }) : t('common.no')],
        [t('streams.form.limits'), t('streams.detail.limitsValue', { viewers: s.limits.max_viewers ?? '—', ingress: s.limits.max_ingress_bps ? fmtBps(s.limits.max_ingress_bps) : '—' })],
        [t('streams.detail.createdUpdated'), t('streams.detail.createdUpdatedValue', { created: fmtTime(s.created_at), updated: fmtTime(s.updated_at), gen: s.generation })],
        Object.keys(s.metadata).length ? [t('streams.form.metadata'), Object.entries(s.metadata).map(([k, v]) => h('div', null, h('code', null, k), ' = ', v))] : null,
      ]),
      live ? h('div', null,
        h('h3', null, t('streams.detail.liveInput')),
        kv([
          [t('streams.detail.transport'), live.transport],
          [t('streams.detail.started'), `${fmtTime(live.started_at)} (${fmtRelative(live.started_at)})`],
          ['epoch', String(live.epoch)],
          [t('streams.col.bitrate'), fmtBps(live.bitrate_bps)],
          [t('streams.detail.viewers'), String(live.viewers_hint)],
        ]),
        h('h3', null, t('streams.detail.tracks')),
        table([
          { label: '#', render: (tr) => String(tr.track ?? '') },
          { label: t('streams.col.codec'), render: (tr) => h('code', null, tr.codec || '?') },
          { label: t('common.details'), render: (tr) => Object.entries(tr).filter(([k]) => !['track', 'codec'].includes(k)).map(([k, v]) => `${k}=${v}`).join(' ') },
        ], live.tracks, { empty: t('streams.detail.noTracks') }))
        : h('p', { class: 'muted' }, t('streams.detail.noInput')));
  };

  const renderIngest = () => {
    const ig = stream.ingest;
    const key = secret;
    const item = (label, value, hint) => h('div', { class: 'field' }, h('div', { class: 'label' }, label), copyField(value, label), hint ? h('div', { class: 'hint' }, hint) : null);
    replace(ingest,
      h('div', { class: 'form' },
        item(t('streams.detail.rtmpUrl'), ig.rtmp_url, t('streams.detail.rtmpHint')),
        ig.srt_url ? item(t('streams.detail.srtUrl'), ig.srt_url, [t('streams.detail.srtHint'), ' ', h('code', null, ig.srt_streamid_format || 'publish:{stream_key}')]) : null,
        ig.whip_url ? item(t('streams.detail.whipUrl'), ig.whip_url,
          h('a', { href: `#/send?stream=${encodeURIComponent(stream.id)}`, class: 'row' }, icon('camera', { size: 'sm' }), t('streams.detail.sendFromBrowser'), icon('arrow-right', { size: 'sm' }))) : null,
        h('div', { class: 'field' },
          h('div', { class: 'field-label-row' }, h('span', { class: 'label' }, t('streams.detail.streamKey')),
            helpHint({ title: t('streams.detail.streamKey'), body: t('streams.detail.streamKeyHelp'), guide: '04-ingest-playback.md' })),
          key
            ? [alert('warn', t('streams.detail.keyOnce'), { role: 'alert' }),
              copyField(key, t('streams.detail.streamKey'), { secret: true }),
              ig.rtmp_url ? h('div', { class: 'stack mt-2' }, h('div', { class: 'hint' }, t('streams.detail.fullTarget')), copyField(`${ig.rtmp_url}/${key}`, t('streams.detail.fullTargetLabel'), { secret: true })) : null]
            : h('div', { class: 'muted small' }, t('streams.detail.keyHidden')),
          h('div', { class: 'mt-2' }, button(t('streams.detail.rotate'), { size: 'sm', icon: 'refresh', onClick: () => rotate() })))));
  };

  const renderAll = () => {
    renderHead();
    renderOverview();
    renderIngest();
    if (captions) captions.render();
  };

  const reload = async () => {
    try {
      const before = stream ? JSON.stringify(stream) : '';
      const ingestBefore = stream ? JSON.stringify(stream.ingest) : '';
      await fetchStream();
      if (JSON.stringify(stream) === before) return;
      renderHead();
      renderOverview();
      if (captions) captions.render();
      // 取り込み先は変わったときだけ描き直す（写している途中のフォーカスを奪わない）
      if (JSON.stringify(stream.ingest) !== ingestBefore) renderIngest();
    } catch (e) {
      if (e.status === 404) {
        toast(t('streams.detail.gone'), 'warn');
        navigate('/streams');
      } else reportError(e, t('streams.detail.reload'));
    }
  };

  const on412 = async (e) => {
    if (e.status === 412) {
      await reload();
      toast(t('streams.detail.stale'), 'warn', 8000);
      return true;
    }
    return false;
  };

  const setDesired = async (value) => {
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

  const edit = async () => {
    const form = streamForm(stream, caps);
    const errBox = h('div');
    const updated = await modal(t('streams.detail.editTitle', { name: stream.name }), () => [form.el, errBox], {
      icon: 'edit',
      wide: true,
      actions: (close) => {
        const ok = h('button', { type: 'button', class: 'primary', onClick: async () => {
          const body = form.value();
          if (!body) return;
          if (!Object.keys(body).length) { close(null); return; }
          ok.disabled = true;
          try {
            const r = await streams.update(id, body, etag);
            etag = r.etag;
            close(r.data);
          } catch (e) {
            if (e.status === 412) {
              await reload();
              close(null);
              toast(t('streams.detail.staleEdit'), 'warn', 9000);
              return;
            }
            replace(errBox, errorBox(e, { title: t('streams.detail.saveFailed') }));
          } finally {
            ok.disabled = false;
          }
        } }, t('common.save'));
        return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
      },
    });
    if (updated) {
      stream = updated;
      renderAll();
      toast(t('common.saved'), 'ok');
    }
  };

  const remove = async () => {
    const ok = await confirmDialog(t('streams.detail.deleteTitle'), t('streams.detail.deleteBody', { name: stream.name }), {
      ok: t('common.delete'), requireText: stream.name,
      consequences: [t('streams.detail.deleteC1'), t('streams.detail.deleteC2'), t('streams.detail.deleteC3')],
    });
    if (!ok) return;
    try {
      await streams.remove(id, etag);
      toast(t('common.deleted'), 'ok');
      forgetRecent(`#/streams/${encodeURIComponent(id)}`);
      invalidatePalette();
      navigate('/streams');
    } catch (e) {
      if (!(await on412(e))) reportError(e, t('common.delete'));
    }
  };

  const rotate = async () => {
    const ok = await confirmDialog(t('streams.detail.rotate'), t('streams.detail.rotateBody'), { ok: t('streams.detail.rotateOk'), icon: 'key' });
    if (!ok) return;
    try {
      const r = await streams.rotateKey(id);
      stream = r.data;
      if (r.etag) etag = r.etag;
      secret = r.data.ingest.stream_key;
      renderAll();
      toast(t('streams.detail.rotated'), 'ok');
    } catch (e) {
      reportError(e, t('streams.detail.rotate'));
    }
  };

  const startRecording = async () => {
    try {
      const { data } = await streams.startRecording(id);
      toast(t('streams.detail.recStarted'), 'ok');
      navigate(`/recordings/${encodeURIComponent(data.id)}`);
    } catch (e) {
      reportError(e, t('streams.detail.startRecording'));
    }
  };

  // 今の画
  const auto = h('input', { type: 'checkbox' });
  let autoTimer = null;
  let lastSnap = null;
  const snapImg = h('img', { class: 'snapshot', alt: t('streams.detail.snapAlt'), hidden: true });
  const snapMsg = h('div', { role: 'status', class: 'snap-msg' });
  const grab = async () => {
    try {
      const { data } = await streams.snapshot(id);
      const u = blobUrl(data);
      if (lastSnap) { URL.revokeObjectURL(lastSnap); blobUrls.delete(lastSnap); }
      lastSnap = u;
      snapImg.src = u;
      snapImg.hidden = false;
      replace(snapMsg, h('span', { class: 'muted small' }, t('streams.detail.snapTaken', { time: fmtClock(new Date()) })));
    } catch (e) {
      if (e.status === 401) return;
      let text = describeError(e);
      if (e.status === 409) text = t('streams.detail.snap409', { detail: e.title });
      else if (e.status === 503) text = t(e.retryable ? 'streams.detail.snap503retry' : 'streams.detail.snap503', { detail: e.title });
      else if (e.status === 422) text = t('streams.detail.snap422', { detail: e.title });
      replace(snapMsg, alert('warn', [text, e.requestId ? h('div', { class: 'meta' }, `${e.code || e.status} · request_id: ${e.requestId}`) : null]));
    }
  };
  auto.addEventListener('change', () => {
    clearInterval(autoTimer);
    if (auto.checked) { grab(); autoTimer = setInterval(grab, 5000); }
  });
  replace(snapNow,
    h('div', { class: 'row' }, button(t('streams.detail.grab'), { icon: 'camera', onClick: grab }), h('label', { class: 'check' }, auto, t('streams.detail.autoGrab'))),
    h('div', { style: { marginTop: '8px' } }, snapImg),
    snapMsg);

  const loadGallery = () => load(gallery, async () => {
    const { data } = await streams.snapshots(id);
    const items = data.items.slice(-24).reverse();
    if (!items.length) return emptyState({ compact: true, icon: 'image', title: t('streams.detail.noSnapshots'), body: t('streams.detail.noSnapshotsBody') });
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
      streams.snapshotImage(id, it.id).then(({ data: b }) => { img.src = blobUrl(b); }).catch(() => { img.alt = t('streams.detail.imgFailed'); });
    }
    return h('div', null, h('p', { class: 'small muted' }, t('streams.detail.snapCount', { n: data.items.length })), grid);
  });

  const loadPlan = () => load(planBox, async () => {
    const { data } = await streams.plan(id);
    const modeKind = (m) => ({ passthrough: 'ok', remux: 'ok', repacketize: 'ok', transcode: 'warn', unsupported: 'danger' }[m] || '');
    return h('div', null,
      h('p', { class: 'row' }, t('common.state'), statusPill(data.state === 'ready' ? 'ready' : 'provisional', { label: data.state === 'ready' ? t('streams.detail.planReady') : t('streams.detail.planProvisional') }),
        data.reason ? h('span', { class: 'muted small' }, ` ${data.reason}`) : null),
      table([
        { label: t('streams.detail.planOutput'), render: (o) => o.output },
        { label: t('streams.detail.planTrack'), render: (o) => o.track },
        { label: t('streams.detail.planMode'), render: (o) => h('span', { class: `badge ${modeKind(o.mode)}` }, o.mode) },
        { label: t('streams.detail.planTo'), render: (o) => [o.container, o.to].filter(Boolean).join(' → ') || '—' },
        { label: t('streams.detail.planLatency'), class: 'num', render: (o) => (o.added_latency_ms !== undefined ? `${o.added_latency_ms} ms` : '—') },
        { label: t('streams.detail.planReason'), render: (o) => (o.code ? [h('code', null, o.code), ' ', o.reason || ''] : o.reason || '') },
      ], data.outputs, { empty: t('streams.detail.planNoOutputs') }));
  });

  const loadRecs = () => load(recBox, async () => {
    const { data } = await recordings.list({ stream: id, limit: 1000 });
    const items = data.items.sort((a, b) => b.started_at.localeCompare(a.started_at)).slice(0, 10);
    return table([
      { label: t('streams.detail.started'), render: (r) => h('a', { href: `#/recordings/${encodeURIComponent(r.id)}` }, fmtTime(r.started_at)) },
      { label: t('common.state'), render: recBadge },
      { label: t('streams.detail.duration'), class: 'num', render: (r) => fmtDuration(r.duration_ms) },
      { label: t('streams.detail.size'), class: 'num', render: (r) => fmtBytes(r.bytes) },
    ], items, { empty: t('streams.detail.noRecordings') });
  });

  try {
    await fetchStream();
  } catch (e) {
    root.append(pageHeader({ title: t('nav.streams'), crumbs: [{ label: t('nav.streams'), href: '#/streams' }, { label: id.slice(0, 10) }] }), errorBox(e, { actions: [h('a', { class: 'btn sm', href: '#/streams' }, icon('arrow-left'), t('common.backToList'))] }));
    return null;
  }

  const container = stream.hls.container;
  const inputsBox = ioSection('input', id);
  const outputsBox = ioSection('output', id);
  captions = captionsSection(() => stream, () => etag, (r) => { stream = r.data; etag = r.etag; renderAll(); });
  const playback = playbackSection(id);
  rememberRecent({ kind: 'stream', id, label: stream.name, href: `#/streams/${encodeURIComponent(id)}` });
  root.append(
    head,
    h('div', { class: 'grid cols-2' },
      panel({ title: t('streams.detail.overview'), icon: 'info' }, overview),
      panel({ title: t('streams.detail.ingest'), icon: 'upload', description: t('streams.detail.ingestDesc') }, ingest)),
    h('div', { class: 'grid cols-2' },
      panel({ title: t('streams.detail.preview'), icon: 'play' },
        container === 'ts'
          ? alert('info', t('streams.detail.tsNote'), { small: true })
          : h('p', { class: 'small muted' }, t('streams.detail.autoNote')),
        player.el),
      panel({ title: t('streams.detail.snapshot'), icon: 'camera' }, snapNow)),
    h('div', { class: 'grid cols-2' },
      panel({ title: t('streams.detail.playbackUrls'), icon: 'link' }, playback.el),
      panel({ title: t('streams.detail.captions'), icon: 'captions' }, captions.el)),
    panel({ title: t('streams.detail.inputs'), icon: 'download', description: t('streams.detail.inputsDesc') }, inputsBox.el),
    panel({ title: t('streams.detail.outputs'), icon: 'send-arrow', description: [t('streams.detail.outputsDesc'), ' ', h('a', { href: '#/secrets' }, t('streams.detail.manageSecrets'))] }, outputsBox.el),
    panel({ title: t('streams.detail.plan'), icon: 'cpu', actions: [button(t('streams.detail.getPlan'), { size: 'sm', onClick: loadPlan })] }, planBox),
    panel({ title: t('streams.detail.gallery'), icon: 'image', actions: [button(t('common.refresh'), { size: 'sm', icon: 'refresh', onClick: loadGallery })] }, gallery),
    panel({ title: t('streams.detail.recentRecordings'), icon: 'recordings', actions: [h('a', { class: 'btn sm ghost', href: `#/recordings?stream=${encodeURIComponent(id)}` }, t('common.all'), icon('arrow-right'))] }, recBox));
  renderAll();
  loadGallery();
  loadRecs();

  const offs = [
    refreshOn(['stream.'], reload, { filter: (ev) => ev.data && ev.data.resource_id === id }),
    refreshOn(['recording.'], () => { loadRecs(); reload(); }),
  ];
  // publish の開始・終了とビットレートは SSE のイベントにならないので、見えている間は 5 秒ごとに読む
  timers.push(setInterval(() => {
    if (document.visibilityState === 'visible') reload();
  }, 5000));
  return () => {
    offs.forEach((off) => off());
    player.stop();
    clearInterval(autoTimer);
    timers.forEach(clearInterval);
    for (const u of blobUrls) URL.revokeObjectURL(u);
    secret = null;
  };
}
