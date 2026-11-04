// 小さなグラフ: スパークライン（直近の値の動き）。数を読ませるものではなく「傾向」を見せる。
// 色は CSS（.spark）で決める。値が 2 つ未満なら平らな線を出す。

import { s } from '../lib/dom.js';

/**
 * @param {{values: number[], width?: number, height?: number, label?: string, max?: number, kind?: 'ok'|'warn'|'danger', testid?: string}} o
 * @returns {SVGSVGElement}
 */
export function sparkline({ values, width = 120, height = 32, label, max, kind, testid }) {
  const v = (values || []).filter((x) => typeof x === 'number' && Number.isFinite(x));
  const top = Math.max(max ?? 0, ...v, 1) * 1.1;
  const pad = 2;
  const n = v.length;
  const x = (i) => (n <= 1 ? width : pad + (i * (width - pad * 2)) / (n - 1));
  const y = (val) => height - pad - (val / top) * (height - pad * 2);
  const pts = n >= 2 ? v.map((val, i) => `${x(i).toFixed(1)},${y(val).toFixed(1)}`) : [`0,${height - pad}`, `${width},${height - pad}`];
  const area = `M${pts[0]} L${pts.slice(1).join(' L')} L${n >= 2 ? x(n - 1).toFixed(1) : width},${height} L${n >= 2 ? x(0).toFixed(1) : 0},${height} Z`;
  return s('svg', {
    class: ['spark', kind].filter(Boolean).join(' '), viewBox: `0 0 ${width} ${height}`, width, height, preserveAspectRatio: 'none',
    role: label ? 'img' : null, 'aria-label': label || null, 'aria-hidden': label ? null : 'true', 'data-testid': testid, 'data-points': String(n),
  },
  s('path', { class: 'spark-area', d: area }),
  s('polyline', { class: 'spark-line', points: pts.join(' ') }),
  n >= 2 ? s('circle', { class: 'spark-dot', cx: x(n - 1).toFixed(1), cy: y(v[n - 1]).toFixed(1), r: 2.5 }) : null);
}
