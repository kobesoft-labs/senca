// 部屋の設定のフォーム（作成と編集で同じもの）。よく使う項目だけを先に出し、残りは「詳しい設定」に畳む。
//
// value() は作成なら {id?, settings, metadata}、編集なら PATCH の本文（変えた項目だけ。settings は JSON Merge Patch）。
// 入力に誤りがあれば欄に理由を出して null を返す。

import { h, select } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { formField, segmented, helpHint, alert } from '../ui/index.js';
import { icon } from '../lib/icons.js';
import { ROOM_ID_RE, ROOM_DEFAULTS, settingsPatch, splitList } from '../lib/realtime.js';

const CODECS = [['vp8', 'VP8'], ['vp9', 'VP9'], ['h264', 'H.264'], ['av1', 'AV1']];

function num(value, attrs) {
  return h('input', { type: 'number', inputmode: 'numeric', value: value ?? '', ...attrs });
}

/** 折りたたみの節（<details>）。開いているかはこのフォームの中だけで覚える。 */
export function disclosure(title, desc, ...children) {
  return h('details', { class: 'disclosure' },
    h('summary', null, icon('chevron-right', { class: 'disclosure-chevron' }), h('span', { class: 'disclosure-title' }, title), desc ? h('span', { class: 'disclosure-desc' }, desc) : null),
    h('div', { class: 'disclosure-body form' }, children));
}

/**
 * @param {object|null} room 編集する部屋（null で作成）
 * @param {{hasParticipants?: boolean}} [o] 参加者がいる間は e2ee・コーデックを変えられない
 */
export function roomForm(room = null, { hasParticipants = false } = {}) {
  const s = room ? room.settings : ROOM_DEFAULTS;
  const meta = room && room.metadata && typeof room.metadata === 'object' && !Array.isArray(room.metadata) ? room.metadata : {};
  const { title: curTitle, ...metaRest } = meta;
  const locked = !!room && hasParticipants;

  const id = h('input', { type: 'text', maxlength: 128, autocomplete: 'off', spellcheck: 'false', placeholder: t('rooms.form.idPlaceholder'), 'data-testid': 'room-id' });
  const title = h('input', { type: 'text', maxlength: 200, value: typeof curTitle === 'string' ? curTitle : '', placeholder: t('rooms.form.titlePlaceholder'), 'data-testid': 'room-title' });
  const max = num(s.max_participants, { min: 1, max: 10000, 'data-testid': 'room-max' });
  const codec = segmented({ options: CODECS, value: s.video_codec || 'vp8', label: t('rooms.form.codec'), testid: 'room-codec' });
  if (locked) codec.querySelectorAll('button').forEach((b) => { b.disabled = true; });
  const recording = select([
    ['none', t('rooms.form.recNone')], ['per_participant', t('rooms.form.recPer')], ['composite', t('rooms.form.recComposite')],
  ], s.recording || 'none', { 'data-testid': 'room-recording' });
  const e2ee = h('input', { type: 'checkbox', checked: !!s.e2ee, disabled: locked, 'data-testid': 'room-e2ee' });

  // 詳しい設定
  const bitrate = num(s.max_video_bitrate_kbps, { min: 50, max: 50000, placeholder: t('rooms.form.noLimit') });
  const spotOn = h('input', { type: 'checkbox', checked: !!s.spotlight });
  const spotNum = num(s.spotlight ? s.spotlight.number : 2, { min: 1, max: 16 });
  const spotUnfocused = select([['lowest', t('rooms.form.unfocusedLowest')], ['off', t('rooms.form.unfocusedOff')]], s.spotlight ? s.spotlight.unfocused || 'lowest' : 'lowest');
  const cascade = h('input', { type: 'checkbox', checked: !!s.cascade });
  const filter = h('textarea', { rows: 4, spellcheck: 'false', placeholder: '{"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]}]]}' },
    s.forwarding_filter ? JSON.stringify(s.forwarding_filter, null, 2) : '');
  const labels = h('input', { type: 'text', value: (s.data_labels || ['#*']).join(', '), spellcheck: 'false' });
  const emptyTimeout = num(s.empty_timeout_secs ?? 60, { min: 0 });
  const webhook = h('input', { type: 'url', value: s.auth_webhook_url || '', placeholder: 'http(s)://app.example/senca/auth', spellcheck: 'false' });
  const metaText = h('textarea', { rows: 3, spellcheck: 'false', placeholder: '{"team":"sales"}' }, Object.keys(metaRest).length ? JSON.stringify(metaRest, null, 2) : '');

  const f = {
    id: formField({ label: t('rooms.form.id'), input: id, optional: true, hint: t('rooms.form.idHint') }),
    title: formField({ label: t('rooms.form.title'), input: title, optional: true, hint: t('rooms.form.titleHint') }),
    max: formField({ label: t('rooms.form.max'), input: max, hint: t('rooms.form.maxHint') }),
    codec: formField({ label: t('rooms.form.codec'), input: codec, hint: locked ? t('rooms.form.lockedHint') : t('rooms.form.codecHint') }),
    recording: formField({ label: t('rooms.form.recording'), input: recording, hint: t('rooms.form.recordingHint'),
      help: helpHint({ title: t('rooms.form.recording'), body: t('rooms.form.recordingHelp'), guide: '10-rooms.md#部屋の録画' }) }),
    bitrate: formField({ label: t('rooms.form.bitrate'), input: bitrate, optional: true, hint: t('rooms.form.bitrateHint') }),
    spotNum: formField({ label: t('rooms.form.spotNumber'), input: spotNum }),
    spotUnfocused: formField({ label: t('rooms.form.unfocused'), input: spotUnfocused }),
    filter: formField({ label: t('rooms.form.filter'), input: filter, optional: true, hint: t('rooms.form.filterHint'),
      help: helpHint({ title: t('rooms.form.filter'), body: t('rooms.form.filterHelp'), guide: '10-rooms.md' }) }),
    labels: formField({ label: t('rooms.form.labels'), input: labels, hint: t('rooms.form.labelsHint') }),
    emptyTimeout: formField({ label: t('rooms.form.emptyTimeout'), input: emptyTimeout, hint: t('rooms.form.emptyTimeoutHint') }),
    webhook: formField({ label: t('rooms.form.webhook'), input: webhook, optional: true, hint: t('rooms.form.webhookHint') }),
    meta: formField({ label: t('rooms.form.metadata'), input: metaText, optional: true, hint: t('rooms.form.metadataHint') }),
  };
  const spotBox = h('div', { class: 'cols' }, f.spotNum, f.spotUnfocused);
  const syncSpot = () => { spotBox.hidden = !spotOn.checked; };
  spotOn.addEventListener('change', syncSpot);
  syncSpot();
  const e2eeWarn = alert('info', t('rooms.form.e2eeComposite'), { small: true });
  const syncE2ee = () => { e2eeWarn.hidden = !(e2ee.checked && recording.value === 'composite'); };
  e2ee.addEventListener('change', syncE2ee);
  recording.addEventListener('change', syncE2ee);
  syncE2ee();

  const advanced = disclosure(t('rooms.form.advanced'), t('rooms.form.advancedDesc'),
    h('div', { class: 'cols' }, f.bitrate, f.emptyTimeout),
    h('fieldset', null, h('legend', null, t('rooms.form.spotlight')),
      h('label', { class: 'check' }, spotOn, h('span', null, t('rooms.form.spotOn'))),
      h('div', { class: 'hint' }, t('rooms.form.spotHint')),
      spotBox),
    h('fieldset', null, h('legend', null, t('rooms.form.cascadeLegend')),
      h('label', { class: 'check' }, cascade, h('span', null, t('rooms.form.cascade'))),
      h('div', { class: 'hint' }, t('rooms.form.cascadeHint'))),
    f.filter, f.labels, f.webhook, f.meta);
  advanced.dataset.testid = 'room-advanced';
  // 既定から変えてある部屋の編集では、最初から開いておく
  if (room && (s.max_video_bitrate_kbps || s.spotlight || s.cascade || s.forwarding_filter || s.auth_webhook_url)) advanced.open = true;

  const el = h('div', { class: 'form room-form' },
    room ? null : h('div', { class: 'cols' }, f.id, f.title),
    room ? f.title : null,
    h('div', { class: 'cols' }, f.max, f.recording),
    f.codec,
    h('div', { class: 'field' },
      h('label', { class: 'check' }, e2ee, h('span', null, t('rooms.form.e2ee'))),
      h('div', { class: 'hint' }, locked ? t('rooms.form.lockedHint') : t('rooms.form.e2eeHint'))),
    e2eeWarn,
    advanced);

  const intIn = (input, field, lo, hi, { optional = false } = {}) => {
    const v = input.value.trim();
    if (!v && optional) return null;
    const n = Number(v);
    if (!Number.isInteger(n) || n < lo || (hi !== null && n > hi)) {
      field.setError(t('rooms.form.range', { lo, hi: hi ?? '∞' }));
      return undefined;
    }
    return n;
  };
  const jsonIn = (input, field) => {
    const v = input.value.trim();
    if (!v) return null;
    try { return JSON.parse(v); } catch { field.setError(t('rooms.form.badJson')); return undefined; }
  };

  const value = () => {
    Object.values(f).forEach((x) => x.setError(null));
    const bad = [];
    const settings = {
      max_participants: intIn(max, f.max, 1, 10000),
      e2ee: e2ee.checked,
      video_codec: codec.value,
      audio_codec: 'opus',
      recording: recording.value,
      data_labels: splitList(labels.value),
      empty_timeout_secs: intIn(emptyTimeout, f.emptyTimeout, 0, null),
      cascade: cascade.checked,
    };
    const br = intIn(bitrate, f.bitrate, 50, 50000, { optional: true });
    if (br) settings.max_video_bitrate_kbps = br;
    if (spotOn.checked) {
      const n = intIn(spotNum, f.spotNum, 1, 16);
      settings.spotlight = { number: n, unfocused: spotUnfocused.value };
      if (n === undefined) bad.push(f.spotNum);
    }
    const ff = jsonIn(filter, f.filter);
    if (ff === undefined) bad.push(f.filter); else if (ff) settings.forwarding_filter = ff;
    if (webhook.value.trim()) {
      if (!/^https?:\/\/\S+$/.test(webhook.value.trim())) { f.webhook.setError(t('rooms.form.badUrl')); bad.push(f.webhook); }
      settings.auth_webhook_url = webhook.value.trim();
    }
    if (!settings.data_labels.length || settings.data_labels.some((l) => !l.startsWith('#'))) { f.labels.setError(t('rooms.form.badLabels')); bad.push(f.labels); }
    if (settings.max_participants === undefined) bad.push(f.max);
    if (settings.empty_timeout_secs === undefined) bad.push(f.emptyTimeout);
    if (br === undefined) bad.push(f.bitrate);
    const extra = jsonIn(metaText, f.meta);
    if (extra === undefined || (extra !== null && (typeof extra !== 'object' || Array.isArray(extra)))) {
      if (extra !== undefined) f.meta.setError(t('rooms.form.metaObject'));
      bad.push(f.meta);
    }
    if (!room && id.value.trim() && !ROOM_ID_RE.test(id.value.trim())) { f.id.setError(t('rooms.form.badId')); bad.push(f.id); }
    if (bad.length) {
      if ([f.filter, f.labels, f.webhook, f.meta, f.bitrate, f.emptyTimeout, f.spotNum].includes(bad[0])) advanced.open = true;
      bad[0].input.focus();
      return null;
    }
    const metadata = { ...(extra || {}) };
    if (title.value.trim()) metadata.title = title.value.trim();
    if (!room) {
      const b = { settings, metadata };
      if (id.value.trim()) b.id = id.value.trim();
      return b;
    }
    // 編集: 変えた項目だけ（参加者がいる間は e2ee・コーデックを送らない）
    const next = { ...settings };
    if (locked) { next.e2ee = s.e2ee; next.video_codec = s.video_codec; }
    const patch = {};
    const sp = settingsPatch(s, next);
    if (sp) patch.settings = sp;
    if (JSON.stringify(metadata) !== JSON.stringify(meta)) patch.metadata = metadata;
    return patch;
  };
  return { el, value, focus: () => (room ? title : id).focus() };
}
