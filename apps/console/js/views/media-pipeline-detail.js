// メディアパイプラインの詳細: 今の状態をふつうの言葉で、作ったもの（参加者ごとのストリーム・合成・録画・配信先）へのリンク、
// 合成のパイプラインと配信先の出力の状態（配置できない理由・接続の誤り）、設定、開始・停止・削除。
// 実行の状態（status）はイベントにならないので、見えている間は 3 秒ごとに読む。

import { h, replace } from '../lib/dom.js';
import { errorBox, toast } from '../lib/ui.js';
import { mediaPipelines } from '../api/realtime.js';
import { streams, outputs } from '../api/resources.js';
import { pipelines } from '../api/pipelines.js';
import { fmtTime, fmtNumber } from '../lib/format.js';
import { refreshOn } from '../lib/live.js';
import { t, tn } from '../i18n/index.js';
import { button, pageHeader, panel, alert, kvList, emptyState, statusPill, helpHint } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { rememberRecent } from '../lib/recent.js';
import { pipelineState, pipelineUse } from '../lib/realtime.js';
import { mpStatePill, productTags, setMpState, deleteMp, USE_ICON } from './media-pipelines.js';
import { ioStatus } from './stream-io.js';
import { stateBadge } from './streams.js';

export async function render(root, [id], { navigate }) {
  let mp = null;
  let streamInfo = new Map(); // stream id → Stream | null
  let compositePl = null;
  let outs = [];
  const head = h('div');
  const nowBox = h('div', { 'data-testid': 'mp-now' });
  const outBox = h('div', { 'data-testid': 'mp-artifacts' });
  const cfgBox = h('div');

  const fetchMp = async () => { mp = (await mediaPipelines.get(id)).data; };
  const fetchChildren = async () => {
    const ids = [...mp.artifacts.participant_streams.map((s) => s.stream), ...(mp.artifacts.composite ? [mp.artifacts.composite.stream] : [])].slice(0, 30);
    const res = await Promise.all(ids.map((sid) => streams.get(sid).then((r) => [sid, r.data]).catch(() => [sid, null])));
    streamInfo = new Map(res);
    if (mp.artifacts.composite) {
      compositePl = await pipelines.get(mp.artifacts.composite.pipeline).then((r) => r.data).catch(() => null);
      outs = await Promise.all(mp.artifacts.composite.outputs.map((o) => outputs.get(o).then((r) => r.data).catch(() => null)));
    } else {
      compositePl = null;
      outs = [];
    }
  };

  const renderHead = () => {
    const running = mp.desired_state === 'running';
    replace(head, pageHeader({
      title: mp.name,
      badges: [mpStatePill(mp, 'mp-state'), mp.managed ? h('span', { class: 'tag', title: t('mp.managedTitle') }, t('mp.managed')) : null],
      description: h('span', { class: 'row' }, icon(USE_ICON[pipelineUse(mp)], { size: 'sm' }), h('span', null, t(`mp.use.${pipelineUse(mp)}`)), h('span', { class: 'subtle', 'aria-hidden': 'true' }, '·'),
        h('span', null, t('mp.detail.ofRoom')), h('a', { href: `#/rooms/${encodeURIComponent(mp.room)}`, class: 'mono' }, mp.room)),
      crumbs: [{ label: t('nav.media-pipelines'), href: '#/media-pipelines' }, { label: mp.name }],
      actions: [
        running
          ? button(t('mp.stop'), { icon: 'stop', testid: 'mp-stop', attrs: { disabled: mp.managed }, onClick: () => toggle('stopped') })
          : button(t('mp.start'), { variant: 'primary', icon: 'play', testid: 'mp-start', attrs: { disabled: mp.managed }, onClick: () => toggle('running') }),
        button(t('common.delete'), { variant: 'danger', icon: 'trash', testid: 'mp-delete', attrs: { disabled: mp.managed }, onClick: async () => { if (await deleteMp(mp)) navigate('/media-pipelines'); } }),
      ],
    }));
  };

  const toggle = async (desired) => {
    const r = await setMpState(mp, desired);
    if (r) { mp = r; renderAll(); }
  };

  // ---------------------------------------------------------------- 今の状態
  const renderNow = () => {
    const st = pipelineState(mp);
    const s = mp.status;
    const explain = {
      running: ['ok', t('mp.now.running')],
      idle: ['info', t('mp.now.idle')],
      waiting: ['info', t('mp.now.waiting')],
      stopped: ['info', t('mp.now.stopped')],
    }[st];
    const problems = [];
    if (compositePl && compositePl.placement_status && ['unschedulable'].includes(compositePl.placement_status.state)) {
      problems.push(t('mp.now.compositeUnschedulable', { reasons: (compositePl.placement_status.reasons || []).map((r) => r.reason).join(', ') || '—' }));
    }
    if (compositePl && compositePl.last_error) problems.push(t('mp.now.compositeFailed', { reason: compositePl.last_error }));
    for (const o of outs) if (o && o.status && ['error', 'backoff'].includes(o.status.state) && o.status.last_error) problems.push(t('mp.now.egressError', { url: o.url || o.provider || o.id, err: o.status.last_error || '—' }));
    replace(nowBox,
      alert(explain[0], explain[1], { small: true, testid: 'mp-now-text' }),
      problems.length ? alert('error', h('ul', { class: 'plain' }, problems.map((p) => h('li', null, p))), { title: t('mp.now.problems'), small: true }) : null,
      s ? kvList([
        [t('mp.now.exporting'), s.participants.length
          ? h('ul', { class: 'plain export-list' }, s.participants.map((p) => h('li', null,
            icon(p.video ? 'send' : 'mic', { size: 'sm', label: p.video ? t('rooms.kind.video') : t('rooms.kind.audio') }),
            h('span', { class: 'cell-stack' }, h('strong', null, p.participant),
              h('a', { href: `#/streams/${encodeURIComponent(p.stream)}`, class: 'small' }, (streamInfo.get(p.stream) || {}).name || p.stream.slice(0, 10))))))
          : t('mp.now.nobody')],
        mp.composite ? [t('mp.now.members'), s.composite_members === null ? t('mp.now.membersNone') : tn('mp.now.membersN', s.composite_members)] : null,
        mp.composite && mp.composite.layout === 'speaker' ? [t('mp.now.speaker'), s.active_speaker || '—'] : null,
      ]) : null);
  };

  // ---------------------------------------------------------------- 作ったもの
  const streamRow = (sid, label, sub) => {
    const s = streamInfo.get(sid);
    return h('li', { class: 'artifact' },
      h('span', { class: 'use-icon sm' }, icon('streams')),
      h('span', { class: 'artifact-main' },
        s ? h('a', { href: `#/streams/${encodeURIComponent(sid)}`, class: 'strong' }, s.name) : h('span', { class: 'mono' }, sid.slice(0, 12)),
        h('span', { class: 'small muted block' }, sub || label)),
      s ? stateBadge(s) : statusPill('pending', { label: t('mp.out.notYet') }),
      h('a', { class: 'btn sm ghost', href: `#/recordings?stream=${encodeURIComponent(sid)}` }, icon('recordings'), t('mp.out.recordings')));
  };
  const renderOut = () => {
    const a = mp.artifacts;
    const parts = a.participant_streams;
    replace(outBox,
      h('h3', { class: 'section-label' }, t('mp.out.participantStreams')),
      parts.length
        ? h('ul', { class: 'artifact-list' }, parts.map((p) => streamRow(p.stream, p.name, t('mp.out.participantOf', { who: p.participant || '?' }))))
        : emptyState({ compact: true, icon: 'users', title: t('mp.out.noneYet'), body: t('mp.out.noneYetBody') }),
      a.composite ? [
        h('h3', { class: 'section-label' }, t('mp.out.compositeTitle')),
        h('ul', { class: 'artifact-list' },
          streamRow(a.composite.stream, null, mp.composite ? t('mp.out.composite', { w: mp.composite.width, h: mp.composite.height, codec: mp.composite.video_codec.toUpperCase(), layout: t(`mp.layout.${mp.composite.layout}`) }) : null),
          h('li', { class: 'artifact' },
            h('span', { class: 'use-icon sm' }, icon('pipelines')),
            h('span', { class: 'artifact-main' },
              compositePl ? h('a', { href: `#/pipelines?id=${encodeURIComponent(a.composite.pipeline)}`, class: 'strong' }, compositePl.name) : h('span', { class: 'mono' }, a.composite.pipeline.slice(0, 12)),
              h('span', { class: 'small muted block' }, t('mp.out.compositePipeline'))),
            compositePl ? statusPill(compositePl.observed_state === 'error' ? 'error' : compositePl.observed_state || compositePl.desired_state || 'unknown') : statusPill('pending', { label: t('mp.out.notYet') })),
          outs.map((o, i) => h('li', { class: 'artifact' },
            h('span', { class: 'use-icon sm' }, icon('send-arrow')),
            h('span', { class: 'artifact-main' },
              h('span', { class: 'strong break' }, o ? o.url || o.provider : (mp.egress[i] || {}).url || '—'),
              h('span', { class: 'small muted block' }, t('mp.out.egressTo'))),
            o ? ioStatus(o.status, o.enabled) : statusPill('pending', { label: t('mp.out.notYet') })))),
      ] : null);
  };

  const renderCfg = () => {
    replace(cfgBox, productTags(mp), kvList([
      ['ID', h('code', null, mp.id)],
      [t('mp.new.who'), mp.participants && mp.participants.length ? mp.participants.join(', ') : t('mp.new.whoAll')],
      [t('mp.cfg.export'), mp.export_participants ? t('common.yes') : t('mp.cfg.internalOnly')],
      mp.composite ? [t('mp.new.composite'), `${t(`mp.layout.${mp.composite.layout}`)} · ${mp.composite.width}×${mp.composite.height} · ${mp.composite.fps} fps · ${mp.composite.video_codec.toUpperCase()} · ${fmtNumber(mp.composite.bitrate_kbps)} kbps`] : null,
      mp.recording ? [t('mp.new.recordLegend'), [mp.recording.participants ? t('mp.tag.recParticipants') : null, mp.recording.composite ? t('mp.tag.recComposite') : null].filter(Boolean).join(' · ') + (mp.recording.retention_days ? ` · ${tn('mp.out.keepDays', mp.recording.retention_days)}` : '')] : null,
      (mp.egress || []).length ? [t('mp.new.destLegend'), h('ul', { class: 'plain' }, mp.egress.map((e) => h('li', null, h('code', { class: 'break' }, e.url || e.provider), e.credential_ref ? h('span', { class: 'small muted' }, ` · ${e.credential_ref}`) : null)))] : null,
      mp.captions ? [t('mp.new.ccLegend'), `${mp.captions.asr_provider} · ${mp.captions.language || '—'}${mp.captions.targets && mp.captions.targets.length ? ` → ${mp.captions.targets.join(', ')}` : ''}`] : null,
      [t('common.created'), fmtTime(mp.created_at)],
      [t('common.updated'), t('rooms.info.updatedGen', { time: fmtTime(mp.updated_at), gen: mp.generation })],
    ]));
  };

  const renderAll = () => { renderHead(); renderNow(); renderOut(); renderCfg(); };

  try {
    await fetchMp();
  } catch (e) {
    root.append(pageHeader({ title: t('nav.media-pipelines'), crumbs: [{ label: t('nav.media-pipelines'), href: '#/media-pipelines' }, { label: id.slice(0, 10) }] }),
      errorBox(e, { actions: [h('a', { class: 'btn sm', href: '#/media-pipelines' }, icon('arrow-left'), t('common.backToList'))] }));
    return null;
  }
  await fetchChildren();
  rememberRecent({ kind: 'media-pipeline', id, label: mp.name, href: `#/media-pipelines/${encodeURIComponent(id)}` });
  const created = new URLSearchParams(location.hash.split('?')[1] || '').get('created') === '1';
  if (created) history.replaceState(history.state, '', `${location.pathname}${location.search}#/media-pipelines/${encodeURIComponent(id)}`);
  root.append(...[head,
    created ? alert('ok', t('mp.detail.createdBody'), { title: t('mp.detail.created'), testid: 'mp-created' }) : null,
    mp.managed ? alert('info', [t('mp.detail.managedBody'), ' ', h('a', { href: `#/rooms/${encodeURIComponent(mp.room)}` }, t('mp.detail.openRoomSettings'))]) : null,
    h('div', { class: 'grid cols-2' },
      panel({ title: t('mp.detail.now'), icon: 'activity', testid: 'mp-now-panel', help: helpHint({ title: t('mp.detail.now'), body: t('mp.detail.nowHelp'), guide: '11-media-pipelines.md' }) }, nowBox),
      panel({ title: t('mp.detail.settings'), icon: 'settings' }, cfgBox)),
    panel({ title: t('mp.detail.artifacts'), icon: 'inbox', description: t('mp.detail.artifactsDesc') }, outBox)].filter(Boolean));
  renderAll();

  let busy = false;
  const reload = async () => {
    if (busy) return;
    busy = true;
    try {
      const before = JSON.stringify(mp);
      await fetchMp();
      await fetchChildren();
      if (JSON.stringify(mp) !== before) renderHead();
      renderNow();
      renderOut();
      renderCfg();
    } catch (e) {
      if (e.status === 404) { toast(t('mp.detail.gone'), 'warn'); navigate('/media-pipelines'); }
    } finally {
      busy = false;
    }
  };
  const offs = [refreshOn(['media_pipeline.', 'stream.', 'pipeline.', 'output.'], reload)];
  const timer = setInterval(() => { if (document.visibilityState === 'visible') reload(); }, 3000);
  return () => { offs.forEach((off) => off()); clearInterval(timer); };
}
