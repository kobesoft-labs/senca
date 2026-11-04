// パスワードの強さの目安（長さと文字の種類から。外の単語の一覧は使わない）と、仮のパスワードの生成。
// ノードの決まり（12〜256 文字、利用者名・メールアドレスと同じものは不可）は ADR-0017。画面に依存しない。

export const MIN_LENGTH = 12;
export const MAX_LENGTH = 256;

const CLASSES = [
  [/[a-z]/, 26],
  [/[A-Z]/, 26],
  [/[0-9]/, 10],
  [/[^A-Za-z0-9\s\u0080-￿]/, 32],
  [/\s/, 1],
  [/[\u0080-￿]/, 100],
];

/** 同じ文字の繰り返し・並び（abcd・1234・4321）の長さを差し引いた「実質の長さ」。 */
function effectiveLength(chars) {
  let len = 0;
  for (let i = 0; i < chars.length; i++) {
    const c = chars[i].codePointAt(0);
    const p = i > 0 ? chars[i - 1].codePointAt(0) : null;
    const pp = i > 1 ? chars[i - 2].codePointAt(0) : null;
    const repeat = p === c;
    const seq = p !== null && pp !== null && c - p === p - pp && Math.abs(c - p) === 1;
    len += repeat || seq ? 0.25 : 1;
  }
  return len;
}

/**
 * 強さを見積もる。
 * @param {string} pw
 * @param {{username?: string, email?: string}} [ctx]
 * @returns {{score: 0|1|2|3|4, ok: boolean, reason: 'empty'|'short'|'long'|'sameAsName'|null, missing: number, bits: number, tips: string[]}}
 *   score: 0 = 使えない、1 = 弱い、2 = ふつう、3 = 強い、4 = とても強い。tips は改善の鍵（'length'・'variety'・'pattern'）
 */
export function passwordStrength(pw, { username, email } = {}) {
  const chars = [...(pw || '')];
  const n = chars.length;
  if (!n) return { score: 0, ok: false, reason: 'empty', missing: MIN_LENGTH, bits: 0, tips: ['length'] };
  const lower = pw.toLowerCase();
  if ((username && lower === String(username).toLowerCase()) || (email && lower === String(email).toLowerCase())) {
    return { score: 0, ok: false, reason: 'sameAsName', missing: 0, bits: 0, tips: [] };
  }
  if (n > MAX_LENGTH) return { score: 0, ok: false, reason: 'long', missing: 0, bits: 0, tips: [] };
  const pool = CLASSES.reduce((a, [re, size]) => a + (re.test(pw) ? size : 0), 0) || 1;
  const kinds = CLASSES.filter(([re]) => re.test(pw)).length;
  const eff = effectiveLength(chars);
  // 利用者名を含むときは、その分を数えない
  let bits = eff * Math.log2(pool);
  if (username && String(username).length >= 3 && lower.includes(String(username).toLowerCase())) bits -= String(username).length * Math.log2(pool) * 0.75;
  bits = Math.max(0, bits);
  const tips = [];
  if (n < 16) tips.push('length');
  if (kinds < 3 && n < 20) tips.push('variety');
  if (eff < n * 0.8) tips.push('pattern');
  if (n < MIN_LENGTH) return { score: 0, ok: false, reason: 'short', missing: MIN_LENGTH - n, bits, tips };
  const score = bits < 50 ? 1 : bits < 70 ? 2 : bits < 95 ? 3 : 4;
  return { score, ok: true, reason: null, missing: 0, bits, tips };
}

// 見間違えやすい文字（0・O・1・l・I）を除いた英数字
const ALPHABET = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789';

/** 仮のパスワード（既定 20 文字、crypto.getRandomValues。偏りが出ないように捨てて選び直す）。 */
export function generatePassword(length = 20) {
  const out = [];
  const limit = 256 - (256 % ALPHABET.length);
  while (out.length < length) {
    const buf = crypto.getRandomValues(new Uint8Array(length * 2));
    for (const b of buf) {
      if (b < limit && out.length < length) out.push(ALPHABET[b % ALPHABET.length]);
    }
  }
  return out.join('');
}
