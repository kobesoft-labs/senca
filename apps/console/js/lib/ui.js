// 通知・確認・ダイアログ・エラー表示。

import { h, replace } from './dom.js';

/** 画面右下の短い通知。 */
export function toast(message, kind = 'info', ms = 5000) {
  const box = document.getElementById('toasts');
  if (!box) return;
  const el = h('div', { class: ['toast', kind], role: kind === 'error' ? 'alert' : 'status' }, message);
  box.appendChild(el);
  while (box.children.length > 4) box.firstChild.remove();
  setTimeout(() => el.remove(), ms);
}

/** ApiError を人が読める文にする。429・503 は再試行の目安を足す。 */
export function describeError(err) {
  if (!err) return '不明なエラー';
  if (err.name !== 'ApiError') return err.message || String(err);
  let msg = err.title;
  if (err.status === 429) msg += `。${err.retryAfter ?? '数'} 秒ほど待ってから再試行してください`;
  else if (err.status === 503) {
    msg += err.retryable || err.retryAfter !== null
      ? `。${err.retryAfter ?? '数'} 秒ほど待ってから再試行してください`
      : '。再試行しても解決しません（装置・設定を確認してください）';
  } else if (err.status === 412) msg += '。最新の内容を読み込み直しました。もう一度操作してください';
  else if (err.retryable) msg += '。再試行できます';
  return msg;
}

/** エラーの詳細（code・request_id）付きの表示。 */
export function errorBox(err, { title } = {}) {
  const meta = [];
  if (err && err.status) meta.push(`HTTP ${err.status}`);
  if (err && err.code) meta.push(err.code);
  if (err && err.requestId) meta.push(`request_id: ${err.requestId}`);
  const details = err && err.details && typeof err.details === 'object' && err.details.reason ? String(err.details.reason) : null;
  return h('div', { class: 'alert error', role: 'alert' },
    title ? h('strong', null, title, ' ') : null,
    describeError(err),
    details ? h('div', { class: 'small' }, details) : null,
    meta.length ? h('div', { class: 'meta' }, meta.join(' · ')) : null);
}

/** 操作の失敗を通知する（401 は client.js がログインへ戻す）。 */
export function reportError(err, what) {
  if (err && err.name === 'AbortError') return;
  if (err && err.status === 401) return;
  const meta = err && err.requestId ? `（${err.code || err.status} / ${err.requestId}）` : err && err.code ? `（${err.code}）` : '';
  toast(`${what ? `${what}: ` : ''}${describeError(err)}${meta}`, 'error', 9000);
}

/**
 * モーダルのダイアログ。戻り値の Promise は閉じたときの値で解決する。
 * build(close) は本文の要素を返す。close(value) で閉じる。
 */
export function modal(title, build, { actions, wide } = {}) {
  return new Promise((resolve) => {
    const dlg = h('dialog', { 'aria-labelledby': 'dlg-title' });
    if (wide) dlg.style.width = 'min(900px, calc(100vw - 24px))';
    let done = false;
    const close = (v) => {
      if (done) return;
      done = true;
      dlg.close();
      dlg.remove();
      resolve(v);
    };
    dlg.addEventListener('cancel', (e) => { e.preventDefault(); close(undefined); });
    const body = build(close);
    dlg.append(
      h('div', { class: 'dlg-head' }, h('h2', { id: 'dlg-title' }, title)),
      h('div', { class: 'dlg-body' }, body),
      actions ? h('div', { class: 'dlg-foot' }, actions(close)) : null,
    );
    document.body.appendChild(dlg);
    dlg.showModal();
    const first = dlg.querySelector('[autofocus], input, select, textarea, button.primary, button');
    if (first) first.focus();
  });
}

/** 確認（危険な操作）。 */
export function confirmDialog(title, message, { ok = '実行', danger = true, requireText } = {}) {
  let input = null;
  return modal(title, () => {
    const parts = [h('p', null, message)];
    if (requireText) {
      input = h('input', { type: 'text', autocomplete: 'off', 'aria-label': `確認のため ${requireText} と入力` });
      parts.push(h('p', { class: 'small muted' }, '確認のため ', h('code', null, requireText), ' と入力してください。'), input);
    }
    return parts;
  }, {
    actions: (close) => {
      const okBtn = h('button', { class: danger ? 'danger solid' : 'primary', onClick: () => {
        if (requireText && input.value !== requireText) {
          input.focus();
          return;
        }
        close(true);
      } }, ok);
      return [h('button', { onClick: () => close(false) }, 'やめる'), okBtn];
    },
  }).then((v) => !!v);
}

/** 一度だけ見せる秘密（stream key・API トークン）。 */
export function secretDialog(title, label, value, warning) {
  return modal(title, () => [
    h('div', { class: 'alert warn', role: 'alert' }, warning || 'この値はもう表示されません。今すぐ控えてください。'),
    h('div', { class: 'field' }, h('strong', null, label)),
    copyField(value, label, { secret: true }),
  ], { actions: (close) => [h('button', { class: 'primary', onClick: () => close(true) }, '控えました')] });
}

/** クリップボードへの写し。 */
export async function copyText(text, label = '値') {
  try {
    await navigator.clipboard.writeText(text);
    toast(`${label}を写しました`, 'ok', 2500);
  } catch {
    toast('クリップボードへ写せませんでした。手で選んで写してください', 'warn');
  }
}

export function copyField(value, label, { secret = false } = {}) {
  const input = h('input', { type: 'text', readonly: true, value: value ?? '', 'aria-label': label, class: secret ? 'secret' : null, spellcheck: 'false' });
  input.addEventListener('focus', () => input.select());
  return h('div', { class: 'copy-field' }, input,
    h('button', { type: 'button', class: 'small', onClick: () => copyText(value, label), 'aria-label': `${label}を写す` }, '写す'));
}

/** 読み込み中の表示を出してから、非同期の結果で置き換える。 */
export async function load(container, fn, { title } = {}) {
  replace(container, h('div', { class: 'muted', 'aria-busy': 'true' }, '読み込み中…'));
  try {
    const node = await fn();
    replace(container, node);
    return true;
  } catch (e) {
    if (e && e.status === 401) return false;
    replace(container, errorBox(e, { title }));
    return false;
  }
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
