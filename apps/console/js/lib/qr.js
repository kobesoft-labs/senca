// QR コードを作る（認証アプリの登録の otpauth:// を、外のサービスを使わずにこの場で絵にする）。
//
// - 符号化はバイトモード（UTF-8）1 つ、誤り訂正は M、型番 1〜40 から入る最小のもの。マスクは 8 つから減点の少ないもの。
// - 作り方は ISO/IEC 18004 のとおり（Reed-Solomon は GF(256)・原始多項式 0x11D、形式情報は BCH(15,5)、型番情報は BCH(18,6)）。
// - 画面に依存しない（qrMatrix は node でも動く）。絵にするのは qrSvg。
// - 試験: tools/interop/console.sh の C が、作った行列を PNG にして tools/interop/qr_decode.py（別に書いた復号器）で読み戻す。

// 型番ごとのブロックあたりの誤り訂正の語数と、ブロックの数（誤り訂正 M）。添字 0 は使わない。
const ECC_PER_BLOCK_M = [-1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26, 30, 22, 22, 24, 24, 28, 28, 26, 26, 26, 26, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28, 28];
const BLOCKS_M = [-1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5, 5, 8, 9, 9, 10, 10, 11, 13, 14, 16, 17, 17, 18, 20, 21, 23, 25, 26, 28, 29, 31, 33, 35, 37, 38, 40, 43, 45, 47, 49];
const ECL_M_BITS = 0; // 形式情報での M（L=1・M=0・Q=3・H=2）

/** 型番のデータ（機能パターンを除いたモジュールの数）。 */
function rawModules(ver) {
  let n = (16 * ver + 128) * ver + 64;
  if (ver >= 2) {
    const align = Math.floor(ver / 7) + 2;
    n -= (25 * align - 10) * align - 55;
    if (ver >= 7) n -= 36;
  }
  return n;
}

const rawCodewords = (ver) => Math.floor(rawModules(ver) / 8);
const dataCodewords = (ver) => rawCodewords(ver) - ECC_PER_BLOCK_M[ver] * BLOCKS_M[ver];

function alignPositions(ver) {
  if (ver === 1) return [];
  const size = ver * 4 + 17;
  const n = Math.floor(ver / 7) + 2;
  const step = ver === 32 ? 26 : Math.ceil((ver * 4 + 4) / (n * 2 - 2)) * 2;
  const out = [6];
  for (let pos = size - 7; out.length < n; pos -= step) out.splice(1, 0, pos);
  return out;
}

// ---------------------------------------------------------------- Reed-Solomon（GF(256)）
function gfMul(x, y) {
  let z = 0;
  for (let i = 7; i >= 0; i--) {
    z = (z << 1) ^ ((z >>> 7) * 0x11d);
    z ^= ((y >>> i) & 1) * x;
  }
  return z & 0xff;
}

function rsDivisor(degree) {
  const r = new Array(degree).fill(0);
  r[degree - 1] = 1;
  let root = 1;
  for (let i = 0; i < degree; i++) {
    for (let j = 0; j < degree; j++) {
      r[j] = gfMul(r[j], root);
      if (j + 1 < degree) r[j] ^= r[j + 1];
    }
    root = gfMul(root, 0x02);
  }
  return r;
}

function rsRemainder(data, divisor) {
  const r = new Array(divisor.length).fill(0);
  for (const b of data) {
    const f = b ^ r.shift();
    r.push(0);
    divisor.forEach((c, i) => { r[i] ^= gfMul(c, f); });
  }
  return r;
}

// ---------------------------------------------------------------- 符号化
function utf8(text) {
  if (typeof TextEncoder !== 'undefined') return [...new TextEncoder().encode(text)];
  return [...unescape(encodeURIComponent(text))].map((c) => c.charCodeAt(0));
}

function encodeData(bytes, ver) {
  const bits = [];
  const put = (v, n) => { for (let i = n - 1; i >= 0; i--) bits.push((v >>> i) & 1); };
  put(0b0100, 4);
  put(bytes.length, ver <= 9 ? 8 : 16);
  for (const b of bytes) put(b, 8);
  const cap = dataCodewords(ver) * 8;
  put(0, Math.min(4, cap - bits.length));
  put(0, (8 - (bits.length % 8)) % 8);
  for (let pad = 0xec; bits.length < cap; pad ^= 0xec ^ 0x11) put(pad, 8);
  const out = [];
  for (let i = 0; i < bits.length; i += 8) out.push(bits.slice(i, i + 8).reduce((a, b) => (a << 1) | b, 0));
  return out;
}

function interleave(data, ver) {
  const nBlocks = BLOCKS_M[ver];
  const eccLen = ECC_PER_BLOCK_M[ver];
  const raw = rawCodewords(ver);
  const nShort = nBlocks - (raw % nBlocks);
  const shortLen = Math.floor(raw / nBlocks);
  const div = rsDivisor(eccLen);
  const blocks = [];
  for (let i = 0, k = 0; i < nBlocks; i++) {
    const dat = data.slice(k, k + shortLen - eccLen + (i < nShort ? 0 : 1));
    k += dat.length;
    const ecc = rsRemainder(dat, div);
    if (i < nShort) dat.push(0);
    blocks.push(dat.concat(ecc));
  }
  const out = [];
  for (let i = 0; i < blocks[0].length; i++) {
    blocks.forEach((b, j) => { if (i !== shortLen - eccLen || j >= nShort) out.push(b[i]); });
  }
  return out;
}

// ---------------------------------------------------------------- 配置
const MASKS = [
  (x, y) => (x + y) % 2 === 0,
  (x, y) => y % 2 === 0,
  (x) => x % 3 === 0,
  (x, y) => (x + y) % 3 === 0,
  (x, y) => (Math.floor(x / 3) + Math.floor(y / 2)) % 2 === 0,
  (x, y) => ((x * y) % 2) + ((x * y) % 3) === 0,
  (x, y) => (((x * y) % 2) + ((x * y) % 3)) % 2 === 0,
  (x, y) => (((x + y) % 2) + ((x * y) % 3)) % 2 === 0,
];

function build(ver, codewords, mask) {
  const size = ver * 4 + 17;
  const m = Array.from({ length: size }, () => new Array(size).fill(false));
  const fn = Array.from({ length: size }, () => new Array(size).fill(false));
  const set = (x, y, dark) => { m[y][x] = dark; fn[y][x] = true; };

  for (let i = 0; i < size; i++) { set(6, i, i % 2 === 0); set(i, 6, i % 2 === 0); }
  for (const [cx, cy] of [[3, 3], [size - 4, 3], [3, size - 4]]) {
    for (let dy = -4; dy <= 4; dy++) {
      for (let dx = -4; dx <= 4; dx++) {
        const x = cx + dx;
        const y = cy + dy;
        if (x < 0 || y < 0 || x >= size || y >= size) continue;
        const d = Math.max(Math.abs(dx), Math.abs(dy));
        set(x, y, d !== 2 && d !== 4);
      }
    }
  }
  const al = alignPositions(ver);
  al.forEach((ay, i) => al.forEach((ax, j) => {
    if ((i === 0 && j === 0) || (i === 0 && j === al.length - 1) || (i === al.length - 1 && j === 0)) return;
    for (let dy = -2; dy <= 2; dy++) for (let dx = -2; dx <= 2; dx++) set(ax + dx, ay + dy, Math.max(Math.abs(dx), Math.abs(dy)) !== 1);
  }));
  const drawFormat = (msk) => {
    const data = (ECL_M_BITS << 3) | msk;
    let rem = data;
    for (let i = 0; i < 10; i++) rem = (rem << 1) ^ ((rem >>> 9) * 0x537);
    const bits = ((data << 10) | rem) ^ 0x5412;
    const bit = (i) => ((bits >>> i) & 1) === 1;
    for (let i = 0; i <= 5; i++) set(8, i, bit(i));
    set(8, 7, bit(6));
    set(8, 8, bit(7));
    set(7, 8, bit(8));
    for (let i = 9; i < 15; i++) set(14 - i, 8, bit(i));
    for (let i = 0; i < 8; i++) set(size - 1 - i, 8, bit(i));
    for (let i = 8; i < 15; i++) set(8, size - 15 + i, bit(i));
    set(8, size - 8, true);
  };
  drawFormat(0); // 場所を確保する（後で本物を書く）
  if (ver >= 7) {
    let rem = ver;
    for (let i = 0; i < 12; i++) rem = (rem << 1) ^ ((rem >>> 11) * 0x1f25);
    const bits = (ver << 12) | rem;
    for (let i = 0; i < 18; i++) {
      const b = ((bits >>> i) & 1) === 1;
      const a = size - 11 + (i % 3);
      const c = Math.floor(i / 3);
      set(a, c, b);
      set(c, a, b);
    }
  }
  // データ（右下から 2 列ずつ、上下に往復）
  let i = 0;
  for (let right = size - 1; right >= 1; right -= 2) {
    if (right === 6) right = 5;
    for (let v = 0; v < size; v++) {
      for (let j = 0; j < 2; j++) {
        const x = right - j;
        const y = ((right + 1) & 2) === 0 ? size - 1 - v : v;
        if (!fn[y][x] && i < codewords.length * 8) {
          m[y][x] = ((codewords[i >>> 3] >>> (7 - (i & 7))) & 1) === 1;
          i++;
        }
      }
    }
  }
  const f = MASKS[mask];
  for (let y = 0; y < size; y++) for (let x = 0; x < size; x++) if (!fn[y][x] && f(x, y)) m[y][x] = !m[y][x];
  drawFormat(mask);
  return m;
}

/** 読み取りにくさの減点（規格の 4 つの決まり）。小さいほどよい。 */
function penalty(m) {
  const n = m.length;
  let p = 0;
  const lines = [];
  for (let y = 0; y < n; y++) lines.push(m[y]);
  for (let x = 0; x < n; x++) lines.push(m.map((row) => row[x]));
  const finder = [true, false, true, true, true, false, true];
  for (const line of lines) {
    let run = 1;
    for (let i = 1; i <= n; i++) {
      if (i < n && line[i] === line[i - 1]) run++;
      else { if (run >= 5) p += 3 + (run - 5); run = 1; }
    }
    // 位置検出パターンに似た並び（前後どちらかに 4 つの明）
    const at = (i) => (i < 0 || i >= n ? false : line[i]);
    for (let i = 0; i + 7 <= n; i++) {
      if (!finder.every((v, k) => line[i + k] === v)) continue;
      const before = [1, 2, 3, 4].every((k) => !at(i - k));
      const after = [7, 8, 9, 10].every((k) => !at(i + k));
      if (before || after) p += 40;
    }
  }
  for (let y = 0; y + 1 < n; y++) {
    for (let x = 0; x + 1 < n; x++) {
      const c = m[y][x];
      if (c === m[y][x + 1] && c === m[y + 1][x] && c === m[y + 1][x + 1]) p += 3;
    }
  }
  let dark = 0;
  for (const row of m) for (const v of row) if (v) dark++;
  p += Math.floor(Math.abs((dark * 100) / (n * n) - 50) / 5) * 10;
  return p;
}

/**
 * 文字列を QR コードのモジュールの行列にする。入らなければ Error。
 * @param {string} text
 * @param {{mask?: number, minVersion?: number}} [o] mask を決めると減点で選ばない（試験用）
 * @returns {{version: number, size: number, modules: boolean[][]}} modules[y][x] が true なら暗
 */
export function qrMatrix(text, { mask, minVersion = 1 } = {}) {
  const bytes = utf8(String(text));
  let ver = Math.max(1, minVersion);
  for (; ver <= 40; ver++) {
    if (4 + (ver <= 9 ? 8 : 16) + bytes.length * 8 <= dataCodewords(ver) * 8) break;
  }
  if (ver > 40) throw new Error('qr: text too long');
  const cw = interleave(encodeData(bytes, ver), ver);
  let best = null;
  for (const k of mask === undefined ? [0, 1, 2, 3, 4, 5, 6, 7] : [mask]) {
    const mm = build(ver, cw, k);
    const p = mask === undefined ? penalty(mm) : 0;
    if (!best || p < best.p) best = { p, mm };
  }
  return { version: ver, size: ver * 4 + 17, modules: best.mm };
}

/**
 * 行列を SVG の path の d にする（周りに quiet モジュールの余白、1 モジュール = 1 単位）。
 * @returns {{d: string, viewBox: number}}
 */
export function qrPath(matrix, quiet = 4) {
  const parts = [];
  matrix.modules.forEach((row, y) => {
    let x = 0;
    while (x < row.length) {
      if (!row[x]) { x++; continue; }
      let w = 1;
      while (x + w < row.length && row[x + w]) w++;
      parts.push(`M${x + quiet} ${y + quiet}h${w}v1h-${w}z`);
      x += w;
    }
  });
  return { d: parts.join(''), viewBox: matrix.size + quiet * 2 };
}
