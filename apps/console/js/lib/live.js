// SSE のイベントで画面を読み直す。

import { on } from './bus.js';
import { debounce } from './dom.js';

/**
 * prefixes（`stream.` など）に合うイベント、または resync で fn を呼ぶ（まとめて 1 回）。
 * filter(ev) を渡すと、さらに絞れる（例: 自分の資源の ID だけ）。
 * @returns {() => void} 解除する関数
 */
export function refreshOn(prefixes, fn, { filter, wait = 400 } = {}) {
  const d = debounce(fn, wait);
  const offs = [
    on('sse', (ev) => {
      if (!prefixes.some((p) => ev.type.startsWith(p))) return;
      if (filter && !filter(ev)) return;
      d();
    }),
    on('resync', () => d()),
  ];
  return () => {
    d.cancel();
    offs.forEach((off) => off());
  };
}
