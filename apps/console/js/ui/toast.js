// 画面の右下（狭い画面では下）の短い通知。操作（元に戻す・再試行）を 1 つ付けられる。
//
//   toast(t('streams.created'), 'ok')
//   toast(t('streams.deleted'), 'ok', { action: { label: t('common.undo'), onClick: undo } })
//   toast(msg, 'error', 9000)  // 3 つ目が数なら表示する時間（ミリ秒）

import { h } from '../lib/dom.js';
import { icon } from '../lib/icons.js';
import { t } from '../i18n/index.js';

const ICON = { info: 'info', ok: 'success', warn: 'warning', error: 'error' };
const MAX = 4;

/**
 * @param {string|Node} message
 * @param {'info'|'ok'|'warn'|'error'} [kind]
 * @param {number|{duration?: number, action?: {label: string, onClick: () => any}}} [opt]
 * @returns {() => void} すぐ消す関数
 */
export function toast(message, kind = 'info', opt = {}) {
  const o = typeof opt === 'number' ? { duration: opt } : opt || {};
  const box = document.getElementById('toasts');
  if (!box) return () => {};
  const duration = o.duration ?? (o.action ? 8000 : kind === 'error' ? 9000 : 5000);
  let timer = null;
  const el = h('div', { class: ['toast', kind], role: kind === 'error' ? 'alert' : 'status' });
  const dismiss = () => {
    clearTimeout(timer);
    if (!el.isConnected) return;
    el.classList.add('leaving');
    setTimeout(() => el.remove(), 200);
  };
  el.append(
    icon(ICON[kind] || 'info'),
    h('div', { class: 'toast-msg' }, message),
    h('div', { class: 'toast-actions' },
      o.action ? h('button', { type: 'button', class: 'link', onClick: () => { dismiss(); o.action.onClick(); } }, o.action.label) : null,
      h('button', { type: 'button', class: 'ghost icon-only sm', 'aria-label': t('common.dismiss'), onClick: dismiss }, icon('close'))));
  box.appendChild(el);
  while (box.children.length > MAX) box.firstChild.remove();
  const arm = () => { timer = setTimeout(dismiss, duration); };
  // 指を置いている間・フォーカスがある間は消さない
  el.addEventListener('mouseenter', () => clearTimeout(timer));
  el.addEventListener('mouseleave', arm);
  el.addEventListener('focusin', () => clearTimeout(timer));
  el.addEventListener('focusout', arm);
  arm();
  return dismiss;
}
