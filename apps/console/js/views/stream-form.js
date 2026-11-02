// 配信の作成・編集のフォーム（StreamCreate / StreamPatch）。

import { h, field, select, checkbox } from '../lib/dom.js';

const NAME_RE = /^[A-Za-z0-9_-][A-Za-z0-9._-]*$/;

function num(value, attrs = {}) {
  return h('input', { type: 'number', inputmode: 'numeric', value: value ?? '', ...attrs });
}

/** メタデータ（文字列の key/value、32 個まで）の編集。 */
function metadataEditor(initial) {
  const rows = h('div', { class: 'form', style: { gap: '6px' } });
  const add = (k = '', v = '') => {
    const key = h('input', { type: 'text', value: k, placeholder: 'キー', maxlength: 64, 'aria-label': 'メタデータのキー' });
    const val = h('input', { type: 'text', value: v, placeholder: '値', maxlength: 1024, 'aria-label': 'メタデータの値' });
    const row = h('div', { class: 'inline' }, key, val,
      h('button', { type: 'button', class: 'small', 'aria-label': '削除', onClick: () => row.remove() }, '✕'));
    row.key = key;
    row.val = val;
    rows.appendChild(row);
  };
  for (const [k, v] of Object.entries(initial || {})) add(k, v);
  const el = h('fieldset', null, h('legend', null, 'メタデータ'), rows,
    h('div', null, h('button', { type: 'button', class: 'small', onClick: () => add() }, '＋ 追加')));
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
  const name = h('input', { type: 'text', required: true, maxlength: 64, pattern: '[A-Za-z0-9_\\-][A-Za-z0-9._\\-]*', value: s ? s.name : '', autocomplete: 'off', placeholder: 'studio-a' });
  const latency = select([['standard', 'standard（HLS）'], ['low', 'low（LL-HLS）'], ['interactive', 'interactive（WebRTC・MoQ）']], s ? s.latency_profile : 'standard');
  const codec = select([['any', 'any（制限なし）'], ['royalty_free', 'royalty_free（AV1・VP9・Opus など）']], s ? s.codec_profile : 'any');
  const containers = caps ? caps.output.hls.containers : ['ts', 'fmp4'];
  const container = select(containers.map((c) => [c, c === 'ts' ? 'ts（MPEG-TS）' : 'fmp4（CMAF）']), s ? s.hls.container : 'fmp4');
  const segment = num(s ? s.hls.segment_ms : 2000, { min: 500, max: 10000, step: 100 });
  const windowN = num(s ? s.hls.window : 6, { min: 3, max: 60 });
  const recEnabled = checkbox('publish が始まったら自動で録画する', s ? s.recording.enabled : false);
  const retention = num(s ? s.recording.retention_days : '', { min: 1, max: 36500, placeholder: '無期限' });
  const snapEnabled = checkbox('一定間隔で静止画を保存する', s ? !!s.snapshots : false);
  const snapInterval = num(s && s.snapshots ? s.snapshots.interval_secs : 60, { min: 1, max: 86400 });
  const snapKeep = num(s && s.snapshots ? s.snapshots.keep : 100, { min: 1, max: 10000 });
  const maxViewers = num(s ? s.limits.max_viewers : '', { min: 0, placeholder: '制限なし' });
  const maxIngress = num(s ? s.limits.max_ingress_bps : '', { min: 0, placeholder: '制限なし' });
  const desired = select([['running', 'running（publish を受け付ける）'], ['stopped', 'stopped（publish を拒否）']], s ? s.desired_state : 'running');
  const meta = metadataEditor(s ? s.metadata : {});
  const errors = h('div', { role: 'alert' });

  const rfHint = h('div', { class: 'hint' });
  const syncHint = () => {
    rfHint.textContent = codec.value === 'royalty_free' && container.value !== 'fmp4'
      ? 'royalty_free では HLS のコンテナを fmp4 にしてください（H.264・H.265・AAC は理由付きで拒否されます）。'
      : container.value === 'ts' ? 'ts は Safari 以外のブラウザでは管理画面のプレビューで再生できません（VLC などは可）。' : '';
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

  const el = h('div', { class: 'form' },
    field('名前', name, '英数字と . _ -（64 文字まで、テナント内で一意）'),
    h('div', { class: 'cols' }, field('遅延の種類', latency), field('コーデックの方針', codec), field('状態', desired)),
    h('fieldset', null, h('legend', null, 'HLS'),
      h('div', { class: 'cols' }, field('コンテナ', container), field('セグメント長（ミリ秒）', segment), field('窓（本数）', windowN)),
      rfHint),
    h('fieldset', null, h('legend', null, '録画'), recEnabled.el, field('保持日数', retention, '空なら無期限')),
    h('fieldset', null, h('legend', null, '静止画'), snapEnabled.el,
      h('div', { class: 'cols' }, field('間隔（秒）', snapInterval), field('残す枚数', snapKeep))),
    h('fieldset', null, h('legend', null, '上限'),
      h('div', { class: 'cols' }, field('同時視聴数', maxViewers), field('受信ビットレート（bps）', maxIngress))),
    meta.el,
    errors);

  const intOrNull = (input) => (input.value === '' ? null : Number(input.value));

  return {
    el,
    focus: () => name.focus(),
    /** 検査して本文を返す。だめなら null。 */
    value() {
      const problems = [];
      if (!NAME_RE.test(name.value) || name.value.length > 64) problems.push('名前は英数字と . _ - で 64 文字まで（先頭に . は使えません）');
      const seg = Number(segment.value);
      if (!(seg >= 500 && seg <= 10000)) problems.push('セグメント長は 500〜10000 ミリ秒');
      const win = Number(windowN.value);
      if (!(win >= 3 && win <= 60)) problems.push('窓は 3〜60 本');
      if (codec.value === 'royalty_free' && container.value !== 'fmp4') problems.push('royalty_free では HLS のコンテナを fmp4 にしてください');
      const md = meta.value();
      if (Object.keys(md).length > 32) problems.push('メタデータは 32 個まで');
      if (snapEnabled.input.checked && !(Number(snapInterval.value) >= 1 && Number(snapKeep.value) >= 1)) problems.push('静止画の間隔と枚数を入れてください');
      errors.replaceChildren(...(problems.length ? [h('div', { class: 'alert error' }, h('ul', null, problems.map((p) => h('li', null, p))))] : []));
      if (problems.length) return null;
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
