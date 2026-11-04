// 一覧のキーボード操作: J・↓ で次、K・↑ で前、Enter・O で開く。行のどこを押しても開く。
// 入力中・ダイアログ中は動かない（shortcuts.js の isTyping）。↑↓ は行にフォーカスがあるときだけ使う（ページのスクロールを奪わない）。
//
//   const off = listNav(container, { rows: () => [...container.querySelectorAll('tbody tr')], open: (tr) => navigate(…) });

import { isTyping } from '../shortcuts.js';

/**
 * @param {HTMLElement} root 行を含む要素（中身は作り直されてもよい）
 * @param {{rows: () => HTMLElement[], open: (row: HTMLElement) => void, key?: (row: HTMLElement) => string}} o
 * @returns {() => void} 解除する関数
 */
export function listNav(root, { rows, open, key = (r) => r.dataset.key || '' }) {
  let activeKey = null;
  const visible = () => rows().filter((r) => r.offsetParent !== null);
  const mark = (focus) => {
    let found = null;
    for (const r of rows()) {
      const on = activeKey !== null && key(r) === activeKey;
      if (on !== r.hasAttribute('data-active')) r.toggleAttribute('data-active', on);
      const ti = on ? '0' : '-1';
      if (r.getAttribute('tabindex') !== ti) r.setAttribute('tabindex', ti);
      if (on) found = r;
    }
    if (found && focus) {
      found.focus({ preventScroll: true });
      found.scrollIntoView({ block: 'nearest' });
    }
    return found;
  };
  const move = (d) => {
    const list = visible();
    if (!list.length) return;
    let i = list.findIndex((r) => key(r) === activeKey);
    i = i < 0 ? (d > 0 ? 0 : list.length - 1) : Math.max(0, Math.min(list.length - 1, i + d));
    activeKey = key(list[i]);
    mark(true);
  };
  const onKey = (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey || e.defaultPrevented) return;
    if (isTyping(e.target) || document.querySelector('dialog[open]') || !root.isConnected) return;
    const onRow = !!(e.target && e.target.hasAttribute && e.target.hasAttribute('data-active'));
    if (e.key === 'j' || (e.key === 'ArrowDown' && onRow)) { e.preventDefault(); move(1); return; }
    if (e.key === 'k' || (e.key === 'ArrowUp' && onRow)) { e.preventDefault(); move(-1); return; }
    if ((e.key === 'Enter' && onRow) || (e.key === 'o' && activeKey !== null)) {
      const r = mark(false);
      if (r) { e.preventDefault(); open(r); }
    }
  };
  const onClick = (e) => {
    if (e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey) return;
    if (e.target.closest('a, button, input, select, textarea, label, video, [role="menu"]')) return;
    const r = rows().find((x) => x.contains(e.target));
    if (!r) return;
    if (window.getSelection && String(window.getSelection())) return;
    activeKey = key(r);
    mark(false);
    open(r);
  };
  document.addEventListener('keydown', onKey);
  root.addEventListener('click', onClick);
  // 作り直された行にも選んでいる印を付け直す
  const observer = new MutationObserver(() => { if (activeKey !== null) mark(false); });
  observer.observe(root, { childList: true, subtree: true });
  return () => {
    document.removeEventListener('keydown', onKey);
    root.removeEventListener('click', onClick);
    observer.disconnect();
  };
}
