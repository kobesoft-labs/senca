// エラーの表示・読み込みの包み・ダウンロード。部品（toast・modal など）は js/ui/ にあり、ここからも使えるように出し直す。

import { h, replace } from './dom.js';
import { t } from '../i18n/index.js';
import { toast } from '../ui/toast.js';
import { modal, confirmDialog } from '../ui/overlay.js';
import { copyField, copyText } from '../ui/form.js';
import { alert as alertBox, skeleton } from '../ui/feedback.js';

export { toast, modal, confirmDialog, copyField, copyText };

function lead(status) {
  if (status === 0) return t('errors.lead.0');
  if ([400, 403, 404, 409, 412, 413, 415, 422, 428, 429, 503].includes(status)) return t(`errors.lead.${status}`);
  if (status >= 500) return t('errors.lead.5xx');
  return t('errors.lead.other');
}

/**
 * ApiError を人が読める文にする: 言語に合わせた前置き ＋ API の説明（problem の title）＋ 再試行の目安。
 * API の説明はノードの言葉のまま出す（言語を変えても訳さない）。
 */
export function describeError(err) {
  if (!err) return t('errors.unknown');
  if (err.name !== 'ApiError') return err.message || String(err);
  let msg = lead(err.status);
  if (err.title && err.title !== t(`http.${err.status}`) && err.status !== 0) msg = t('errors.withDetail', { lead: msg, detail: err.title });
  const wait = err.retryAfter ?? t('errors.someSeconds');
  if (err.status === 429) msg += t('errors.retryAfter', { s: wait });
  else if (err.status === 503) msg += err.retryable || err.retryAfter !== null ? t('errors.retryAfter', { s: wait }) : t('errors.noRetry');
  else if (err.status === 412) msg += t('errors.reloaded');
  else if (err.retryable) msg += t('errors.retryable');
  return msg;
}

/** エラーの詳細（code・request_id）付きの表示。 */
export function errorBox(err, { title, actions } = {}) {
  const meta = [];
  if (err && err.status) meta.push(`HTTP ${err.status}`);
  if (err && err.code) meta.push(err.code);
  if (err && err.requestId) meta.push(`request_id: ${err.requestId}`);
  const details = err && err.details && typeof err.details === 'object' && err.details.reason ? String(err.details.reason) : null;
  return alertBox('error', [
    describeError(err),
    details ? h('div', { class: 'small' }, details) : null,
    meta.length ? h('div', { class: 'meta' }, meta.join(' · ')) : null,
  ], { title, actions, role: 'alert' });
}

/** 操作の失敗を通知する（401 は client.js がログインへ戻す）。retry を渡すと「再試行」を付ける。 */
export function reportError(err, what, { retry } = {}) {
  if (err && err.name === 'AbortError') return;
  if (err && err.status === 401) return;
  const meta = err && err.requestId ? ` (${err.code || err.status} / ${err.requestId})` : err && err.code ? ` (${err.code})` : '';
  const msg = `${what ? t('errors.what', { what }) : ''}${describeError(err)}${meta}`;
  toast(msg, 'error', retry ? { duration: 10000, action: { label: t('common.retry'), onClick: retry } } : 9000);
}

/** 読み込み中の形（スケルトン）を出してから、非同期の結果で置き換える。 */
export async function load(container, fn, { title, kind = 'lines' } = {}) {
  // 2 回目からは前の中身を出したまま読み直す（SSE の更新でちらつかないように）
  if (container.dataset.loaded !== '1') replace(container, skeleton({ kind, rows: kind === 'table' ? 4 : 3 }));
  container.setAttribute('aria-busy', 'true');
  try {
    const node = await fn();
    replace(container, node);
    container.dataset.loaded = '1';
    return true;
  } catch (e) {
    if (e && e.status === 401) return false;
    replace(container, errorBox(e, { title }));
    container.dataset.loaded = '';
    return false;
  } finally {
    container.removeAttribute('aria-busy');
  }
}

/** 一度だけ見せる秘密（stream key・API トークン）。 */
export function secretDialog(title, label, value, warning) {
  return modal(title, () => [
    alertBox('warn', warning || t('secret.onceWarning'), { role: 'alert' }),
    h('div', { class: 'field' }, h('div', { class: 'label' }, label), copyField(value, label, { secret: true })),
  ], { icon: 'key', kind: 'warn', actions: (close) => [h('button', { type: 'button', class: 'primary', onClick: () => close(true) }, t('secret.saved'))] });
}

/** Blob を名前付きでダウンロードさせる。 */
export function downloadBlob(blob, name) {
  const url = URL.createObjectURL(blob);
  const a = h('a', { href: url, download: name });
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 30000);
}
