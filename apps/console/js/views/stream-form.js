// 配信の作成・編集のフォーム（StreamCreate / StreamPatch）。

import { h, select, checkbox } from '../lib/dom.js';
import { t } from '../i18n/index.js';
import { formField, alert, helpHint } from '../ui/index.js';
import { icon } from '../lib/icons.js';

const NAME_RE = /^[A-Za-z0-9_-][A-Za-z0-9._-]*$/;

function num(value, attrs = {}) {
  return h('input', { type: 'number', inputmode: 'numeric', value: value ?? '', ...attrs });
}

/** メタデータ（文字列の key/value、32 個まで）の編集。 */
function metadataEditor(initial) {
  const rows = h('div', { class: 'stack' });
  rows.style.gap = '8px';
  const add = (k = '', v = '') => {
    const key = h('input', { type: 'text', value: k, placeholder: t('streams.form.metaKey'), maxlength: 64, 'aria-label': t('streams.form.metaKeyLabel') });
    const val = h('input', { type: 'text', value: v, placeholder: t('streams.form.metaValue'), maxlength: 1024, 'aria-label': t('streams.form.metaValueLabel') });
    key.style.flex = '1';
    val.style.flex = '2';
    const row = h('div', { class: 'inline' }, key, val,
      h('button', { type: 'button', class: 'ghost icon-only', 'aria-label': t('streams.form.metaRemove'), onClick: () => row.remove() }, icon('close')));
    row.key = key;
    row.val = val;
    rows.appendChild(row);
  };
  for (const [k, v] of Object.entries(initial || {})) add(k, v);
  const el = h('fieldset', null, h('legend', null, t('streams.form.metadata')), rows,
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
 * @param {object} [s] 既存の Stream（編集のとき）
 * @param {object} [caps] capabilities
 */
export function streamForm(s = null, caps = null) {
  const editing = !!s;
  const name = h('input', { type: 'text', required: true, maxlength: 64, pattern: '[A-Za-z0-9_\\-][A-Za-z0-9._\\-]*', value: s ? s.name : '', autocomplete: 'off', spellcheck: 'false', placeholder: 'studio-a', 'data-testid': 'stream-name' });
  const latency = select([['standard', t('streams.form.latStandard')], ['low', t('streams.form.latLow')], ['interactive', t('streams.form.latInteractive')]], s ? s.latency_profile : 'standard');
  const codec = select([['any', t('streams.form.codecAny')], ['royalty_free', t('streams.form.codecRf')]], s ? s.codec_profile : 'any');
  const containers = caps ? caps.output.hls.containers : ['ts', 'fmp4'];
  const container = select(containers.map((c) => [c, c === 'ts' ? t('streams.form.ts') : t('streams.form.fmp4')]), s ? s.hls.container : 'fmp4');
  const segment = num(s ? s.hls.segment_ms : 2000, { min: 500, max: 10000, step: 100 });
  const windowN = num(s ? s.hls.window : 6, { min: 3, max: 60 });
  const recEnabled = checkbox(t('streams.form.autoRecord'), s ? s.recording.enabled : false);
  const retention = num(s ? s.recording.retention_days : '', { min: 1, max: 36500, placeholder: t('common.unlimited') });
  const snapEnabled = checkbox(t('streams.form.snapshots'), s ? !!s.snapshots : false);
  const snapInterval = num(s && s.snapshots ? s.snapshots.interval_secs : 60, { min: 1, max: 86400 });
  const snapKeep = num(s && s.snapshots ? s.snapshots.keep : 100, { min: 1, max: 10000 });
  const maxViewers = num(s ? s.limits.max_viewers : '', { min: 0, placeholder: t('common.noLimit') });
  const maxIngress = num(s ? s.limits.max_ingress_bps : '', { min: 0, placeholder: t('common.noLimit') });
  const desired = select([['running', t('streams.form.desiredRunning')], ['stopped', t('streams.form.desiredStopped')]], s ? s.desired_state : 'running');
  const meta = metadataEditor(s ? s.metadata : {});
  const errors = h('div');

  const rfHint = h('div', { class: 'hint' });
  const syncHint = () => {
    rfHint.textContent = codec.value === 'royalty_free' && container.value !== 'fmp4'
      ? t('streams.form.rfNeedsFmp4')
      : container.value === 'ts' ? t('streams.form.tsNote') : '';
  };
  codec.addEventListener('change', () => {
    if (codec.value === 'royalty_free') container.value = 'fmp4';
    syncHint();
  });
  container.addEventListener('change', syncHint);
  syncHint();
  const syncSnap = () => { snapInterval.disabled = !snapEnabled.input.checked; snapKeep.disabled = !snapEnabled.input.checked; };
  snapEnabled.input.addEventListener('change', syncSnap);
  syncSnap();

  const nameField = formField({ label: t('common.name'), input: name, required: true, hint: t('streams.form.nameHint') });
  name.addEventListener('blur', () => nameField.setError(name.value && !NAME_RE.test(name.value) ? t('streams.form.nameInvalid') : null));
  name.addEventListener('input', () => { if (NAME_RE.test(name.value)) nameField.setError(null); });

  const el = h('div', { class: 'form' },
    nameField,
    h('div', { class: 'cols' },
      formField({ label: t('streams.form.latency'), input: latency, help: helpHint({ title: t('streams.form.latency'), body: t('streams.form.latencyHelp'), guide: '04-ingest-playback.md' }) }),
      formField({ label: t('streams.form.codec'), input: codec, help: helpHint({ title: t('streams.form.codec'), body: t('streams.form.codecHelp'), guide: '14-codecs-and-patents.md' }) }),
      formField({ label: t('common.state'), input: desired })),
    h('fieldset', null, h('legend', null, 'HLS'),
      h('div', { class: 'cols' }, formField({ label: t('streams.form.container'), input: container }), formField({ label: t('streams.form.segment'), input: segment }), formField({ label: t('streams.form.window'), input: windowN })),
      rfHint),
    h('fieldset', null, h('legend', null, t('streams.form.recording')), recEnabled.el, formField({ label: t('streams.form.retention'), input: retention, hint: t('streams.form.retentionHint') })),
    h('fieldset', null, h('legend', null, t('streams.form.snapshotsLegend')), snapEnabled.el,
      h('div', { class: 'cols' }, formField({ label: t('streams.form.snapInterval'), input: snapInterval }), formField({ label: t('streams.form.snapKeep'), input: snapKeep }))),
    h('fieldset', null, h('legend', null, t('streams.form.limits')),
      h('div', { class: 'cols' }, formField({ label: t('streams.form.maxViewers'), input: maxViewers }), formField({ label: t('streams.form.maxIngress'), input: maxIngress }))),
    meta.el,
    errors);

  const intOrNull = (input) => (input.value === '' ? null : Number(input.value));

  return {
    el,
    focus: () => name.focus(),
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
      if (snapEnabled.input.checked && !(Number(snapInterval.value) >= 1 && Number(snapKeep.value) >= 1)) problems.push(t('streams.form.snapNeeded'));
      errors.replaceChildren(...(problems.length ? [alert('error', h('ul', null, problems.map((p) => h('li', null, p))), { title: t('streams.form.fixBelow') })] : []));
      if (problems.length) {
        if (!NAME_RE.test(name.value)) name.focus();
        return null;
      }
      const body = {
        name: name.value,
        latency_profile: latency.value,
        codec_profile: codec.value,
        desired_state: desired.value,
        hls: { container: container.value, segment_ms: seg, window: win },
        recording: { enabled: recEnabled.input.checked, retention_days: intOrNull(retention) },
        snapshots: snapEnabled.input.checked ? { interval_secs: Number(snapInterval.value), keep: Number(snapKeep.value) } : null,
        limits: { max_viewers: intOrNull(maxViewers), max_ingress_bps: intOrNull(maxIngress) },
        metadata: md,
      };
      if (editing) {
        // 変わったところだけ送る（snapshots: null は「消す」なので、元が null なら送らない）
        const out = {};
        for (const [k, v] of Object.entries(body)) {
          if (JSON.stringify(v) !== JSON.stringify(s[k])) out[k] = v;
        }
        return out;
      }
      if (body.snapshots === null) delete body.snapshots;
      return body;
    },
  };
}
