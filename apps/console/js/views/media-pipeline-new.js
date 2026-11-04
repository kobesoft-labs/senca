// メディアパイプラインの作成（#/media-pipelines/new?room=ID&use=record|broadcast|export|captions）。
//
// 用途を選ぶ → 部屋と名前 → 用途に合った設定だけ → 右に「作られるもの・行き先」を入力に合わせて出す → 作成。
// 配信先の鍵は、秘密値から選ぶか、その場で入れて新しい秘密値として保存する（URL には入れない）。

import { h, replace, select } from '../lib/dom.js';
import { errorBox, reportError } from '../lib/ui.js';
import { rooms, mediaPipelines } from '../api/realtime.js';
import { secrets as secretsApi, listAll } from '../api/resources.js';
import { t, tn } from '../i18n/index.js';
import { button, pageHeader, panel, formField, segmented, alert, emptyState, helpHint } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { idemKey } from '../lib/format.js';
import { invalidatePalette } from '../palette.js';
import { MP_NAME_RE, USE_CASES, EGRESS_PRESETS, pipelineName, pipelineBody, pipelineProblems, pipelineProducts, splitList } from '../lib/realtime.js';
import { PROVIDERS, captionsForm } from './stream-io.js';
import { useCards, USE_ICON } from './media-pipelines.js';
import { disclosure } from './room-form.js';

const SIZES = [['640x360', '640×360'], ['1280x720', '1280×720'], ['1920x1080', '1920×1080']];
const VCODECS = [['av1', 'AV1'], ['vp9', 'VP9'], ['vp8', 'VP8'], ['h264', 'H.264'], ['h265', 'H.265']];

export async function render(root, _params, { navigate, caps }) {
  const params = new URLSearchParams(location.hash.split('?')[1] || '');
  let use = USE_CASES.includes(params.get('use')) ? params.get('use') : null;
  let roomList = [];
  let secretList = null;
  try { roomList = await listAll(rooms.list); } catch (e) { reportError(e, t('nav.rooms')); }
  try { secretList = (await secretsApi.list()).data.items.map((s) => [s.ref, s.ref]); } catch { secretList = null; }

  const header = pageHeader({
    title: t('mp.new.title'),
    description: t('mp.new.lead'),
    crumbs: [{ label: t('nav.media-pipelines'), href: '#/media-pipelines' }, { label: t('mp.new.crumb') }],
    help: helpHint({ title: t('nav.media-pipelines'), body: t('mp.help'), guide: '11-media-pipelines.md' }),
  });
  if (!roomList.length) {
    root.append(header, h('section', { class: 'panel' }, emptyState({
      icon: 'rooms', title: t('mp.new.noRoomsTitle'), body: t('mp.new.noRoomsBody'), testid: 'mp-new-norooms',
      actions: [h('a', { class: 'btn primary', href: '#/rooms?new=1' }, icon('plus'), t('rooms.create'))],
    })));
    return null;
  }

  // ---------------------------------------------------------------- 入力
  const want = params.get('room');
  const roomSel = select(roomList.map((r) => [r.id, r.id]), roomList.some((r) => r.id === want) ? want : roomList[0].id, { 'data-testid': 'mp-new-room' });
  const name = h('input', { type: 'text', maxlength: 40, spellcheck: 'false', autocomplete: 'off', 'data-testid': 'mp-new-name' });
  let nameTouched = false;
  name.addEventListener('input', () => { nameTouched = true; });
  const who = segmented({ options: [['all', t('mp.new.whoAll')], ['some', t('mp.new.whoSome')]], value: 'all', label: t('mp.new.who'), testid: 'mp-new-who' });
  const whoList = h('input', { type: 'text', spellcheck: 'false', placeholder: 'teacher, alice', 'data-testid': 'mp-new-who-list' });

  // 録画
  const recP = h('input', { type: 'checkbox', checked: true, 'data-testid': 'mp-new-rec-participants' });
  const recC = h('input', { type: 'checkbox', checked: false, 'data-testid': 'mp-new-rec-composite' });
  const retention = h('input', { type: 'number', min: 1, max: 36500, placeholder: t('mp.new.keepForever'), 'data-testid': 'mp-new-retention' });

  // 合成
  const layout = segmented({ options: [['grid', t('mp.layout.grid'), 'layout-grid'], ['speaker', t('mp.layout.speaker'), 'users']], value: 'grid', label: t('mp.new.layout') });
  const size = select(SIZES, '1280x720');
  const fps = select([['15', '15 fps'], ['24', '24 fps'], ['30', '30 fps'], ['60', '60 fps']], '30');
  const vcodec = select(VCODECS.map(([v, l]) => [v, v === 'h264' || v === 'h265' ? `${l}（${t('mp.new.needsGpu')}）` : v === 'av1' ? `${l}（${t('mp.new.recommended')}）` : l]), 'av1');
  const bitrate = h('input', { type: 'number', min: 100, max: 50000, value: 2500 });

  // 配信先
  const preset = segmented({ options: EGRESS_PRESETS.map((p) => [p.id, t(`mp.dest.${p.id}`)]), value: 'youtube', label: t('mp.new.dest'), testid: 'mp-new-dest' });
  const egressUrl = h('input', { type: 'text', spellcheck: 'false', autocomplete: 'off', 'data-testid': 'mp-new-url' });
  const provider = select(PROVIDERS, 'cloudflare_stream');
  const keySrc = segmented({ options: [['new', t('mp.new.keyNew')], ['secret', t('mp.new.keySecret')], ['none', t('mp.new.keyNone')]], value: 'new', label: t('mp.new.key'), testid: 'mp-new-keysrc' });
  const newKey = h('input', { type: 'password', autocomplete: 'new-password', spellcheck: 'false', 'data-testid': 'mp-new-key' });
  const secretSel = secretList ? select([['', t('mp.new.pickSecret')], ...secretList], '') : h('input', { type: 'text', placeholder: 'secret:name' });
  const alsoRecord = h('input', { type: 'checkbox', checked: false, 'data-testid': 'mp-new-also-record' });

  // 字幕
  const cc = captionsForm(null, secretList);
  const ccOn = cc.el.querySelector('input[type="checkbox"]');
  if (ccOn) { ccOn.checked = true; ccOn.dispatchEvent(new Event('change')); ccOn.closest('label').hidden = true; }

  const startStopped = h('input', { type: 'checkbox', checked: false });

  const f = {
    room: formField({ label: t('mp.col.room'), input: roomSel, required: true }),
    name: formField({ label: t('common.name'), input: name, required: true, hint: t('mp.new.nameHint') }),
    who: formField({ label: t('mp.new.who'), input: who, hint: t('mp.new.whoHint') }),
    whoList: formField({ label: t('mp.new.whoList'), input: whoList, hint: t('mp.new.whoListHint') }),
    retention: formField({ label: t('mp.new.retention'), input: retention, optional: true, hint: t('mp.new.retentionHint') }),
    url: formField({ label: t('mp.new.url'), input: egressUrl, required: true }),
    provider: formField({ label: t('mp.new.provider'), input: provider }),
    keySrc: formField({ label: t('mp.new.key'), input: keySrc }),
    newKey: formField({ label: t('mp.new.keyValue'), input: newKey, hint: t('mp.new.keyValueHint') }),
    secret: formField({ label: t('mp.new.secret'), input: secretSel, hint: [t('mp.new.secretHint'), ' ', h('a', { href: '#/secrets', target: '_blank' }, t('mp.new.manageSecrets'))] }),
    bitrate: formField({ label: t('mp.new.bitrate'), input: bitrate, hint: t('mp.new.bitrateHint') }),
  };

  const compositeBox = h('fieldset', { class: 'group' }, h('legend', null, t('mp.new.composite')),
    caps && caps.pipelines === false ? alert('warn', t('mp.new.noCodec'), { small: true }) : null,
    formField({ label: t('mp.new.layout'), input: layout, hint: t('mp.new.layoutHint') }),
    h('div', { class: 'cols' }, formField({ label: t('mp.new.size'), input: size }), formField({ label: t('mp.new.fps'), input: fps })),
    h('div', { class: 'cols' }, formField({ label: t('mp.new.codec'), input: vcodec, hint: t('mp.new.codecHint') }), f.bitrate));

  const destBox = h('fieldset', { class: 'group' }, h('legend', null, t('mp.new.destLegend')),
    formField({ label: t('mp.new.dest'), input: preset }),
    f.url, f.provider, f.keySrc, f.newKey, f.secret,
    h('label', { class: 'check' }, alsoRecord, t('mp.new.alsoRecord')));
  const recordBox = h('fieldset', { class: 'group' }, h('legend', null, t('mp.new.recordLegend')),
    h('label', { class: 'check' }, recP, h('span', null, t('mp.new.recParticipants'), h('small', { class: 'muted block' }, t('mp.new.recParticipantsHint')))),
    h('label', { class: 'check' }, recC, h('span', null, t('mp.new.recComposite'), h('small', { class: 'muted block' }, t('mp.new.recCompositeHint')))),
    f.retention);
  const exportBox = alert('info', t('mp.new.exportNote'), { small: true });
  const ccBox = h('fieldset', { class: 'group' }, h('legend', null, t('mp.new.ccLegend')), cc.el);
  const whoBox = h('div', { class: 'stack' }, f.who, f.whoList);

  const useBox = h('div');
  const configBox = h('div', { class: 'form' });
  const summary = h('div', { class: 'mp-summary', 'data-testid': 'mp-new-summary', 'aria-live': 'polite' });
  const errBox = h('div');
  const createBtn = button(t('mp.new.create'), { variant: 'primary', icon: 'check', testid: 'mp-new-create', onClick: () => create() });

  const currentRoom = () => roomList.find((r) => r.id === roomSel.value) || null;
  const hasComposite = () => use === 'broadcast' || (use === 'record' && recC.checked);
  const sizeWH = () => size.value.split('x').map(Number);

  // ---------------------------------------------------------------- 本文
  const values = () => {
    const [w, hh] = sizeWH();
    const v = {
      name: name.value.trim(), room: roomSel.value,
      participants: who.value === 'some' ? splitList(whoList.value) : [],
      composite: hasComposite() ? { layout: layout.value, width: w, height: hh, fps: Number(fps.value), video_codec: vcodec.value, bitrate_kbps: Number(bitrate.value) || 2500 } : null,
      recording: null, egress: [], captions: null,
      desired_state: startStopped.checked ? 'stopped' : 'running',
    };
    if (use === 'record') v.recording = { participants: recP.checked, composite: recC.checked, retention_days: retention.value ? Number(retention.value) : null };
    if (use === 'broadcast') {
      const p = EGRESS_PRESETS.find((x) => x.id === preset.value);
      const e = { protocol: p.protocol, url: p.protocol === 'paas' ? '' : egressUrl.value.trim() };
      if (p.protocol === 'paas') e.provider = provider.value;
      if (keySrc.value === 'secret' && secretSel.value) e.credential_ref = secretSel.value;
      if (keySrc.value === 'new' && newKey.value) e.credential_ref = 'secret:(new)';
      v.egress = [e];
      if (alsoRecord.checked) v.recording = { participants: false, composite: true, retention_days: null };
    }
    if (use === 'captions') {
      try { v.captions = cc.value(); } catch (e) { v.captions = {}; v.ccError = e.message; }
    }
    return v;
  };

  const localProblems = (v) => {
    const p = [];
    if (use === 'broadcast') {
      const e = v.egress[0];
      if (e.protocol !== 'paas' && !/^(rtmps?|srt):\/\/\S+$/.test(e.url)) p.push('egressUrl');
      if (e.protocol === 'paas' && !e.credential_ref) p.push('paasKey');
      if (keySrc.value === 'new' && !newKey.value && e.protocol !== 'paas') p.push('keyMissing');
      else if (keySrc.value === 'secret' && !e.credential_ref) p.push('secretMissing');
      else if (keySrc.value === 'none' && (preset.value === 'youtube' || preset.value === 'twitch')) p.push('keyMissing');
    }
    if (use === 'record' && !recP.checked && !recC.checked) p.push('recordNothing');
    if (who.value === 'some' && !v.participants.length) p.push('whoEmpty');
    if (v.ccError) p.push({ text: v.ccError });
    return p;
  };

  const productLine = (p) => {
    if (p.kind === 'participantStreams') {
      return [USE_ICON.export, p.who ? tn('mp.out.someStreams', p.who.length, { who: p.who.join(', ') }) : t('mp.out.streams'),
        p.artifact ? t('mp.out.streamsWhere') : t('mp.out.streamsInternal')];
    }
    if (p.kind === 'composite') return ['layout-grid', t('mp.out.composite', { w: p.width, h: p.height, codec: String(p.video_codec).toUpperCase(), layout: t(`mp.layout.${p.layout}`) }), t('mp.out.compositeWhere')];
    if (p.kind === 'recording') return ['record', t(p.target === 'composite' ? 'mp.out.recComposite' : 'mp.out.recParticipants'), p.days ? tn('mp.out.keepDays', p.days) : t('mp.out.keepDefault')];
    if (p.kind === 'egress') return ['send-arrow', p.protocol === 'paas' ? t('mp.out.paas', { provider: p.provider }) : t('mp.out.egress', { url: p.url || '…' }), t('mp.out.egressWhere')];
    if (!p.language) return ['captions', t('mp.tag.captions'), t('mp.out.captionsWhere')];
    return ['captions', t('mp.out.captions', { lang: p.language, targets: p.targets.length ? p.targets.join(', ') : t('mp.out.noTranslation') }), t('mp.out.captionsWhere')];
  };

  const renderSummary = () => {
    if (!use) {
      replace(summary, h('p', { class: 'muted small' }, t('mp.new.pickFirst')));
      createBtn.disabled = true;
      return;
    }
    const v = values();
    const b = pipelineBody(v);
    const probs = [...pipelineProblems(b, currentRoom()), ...localProblems(v)];
    replace(summary,
      h('ul', { class: 'product-list' }, pipelineProducts(b).map((p) => {
        const [ic, text, where] = productLine(p);
        return h('li', null, h('span', { class: 'use-icon sm' }, icon(ic)), h('span', null, h('span', { class: 'strong' }, text), h('span', { class: 'small muted block' }, where)));
      })),
      h('h3', { class: 'section-label' }, t('mp.new.howRuns')),
      h('p', { class: 'small' }, v.desired_state === 'stopped' ? t('mp.new.runsStopped') : t('mp.new.runs')),
      probs.length ? alert('warn', h('ul', { class: 'plain' }, probs.map((k) => h('li', null, typeof k === 'string' ? t(`mp.problem.${k}`) : k.text))), { small: true, title: t('mp.new.fixFirst'), testid: 'mp-new-problems' }) : null);
    createBtn.disabled = probs.length > 0;
  };

  const renderConfig = () => {
    if (!nameTouched || !name.value) name.value = pipelineName(roomSel.value, use || 'media');
    replace(useBox, useCards(null, { selected: use, onSelect: (u) => { use = u; history.replaceState(history.state, '', `${location.pathname}${location.search}#/media-pipelines/new?room=${encodeURIComponent(roomSel.value)}&use=${u}`); renderConfig(); } }));
    if (!use) { replace(configBox); renderSummary(); return; }
    const room = currentRoom();
    const e2ee = room && room.settings && room.settings.e2ee;
    replace(configBox,
      h('div', { class: 'cols' }, f.room, f.name),
      e2ee && (use === 'broadcast' || use === 'captions' || recC.checked) ? alert('warn', t('mp.new.e2ee'), { small: true }) : null,
      use === 'record' ? recordBox : null,
      use === 'broadcast' ? destBox : null,
      use === 'export' ? exportBox : null,
      use === 'captions' ? ccBox : null,
      hasComposite() ? compositeBox : null,
      disclosure(t('mp.new.more'), t('mp.new.moreDesc'), whoBox,
        h('label', { class: 'check' }, startStopped, t('mp.new.startStopped'))));
    syncDest();
    syncWho();
    renderSummary();
  };

  const syncDest = () => {
    const p = EGRESS_PRESETS.find((x) => x.id === preset.value);
    const fixed = p.url && (p.id === 'youtube' || p.id === 'twitch');
    if (fixed) egressUrl.value = p.url;
    else if (EGRESS_PRESETS.some((x) => x.url && x.url === egressUrl.value)) egressUrl.value = '';
    egressUrl.readOnly = !!fixed;
    egressUrl.placeholder = p.protocol === 'srt_push' ? 'srt://host:port' : 'rtmp://host/app';
    f.url.hidden = p.protocol === 'paas';
    f.provider.hidden = p.protocol !== 'paas';
    f.newKey.hidden = keySrc.value !== 'new';
    f.secret.hidden = keySrc.value !== 'secret';
    const keyLabel = f.keySrc.querySelector('label');
    if (keyLabel) keyLabel.textContent = p.protocol === 'paas' ? t('mp.new.keyPaas') : t('mp.new.key');
  };
  const syncWho = () => { f.whoList.hidden = who.value !== 'some'; };

  for (const el of [roomSel, name, whoList, retention, size, fps, vcodec, bitrate, egressUrl, provider, newKey, secretSel, startStopped]) {
    el.addEventListener('input', renderSummary);
    el.addEventListener('change', renderSummary);
  }
  cc.el.addEventListener('input', renderSummary);
  cc.el.addEventListener('change', renderSummary);
  roomSel.addEventListener('change', () => renderConfig());
  for (const el of [recC, alsoRecord]) el.addEventListener('change', () => renderConfig());
  recP.addEventListener('change', renderSummary);
  for (const seg of [preset, keySrc]) seg.addEventListener('click', () => { syncDest(); renderSummary(); });
  who.addEventListener('click', () => { syncWho(); renderSummary(); });
  layout.addEventListener('click', renderSummary);

  // ---------------------------------------------------------------- 作成
  const key = idemKey();
  const create = async () => {
    const v = values();
    if (!MP_NAME_RE.test(v.name)) { f.name.setError(t('mp.problem.name')); name.focus(); return; }
    f.name.setError(null);
    replace(errBox);
    try {
      if (use === 'captions') v.captions = cc.value();
    } catch (e) {
      replace(errBox, alert('error', e.message, { small: true }));
      return;
    }
    try {
      if (use === 'broadcast' && keySrc.value === 'new' && newKey.value) {
        const base = `${v.name}-key`.slice(0, 56);
        let ref = null;
        for (const n of [base, `${base}-${Math.random().toString(36).slice(2, 6)}`]) {
          try { ref = (await secretsApi.create({ name: n, value: newKey.value })).data.ref; break; } catch (e) { if (e.status !== 409) throw e; }
        }
        newKey.value = '';
        v.egress[0].credential_ref = ref;
      }
      const { data } = await mediaPipelines.create(pipelineBody(v), key);
      invalidatePalette();
      navigate(`/media-pipelines/${encodeURIComponent(data.id)}?created=1`);
    } catch (e) {
      replace(errBox, errorBox(e, { title: t('mp.new.failed') }));
      errBox.scrollIntoView({ block: 'nearest' });
    }
  };

  root.append(header,
    h('div', { class: 'mp-new-layout' },
      h('div', { class: 'mp-new-main' },
        panel({ title: t('mp.new.step1'), icon: 'lightbulb' }, useBox),
        panel({ title: t('mp.new.step2'), icon: 'settings', testid: 'mp-new-config' }, configBox)),
      h('aside', { class: 'mp-new-side', 'aria-label': t('mp.new.willMake') },
        panel({ title: t('mp.new.step3'), icon: 'check' }, summary, errBox,
          h('div', { class: 'actions mt-4' }, h('a', { class: 'btn', href: '#/media-pipelines' }, t('common.cancel')), createBtn)))));
  renderConfig();
  return () => { newKey.value = ''; };
}
