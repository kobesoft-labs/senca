// 一覧のキーボード操作: J・K（または ↓・↑ を押す前に J/K で入る）で行を移り、Enter で開く。
// 行の「開く」リンク（data-row-link）にフォーカスを移すだけなので、Enter・読み上げはブラウザーのリンクのまま。
// 入力中・ダイアログ中は動かない（js/shortcuts.js の isTyping）。戻り値は解除する関数（画面の片付けで呼ぶ）。
//
//   const off = listKeys(tableContainer);   // 行の中の a[data-row-link] を順にたどる

import { isTyping } from '../shortcuts.js';

export function listKeys(container, { link = 'a[data-row-link]' } = {}) {
  const onKey = (e) => {
    if (e.ctrlKey || e.metaKey || e.altKey || e.defaultPrevented) return;
    if (isTyping(e.target) || document.querySelector('dialog[open]')) return;
    const links = [...container.querySelectorAll(link)].filter((a) => a.offsetParent !== null);
    if (!links.length) return;
    const cur = links.indexOf(document.activeElement);
    const inList = cur >= 0;
    let next = null;
    if (e.key === 'j' || (inList && e.key === 'ArrowDown')) next = inList ? Math.min(cur + 1, links.length - 1) : 0;
    else if (e.key === 'k' || (inList && e.key === 'ArrowUp')) next = inList ? Math.max(cur - 1, 0) : 0;
    if (next === null) return;
    e.preventDefault();
    for (const r of container.querySelectorAll('.kb-cursor')) r.classList.remove('kb-cursor');
    const a = links[next];
    a.focus();
    const row = a.closest('tr, li, .row-card');
    if (row) {
      row.classList.add('kb-cursor');
      a.addEventListener('blur', () => row.classList.remove('kb-cursor'), { once: true });
    }
    a.scrollIntoView({ block: 'nearest' });
  };
  document.addEventListener('keydown', onKey);
  return () => document.removeEventListener('keydown', onKey);
}
