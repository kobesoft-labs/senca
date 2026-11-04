// パイプラインの状態の見せ方（一覧・編集・スイッチャーで同じもの）。

import { statusPill } from '../ui/index.js';

/** 観測した状態（observed_state）のピル。data-state に生の値（running など）を入れる（試験が選ぶ）。 */
export function observedBadge(state) {
  const b = statusPill(state || 'unknown');
  b.dataset.state = state || '';
  return b;
}
