// コマンドパレットのあいまい検索（画面に依存しない。tools/interop/console.sh の C で試す）。
//
// 点数: 文字が順に現れれば一致。先頭・語の頭・続けての一致に加点、離れているほど減点。
// 大文字・小文字、全角・半角、ひらがな・カタカナの違いは無視する。

/** 比べるための正規化（NFKC・小文字・カタカナ → ひらがな）。 */
export function norm(s) {
  return String(s || '').normalize('NFKC').toLowerCase().replace(/[ァ-ヶ]/g, (c) => String.fromCharCode(c.charCodeAt(0) - 0x60));
}

/**
 * query が text にあいまいに一致すれば点数（大きいほど良い）と一致した位置、しなければ null。
 * @returns {{score: number, positions: number[]}|null}
 */
export function fuzzyMatch(query, text) {
  const q = norm(query).replace(/\s+/g, '');
  const s = norm(text);
  if (!q) return { score: 0, positions: [] };
  // そのまま含むなら強い一致
  const at = s.indexOf(q);
  if (at >= 0) {
    const word = at === 0 || /[\s\-_./:（(・]/.test(s[at - 1]);
    return { score: 1000 - at * 2 + (word ? 200 : 0) + (at === 0 ? 300 : 0) - (s.length - q.length), positions: Array.from({ length: q.length }, (_, i) => at + i) };
  }
  const positions = [];
  let score = 0;
  let prev = -2;
  let i = 0;
  for (const ch of q) {
    const j = s.indexOf(ch, i);
    if (j < 0) return null;
    positions.push(j);
    if (j === prev + 1) score += 15;
    if (j === 0 || /[\s\-_./:（(・]/.test(s[j - 1])) score += 10;
    score -= Math.min(j - i, 10);
    prev = j;
    i = j + 1;
  }
  return { score: score - (s.length - q.length) * 0.1, positions };
}

/**
 * 項目を絞って並べる。item.label（と item.keywords）で照合し、点数の高い順。query が空なら元の順。
 * @template T
 * @param {string} query
 * @param {(T & {label: string, keywords?: string[]})[]} items
 * @param {number} [limit]
 * @returns {(T & {score: number, positions: number[]})[]}
 */
export function fuzzyFilter(query, items, limit = 50) {
  if (!String(query || '').trim()) return items.slice(0, limit).map((it) => ({ ...it, score: 0, positions: [] }));
  const out = [];
  for (const it of items) {
    let best = fuzzyMatch(query, it.label);
    for (const k of it.keywords || []) {
      const m = fuzzyMatch(query, k);
      if (m && (!best || m.score - 50 > best.score)) best = { score: m.score - 50, positions: [] };
    }
    if (best) out.push({ ...it, score: best.score + (it.boost || 0), positions: best.positions });
  }
  out.sort((a, b) => b.score - a.score);
  return out.slice(0, limit);
}
