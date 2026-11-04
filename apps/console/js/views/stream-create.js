// 配信の作成（はじめての人向けの流れ）: 名前 →「どこから送りますか」→ 作成 → 詳細ページの「送り手をつなぐ」へ。
// 詳しい設定（遅延・コーデック・HLS・静止画・上限・メタデータ）は畳んでおく。
//
// - 送り手の種類は API には無い（どの方式でも同じ配信に送れる）。選んだ種類は既定値（遅延）と、作成後の案内（?setup=）にだけ使う。
// - 「カメラ・URL から取り込む」は、配信を作ってから入力（*_pull）を 1 つ足す。
// - 同じ入力の再送で二重に作らないよう、ダイアログごとに Idempotency-Key を 1 つ使う。

import { h, replace } from '../lib/dom.js';
import { modal, errorBox, toast } from '../lib/ui.js';
import { streams, inputs as inputsApi } from '../api/resources.js';
import { idemKey } from '../lib/format.js';
import { streamForm } from './stream-form.js';
import { t } from '../i18n/index.js';
import { choiceCards, formField } from '../ui/index.js';
import { invalidatePalette } from '../palette.js';
import { icon } from '../lib/icons.js';

/** URL の形から入力の方式を決める。 */
export function pullProtocolOf(url) {
  const u = String(url || '').trim().toLowerCase();
  if (u.startsWith('rtsp://') || u.startsWith('rtsps://')) return 'rtsp_pull';
  if (u.startsWith('rtmp://') || u.startsWith('rtmps://')) return 'rtmp_pull';
  if (u.startsWith('srt://')) return 'srt_pull';
  if (/^https?:\/\//.test(u)) return 'hls_pull';
  return null;
}

/** 送り手の種類（作成の後の案内で使う）。 */
export const SOURCES = ['rtmp', 'srt', 'whip', 'pull'];

/**
 * 作成のダイアログ。作ったら {stream, source, input, inputError} を返す（やめたら null）。
 * @param {object} caps capabilities
 */
export function createStreamDialog(caps) {
  const form = streamForm(null, caps);
  const ing = (caps && caps.ingest) || {};
  const errBox = h('div');
  const key = idemKey();
  const pullUrl = h('input', { type: 'url', spellcheck: 'false', autocomplete: 'off', placeholder: 'rtsp://192.0.2.10/stream1', 'data-testid': 'stream-pull-url' });
  const pullField = formField({ label: t('streams.create.pullUrl'), input: pullUrl, hint: t('streams.create.pullUrlHint') });
  const pullBox = h('div', { class: 'create-pull', hidden: true }, pullField);
  const source = choiceCards({
    label: t('streams.create.sourceLabel'),
    testid: 'stream-source',
    value: 'rtmp',
    options: [
      { value: 'rtmp', icon: 'monitor', title: t('streams.create.src.rtmp'), description: t('streams.create.src.rtmpDesc') },
      { value: 'srt', icon: 'activity', title: t('streams.create.src.srt'), description: t('streams.create.src.srtDesc'), disabled: ing.srt === false, note: ing.srt === false ? t('streams.create.src.notEnabled') : null },
      { value: 'whip', icon: 'camera', title: t('streams.create.src.whip'), description: t('streams.create.src.whipDesc'), disabled: ing.whip === false, note: ing.whip === false ? t('streams.create.src.notEnabled') : null },
      { value: 'pull', icon: 'download', title: t('streams.create.src.pull'), description: t('streams.create.src.pullDesc') },
    ],
    onChange: (v) => {
      pullBox.hidden = v !== 'pull';
      // 既定の遅延: ブラウザ（WebRTC）は interactive、ほかは standard
      form.setLatency(v === 'whip' ? 'interactive' : 'standard');
      if (v === 'pull') pullUrl.focus();
    },
  });
  const advanced = h('details', { class: 'disclosure', 'data-testid': 'stream-advanced' },
    h('summary', null, icon('settings'), h('span', null, t('streams.create.advanced')), h('small', null, t('streams.create.advancedHint'))),
    h('div', { class: 'form' }, form.parts.delivery, form.parts.retentionField, form.parts.snapshots, form.parts.limits, form.parts.metadata));

  const submit = async (close, ok) => {
    const body = form.value();
    if (!body) {
      if (form.parts.errors.querySelector('.alert') && !form.parts.name.querySelector('[aria-invalid="true"]')) advanced.open = true;
      return;
    }
    const src = source.value;
    let proto = null;
    if (src === 'pull') {
      proto = pullProtocolOf(pullUrl.value);
      if (!proto) {
        pullField.setError(t('streams.create.pullUrlInvalid'));
        pullUrl.focus();
        return;
      }
      pullField.setError(null);
    }
    ok.disabled = true;
    replace(errBox);
    try {
      const { data } = await streams.create(body, key);
      invalidatePalette();
      let input = null;
      let inputError = null;
      if (proto) {
        try { input = (await inputsApi.create(data.id, { protocol: proto, url: pullUrl.value.trim(), enabled: true })).data; } catch (e) { inputError = e; }
      }
      close({ stream: data, source: src, input, inputError });
    } catch (e) {
      replace(errBox, errorBox(e, { title: t('streams.createFailed') }));
    } finally {
      ok.disabled = false;
    }
  };

  return modal(t('streams.createTitle'), () => [
    h('div', { class: 'form create-stream' },
      h('p', { class: 'muted small create-lead' }, t('streams.create.lead')),
      form.parts.name,
      source,
      pullBox,
      form.parts.recordToggle,
      advanced,
      form.parts.errors),
    errBox,
  ], {
    wide: true,
    icon: 'streams',
    testid: 'stream-create-dialog',
    actions: (close) => {
      const ok = h('button', { type: 'button', class: 'primary', 'data-testid': 'stream-create-submit' }, t('streams.create.submit'));
      ok.addEventListener('click', () => submit(close, ok));
      return [h('button', { type: 'button', onClick: () => close(null) }, t('common.cancel')), ok];
    },
  }).then((r) => {
    if (r && r.inputError) toast(t('streams.create.inputFailed'), 'warn', 9000);
    return r || null;
  });
}

