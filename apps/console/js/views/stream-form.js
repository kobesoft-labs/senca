// 配信の作成・設定のフォーム（StreamCreate / StreamPatch）。
//
// 部品（parts）に分けて返すので、作成のダイアログは「名前・自動録画」だけを見せて残りを「詳しい設定」に畳み、
// 詳細ページの設定タブは全部を並べる（el）。value() はどちらの並べ方でも同じ本文を返す。

import { h, select } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { formField, alert, helpHint, toggle } from '../ui/index.js';
import { icon } from '../lib/icons.js';

export const NAME_RE = /^[A-Za-z0-9_-][A-Za-z0-9._-]*$/;

/** 鍵の順によらない JSON（変わったかどうかの比較）。 */
const canon = (v) => JSON.stringify(v, (_k, x) => (x && typeof x === 'object' && !Array.isArray(x) ? Object.fromEntries(Object.entries(x).sort()) : x));

function num(value, attrs = {}) {
  return h('input', { type: 'number', inputmode: 'numeric', value: value ?? '', ...attrs });
}

/** メタデータ（文字列の key/value、32 個まで）の編集。 */
function metadataEditor(initial) {
  const rows = h('div', { class: 'meta-rows' });
  const add = (k = '', v = '') => {
    const key = h('input', { type: 'text', value: k, placeholder: t('streams.form.metaKey'), maxlength: 64, 'aria-label': t('streams.form.metaKeyLabel') });
    const val = h('input', { type: 'text', value: v, placeholder: t('streams.form.metaValue'), maxlength: 1024, 'aria-label': t('streams.form.metaValueLabel') });
    const row = h('div', { class: 'meta-row' }, key, val,
      h('button', { type: 'button', class: 'ghost icon-only', 'aria-label': t('streams.form.metaRemove'), onClick: () => row.remove() }, icon('close')));
    row.key = key;
    row.val = val;
    rows.appendChild(row);
  };
  for (const [k, v] of Object.entries(initial || {})) add(k, v);
  const el = h('fieldset', null, h('legend', null, t('streams.form.metadata')),
    h('p', { class: 'hint' }, t('streams.form.metaHint')), rows,
    h('div', null, h('button', { type: 'button', class: 'sm', onClick: () => add() }, icon('plus'), t('streams.form.metaAdd'))));
  return {
    el,
    value() {
      const out = {};
      for (const r of rows.children) {
        const k = r.key.value.trim();
        if (k) out[k] = r.val.value;
      }
      return out;
    },
  };
}

/**
 * @param {object} [s] 既存の Stream（設定の変更のとき）
 * @param {object} [caps] capabilities
 */
export function streamForm(s = null, caps = null) {
  const editing = !!s;
  const name = h('input', { type: 'text', required: true, maxlength: 64, pattern: '[A-Za-z0-9_\\-][A-Za-z0-9._\\-]*', value: s ? s.name : '', autocomplete: 'off', spellcheck: 'false', placeholder: 'studio-a', 'data-testid': 'stream-name' });
  const latency = select([['standard', t('streams.form.latStandard')], ['low', t('streams.form.latLow')], ['interactive', t('streams.form.latInteractive')]], s ? s.latency_profile : 'standard', { 'data-testid': 'stream-latency' });
  const codec = select([['any', t('streams.form.codecAny')], ['royalty_free', t('streams.form.codecRf')]], s ? s.codec_profile : 'any', { 'data-testid': 'stream-codec' });
  const containers = caps ? caps.output.hls.containers : ['ts', 'fmp4'];
  const container = select(containers.map((c) => [c, c === 'ts' ? t('streams.form.ts') : t('streams.form.fmp4')]), s ? s.hls.container : (containers.includes('fmp4') ? 'fmp4' : containers[0]));
  const segment = num(s ? s.hls.segment_ms : 2000, { min: 500, max: 10000, step: 100 });
  const windowN = num(s ? s.hls.window : 6, { min: 3, max: 60 });
  const recEnabled = toggle({ label: t('streams.form.autoRecord'), description: t('streams.form.autoRecordHint'), checked: s ? s.recording.enabled : false, testid: 'stream-autorecord' });
  const retention = num(s ? s.recording.retention_days : '', { min: 1, max: 36500, placeholder: t('common.unlimited') });
  const snapEnabled = h('input', { type: 'checkbox', checked: s ? !!s.snapshots : false });
  const snapInterval = num(s && s.snapshots ? s.snapshots.interval_secs : 60, { min: 1, max: 86400 });
  const snapKeep = num(s && s.snapshots ? s.snapshots.keep : 100, { min: 1, max: 10000 });
  const maxViewers = num(s ? s.limits.max_viewers : '', { min: 0, placeholder: t('common.noLimit') });
  const maxIngress = num(s && s.limits.max_ingress_bps ? Math.round(s.limits.max_ingress_bps / 1000) : '', { min: 0, placeholder: t('common.noLimit') });
  const meta = metadataEditor(s ? s.metadata : {});
  const errors = h('div');

  const rfHint = h('div', { class: 'hint' });
  const syncHint = () => {
    rfHint.textContent = codec.value === 'royalty_free' && container.value !== 'fmp4'
      ? t('streams.form.rfNeedsFmp4')
      : container.value === 'ts' ? t('streams.form.tsNote') : '';
    rfHint.hidden = !rfHint.textContent;
  };
  codec.addEventListener('change', () => {
    if (codec.value === 'royalty_free') container.value = 'fmp4';
    syncHint();
  });
  container.addEventListener('change', syncHint);
  syncHint();
  const syncSnap = () => { snapInterval.disabled = !snapEnabled.checked; snapKeep.disabled = !snapEnabled.checked; };
  snapEnabled.addEventListener('change', syncSnap);
  syncSnap();
  const syncRec = () => { retention.disabled = !recEnabled.input.checked; };
  recEnabled.input.addEventListener('change', syncRec);
  syncRec();

  const nameField = formField({ label: t('common.name'), input: name, required: true, hint: t('streams.form.nameHint') });
  name.addEventListener('blur', () => nameField.setError(name.value && !NAME_RE.test(name.value) ? t('streams.form.nameInvalid') : null));
  name.addEventListener('input', () => { if (NAME_RE.test(name.value)) nameField.setError(null); });

  const delivery = h('fieldset', null, h('legend', null, t('streams.form.delivery')),
    h('div', { class: 'cols' },
      formField({ label: t('streams.form.latency'), input: latency, help: helpHint({ title: t('streams.form.latency'), body: t('streams.form.latencyHelp'), guide: '04-ingest-playback.md' }) }),
      formField({ label: t('streams.form.codec'), input: codec, help: helpHint({ title: t('streams.form.codec'), body: t('streams.form.codecHelp'), guide: '14-codecs-and-patents.md' }) })),
    h('div', { class: 'cols' },
      formField({ label: t('streams.form.container'), input: container, help: helpHint({ title: t('streams.form.container'), body: t('streams.form.containerHelp'), guide: '04-ingest-playback.md' }) }),
      formField({ label: t('streams.form.segment'), input: segment }),
      formField({ label: t('streams.form.window'), input: windowN })),
    rfHint);
  const retentionField = formField({ label: t('streams.form.retention'), input: retention, hint: t('streams.form.retentionHint') });
  const recording = h('fieldset', null, h('legend', null, t('streams.form.recording')), recEnabled.el, retentionField);
  const snapshots = h('fieldset', null, h('legend', null, t('streams.form.snapshotsLegend')),
    h('label', { class: 'check' }, snapEnabled, t('streams.form.snapshots')),
    h('div', { class: 'cols' }, formField({ label: t('streams.form.snapInterval'), input: snapInterval }), formField({ label: t('streams.form.snapKeep'), input: snapKeep })));
  const limits = h('fieldset', null, h('legend', null, t('streams.form.limits')),
    h('div', { class: 'cols' },
      formField({ label: t('streams.form.maxViewers'), input: maxViewers, hint: t('streams.form.maxViewersHint') }),
      formField({ label: t('streams.form.maxIngress'), input: maxIngress, hint: t('streams.form.maxIngressHint') })));

  const intOrNull = (input) => (input.value === '' ? null : Number(input.value));
  let layout = null;

  return {
    parts: { name: nameField, recordToggle: recEnabled.el, delivery, recording, snapshots, limits, metadata: meta.el, errors, retentionField },
    /** 全部を並べたもの（設定タブ）。 */
    get el() {
      if (!layout) {
        layout = h('div', { class: 'form settings-form' },
          nameField,
          h('div', { class: 'settings-grid' }, delivery, recording, snapshots, limits),
          meta.el,
          errors);
      }
      return layout;
    },
    focus: () => name.focus(),
    setLatency: (v) => { latency.value = v; },
    /** 検査して本文を返す。だめなら null。 */
    value() {
      const problems = [];
      if (!NAME_RE.test(name.value) || name.value.length > 64) {
        problems.push(t('streams.form.nameInvalid'));
        nameField.setError(t('streams.form.nameInvalid'));
      }
      const seg = Number(segment.value);
      if (!(seg >= 500 && seg <= 10000)) problems.push(t('streams.form.segmentRange'));
      const win = Number(windowN.value);
      if (!(win >= 3 && win <= 60)) problems.push(t('streams.form.windowRange'));
      if (codec.value === 'royalty_free' && container.value !== 'fmp4') problems.push(t('streams.form.rfNeedsFmp4Short'));
      const md = meta.value();
      if (Object.keys(md).length > 32) problems.push(t('streams.form.metaTooMany'));
      if (snapEnabled.checked && !(Number(snapInterval.value) >= 1 && Number(snapKeep.value) >= 1)) problems.push(t('streams.form.snapNeeded'));
      errors.replaceChildren(...(problems.length ? [alert('error', h('ul', null, problems.map((p) => h('li', null, p))), { title: t('streams.form.fixBelow') })] : []));
      if (problems.length) {
        if (!NAME_RE.test(name.value)) name.focus();
        return null;
      }
      const ingressKbps = intOrNull(maxIngress);
      const body = {
        name: name.value,
        latency_profile: latency.value,
        codec_profile: codec.value,
        hls: { container: container.value, segment_ms: seg, window: win },
        recording: { enabled: recEnabled.input.checked, retention_days: intOrNull(retention) },
        snapshots: snapEnabled.checked ? { interval_secs: Number(snapInterval.value), keep: Number(snapKeep.value) } : null,
        limits: { max_viewers: intOrNull(maxViewers), max_ingress_bps: ingressKbps === null ? null : ingressKbps * 1000 },
        metadata: md,
      };
      if (editing) {
        // 変わったところだけ送る（snapshots: null は「消す」なので、元が null なら送らない）
        const out = {};
        for (const [k, v] of Object.entries(body)) {
          if (canon(v) !== canon(s[k])) out[k] = v;
        }
        return out;
      }
      if (body.snapshots === null) delete body.snapshots;
      return body;
    },
  };
}
