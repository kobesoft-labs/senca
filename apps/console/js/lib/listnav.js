// 一覧のキーボード操作: J・K で次・前の項目へ（フォーカスを移す）、Enter・O で開く。
// 入力中・ダイアログ中は効かない（shortcuts.js の onPageKey）。項目は `[data-nav-item]`（tabindex を付ける）。
//
//   const off = listNav(box, { onOpen: (el) => navigate(el.dataset.href) });   // 片付けで off()

import { onPageKey } from '../shortcuts.js';

/**
 * @param {HTMLElement} container 項目を含む要素（中身は作り直してよい）
 * @param {{selector?: string, onOpen?: (el: HTMLElement) => void}} [o] onOpen が無ければ、項目のリンク（a[href]）か項目そのものを押す
 * @returns {() => void} 解除する関数
 */
export function listNav(container, { selector = '[data-nav-item]', onOpen } = {}) {
  const items = () => [...container.querySelectorAll(selector)].filter((el) => el.offsetParent !== null);
  const move = (d) => {
    const list = items();
    if (!list.length) return;
    const cur = list.findIndex((el) => el === document.activeElement || el.contains(document.activeElement));
    const next = list[cur < 0 ? (d > 0 ? 0 : list.length - 1) : Math.max(0, Math.min(list.length - 1, cur + d))];
    next.focus();
    next.scrollIntoView({ block: 'nearest' });
  };
  const open = () => {
    const el = items().find((x) => x === document.activeElement);
    if (!el) return;
    if (onOpen) onOpen(el);
    else (el.matches('a[href]') ? el : el.querySelector('a[href]') || el).click();
  };
  const offs = [onPageKey('j', () => move(1)), onPageKey('k', () => move(-1)), onPageKey('o', open)];
  // Enter はフォーカスのある項目の上でだけ（リンク・ボタンは自分で動くので、それ以外の項目だけ）
  const onEnter = (e) => {
    if (e.key !== 'Enter' || e.defaultPrevented) return;
    const el = e.target.closest && e.target.closest(selector);
    if (!el || el !== e.target || el.matches('a[href], button')) return;
    e.preventDefault();
    open();
  };
  container.addEventListener('keydown', onEnter);
  return () => {
    offs.forEach((f) => f());
    container.removeEventListener('keydown', onEnter);
  };
}
