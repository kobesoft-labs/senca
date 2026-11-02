//! QR コードの生成（ISO/IEC 18004）。管理画面・API がスマホ向けに送り先の設定を出すのに使う（REQ-009 R9-4）。
//!
//! 対象: 数字モードとバイトモード（1 セグメント）、誤り訂正レベル M、型番 1〜10、マスクは 8 種から失点が最小のもの。
//! 型番 10・M のバイトモードで 213 バイトまで入る。
//!
//! 座標は `x` が列、`y` が行（左上が原点）。

use crate::png;
use std::fmt;

/// QR の生成の誤り。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrError {
    /// 型番 10・M に入らない。
    TooLong,
    /// マスク番号が 0〜7 でない。
    BadMask,
}

impl fmt::Display for QrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QrError::TooLong => f.write_str("data too long for QR version 10-M"),
            QrError::BadMask => f.write_str("mask must be 0..7"),
        }
    }
}

impl std::error::Error for QrError {}

/// 対応する最大の型番。
pub const MAX_VERSION: u8 = 10;

/// 型番ごとの（総コード語数, ブロックあたりの誤り訂正コード語数, [(ブロック数, データコード語数)]）。レベル M。
/// （ブロック数, データコード語数）の組。
type Group = (usize, usize);

const BLOCKS_M: [(usize, usize, [Group; 2]); 10] = [
    (26, 10, [(1, 16), (0, 0)]),
    (44, 16, [(1, 28), (0, 0)]),
    (70, 26, [(1, 44), (0, 0)]),
    (100, 18, [(2, 32), (0, 0)]),
    (134, 24, [(2, 43), (0, 0)]),
    (172, 16, [(4, 27), (0, 0)]),
    (196, 18, [(4, 31), (0, 0)]),
    (242, 22, [(2, 38), (2, 39)]),
    (292, 22, [(3, 36), (2, 37)]),
    (346, 26, [(4, 43), (1, 44)]),
];

/// 位置合わせパターンの中心（型番 2〜10）。
const ALIGN: [&[usize]; 10] = [
    &[],
    &[6, 18],
    &[6, 22],
    &[6, 26],
    &[6, 30],
    &[6, 34],
    &[6, 22, 38],
    &[6, 24, 42],
    &[6, 26, 46],
    &[6, 28, 50],
];

/// レベル M の 2 bit 表示。
const ECL_M_BITS: u32 = 0b00;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Numeric,
    Byte,
}

impl Mode {
    fn indicator(self) -> u32 {
        match self {
            Mode::Numeric => 0b0001,
            Mode::Byte => 0b0100,
        }
    }

    fn count_bits(self, version: u8) -> usize {
        match (self, version) {
            (Mode::Numeric, 1..=9) => 10,
            (Mode::Numeric, _) => 12,
            (Mode::Byte, 1..=9) => 8,
            (Mode::Byte, _) => 16,
        }
    }

    fn data_bits(self, n: usize) -> usize {
        match self {
            Mode::Numeric => n / 3 * 10 + [0, 4, 7][n % 3],
            Mode::Byte => n * 8,
        }
    }
}

fn data_codewords(version: u8) -> usize {
    let (_, _, g) = BLOCKS_M[version as usize - 1];
    g[0].0 * g[0].1 + g[1].0 * g[1].1
}

struct BitBuf(Vec<bool>);

impl BitBuf {
    fn push(&mut self, v: u32, n: usize) {
        for i in (0..n).rev() {
            self.0.push((v >> i) & 1 != 0);
        }
    }
}

/// データコード語（誤り訂正前）を作る。
fn encode_data(data: &[u8], mode: Mode, version: u8) -> Vec<u8> {
    let cap = data_codewords(version);
    let mut bb = BitBuf(Vec::with_capacity(cap * 8));
    bb.push(mode.indicator(), 4);
    bb.push(data.len() as u32, mode.count_bits(version));
    match mode {
        Mode::Numeric => {
            for g in data.chunks(3) {
                let v = g.iter().fold(0u32, |a, &d| a * 10 + (d - b'0') as u32);
                bb.push(v, [0, 4, 7, 10][g.len()]);
            }
        }
        Mode::Byte => {
            for &b in data {
                bb.push(b as u32, 8);
            }
        }
    }
    // 終端（最大 4 bit）とバイト境界
    let term = (cap * 8 - bb.0.len()).min(4);
    bb.push(0, term);
    let pad = (8 - bb.0.len() % 8) % 8;
    bb.push(0, pad);
    let mut out: Vec<u8> =
        bb.0.chunks(8)
            .map(|c| c.iter().fold(0u8, |a, &b| a << 1 | u8::from(b)))
            .collect();
    let mut fill = [0xEC, 0x11].into_iter().cycle();
    while out.len() < cap {
        out.push(fill.next().unwrap_or(0xEC));
    }
    out
}

// ---- Reed-Solomon（GF(256)、原始多項式 0x11D）----

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut r = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            r ^= a;
        }
        let hi = a & 0x80 != 0;
        a <<= 1;
        if hi {
            a ^= 0x1D;
        }
        b >>= 1;
    }
    r
}

/// 生成多項式 (x - α^0)(x - α^1)…(x - α^{n-1}) の係数（最高次の 1 を除く、高次から）。
fn rs_divisor(n: usize) -> Vec<u8> {
    let mut d = vec![0u8; n];
    d[n - 1] = 1;
    let mut root = 1u8;
    for _ in 0..n {
        for j in 0..n {
            d[j] = gf_mul(d[j], root);
            if j + 1 < n {
                d[j] ^= d[j + 1];
            }
        }
        root = gf_mul(root, 2);
    }
    d
}

fn rs_remainder(data: &[u8], div: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; div.len()];
    for &b in data {
        let f = b ^ r[0];
        r.rotate_left(1);
        let last = r.len() - 1;
        r[last] = 0;
        for (x, &y) in r.iter_mut().zip(div) {
            *x ^= gf_mul(y, f);
        }
    }
    r
}

/// 誤り訂正を付けて並べ替えた（インターリーブした）最終のコード語列。
fn add_ecc_and_interleave(data: &[u8], version: u8) -> Vec<u8> {
    let (total, ec, groups) = BLOCKS_M[version as usize - 1];
    let div = rs_divisor(ec);
    let mut blocks: Vec<(&[u8], Vec<u8>)> = Vec::new();
    let mut off = 0;
    for (count, len) in groups {
        for _ in 0..count {
            let d = &data[off..off + len];
            off += len;
            blocks.push((d, rs_remainder(d, &div)));
        }
    }
    let max_len = blocks.iter().map(|b| b.0.len()).max().unwrap_or(0);
    let mut out = Vec::with_capacity(total);
    for i in 0..max_len {
        for (d, _) in &blocks {
            if let Some(&b) = d.get(i) {
                out.push(b);
            }
        }
    }
    for i in 0..ec {
        for (_, e) in &blocks {
            out.push(e[i]);
        }
    }
    debug_assert_eq!(out.len(), total);
    out
}

/// 15 bit の形式情報（BCH、マスク 0x5412 適用後）。
pub fn format_bits(mask: u8) -> u32 {
    let data = (ECL_M_BITS << 3 | mask as u32) & 0x1F;
    let mut rem = data;
    for _ in 0..10 {
        rem = (rem << 1) ^ ((rem >> 9) * 0x537);
    }
    (data << 10 | rem) ^ 0x5412
}

/// 18 bit の型番情報（型番 7 以上）。
pub fn version_bits(version: u8) -> u32 {
    let v = version as u32;
    let mut rem = v;
    for _ in 0..12 {
        rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
    }
    v << 12 | rem
}

fn mask_bit(mask: u8, x: usize, y: usize) -> bool {
    match mask {
        0 => (x + y).is_multiple_of(2),
        1 => y.is_multiple_of(2),
        2 => x.is_multiple_of(3),
        3 => (x + y).is_multiple_of(3),
        4 => (x / 3 + y / 2).is_multiple_of(2),
        5 => x * y % 2 + x * y % 3 == 0,
        6 => (x * y % 2 + x * y % 3).is_multiple_of(2),
        _ => ((x + y) % 2 + x * y % 3).is_multiple_of(2),
    }
}

/// QR コード（モジュールの並び）。
#[derive(Clone, PartialEq, Eq)]
pub struct QrCode {
    version: u8,
    mask: u8,
    size: usize,
    modules: Vec<bool>,
    function: Vec<bool>,
}

impl fmt::Debug for QrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "QrCode(version {}, mask {}, {}x{})",
            self.version, self.mask, self.size, self.size
        )
    }
}

impl QrCode {
    /// 文字列を符号化する（すべて数字なら数字モード、他はバイトモード（UTF-8））。型番は入る最小のもの。
    pub fn encode(text: &str) -> Result<QrCode, QrError> {
        Self::encode_with(text.as_bytes(), None)
    }

    /// バイト列を符号化する。`mask` を指定しなければ失点が最小のマスクを選ぶ。
    pub fn encode_with(data: &[u8], mask: Option<u8>) -> Result<QrCode, QrError> {
        if mask.is_some_and(|m| m > 7) {
            return Err(QrError::BadMask);
        }
        let mode = if !data.is_empty() && data.iter().all(u8::is_ascii_digit) {
            Mode::Numeric
        } else {
            Mode::Byte
        };
        let version = (1..=MAX_VERSION)
            .find(|&v| {
                data.len() < 1 << mode.count_bits(v)
                    && 4 + mode.count_bits(v) + mode.data_bits(data.len()) <= data_codewords(v) * 8
            })
            .ok_or(QrError::TooLong)?;
        let codewords = add_ecc_and_interleave(&encode_data(data, mode, version), version);
        let mut q = QrCode::blank(version);
        q.draw_function_patterns();
        q.draw_codewords(&codewords);
        let mask = match mask {
            Some(m) => m,
            None => (0..8u8)
                .min_by_key(|&m| {
                    let mut t = q.clone();
                    t.apply_mask(m);
                    t.draw_format(m);
                    t.penalty()
                })
                .unwrap_or(0),
        };
        q.apply_mask(mask);
        q.draw_format(mask);
        q.mask = mask;
        Ok(q)
    }

    fn blank(version: u8) -> QrCode {
        let size = 17 + 4 * version as usize;
        QrCode {
            version,
            mask: 0,
            size,
            modules: vec![false; size * size],
            function: vec![false; size * size],
        }
    }

    pub fn version(&self) -> u8 {
        self.version
    }

    pub fn mask(&self) -> u8 {
        self.mask
    }

    /// 1 辺のモジュール数（静穏域を含まない）。
    pub fn size(&self) -> usize {
        self.size
    }

    /// (x, y) のモジュールが暗いか。範囲外は明（静穏域）。
    pub fn get(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.modules[y * self.size + x]
    }

    fn set_fn(&mut self, x: usize, y: usize, dark: bool) {
        let i = y * self.size + x;
        self.modules[i] = dark;
        self.function[i] = true;
    }

    /// 機能パターン（データを置かない場所）か。
    pub fn is_function(&self, x: usize, y: usize) -> bool {
        self.function[y * self.size + x]
    }

    fn draw_function_patterns(&mut self) {
        let n = self.size;
        // タイミングパターン
        for i in 0..n {
            self.set_fn(6, i, i % 2 == 0);
            self.set_fn(i, 6, i % 2 == 0);
        }
        // 位置検出パターン（分離パターン込み）
        for (cx, cy) in [(3, 3), (n - 4, 3), (3, n - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                    if (0..n as i32).contains(&x) && (0..n as i32).contains(&y) {
                        let d = dx.abs().max(dy.abs());
                        self.set_fn(x as usize, y as usize, d != 2 && d != 4);
                    }
                }
            }
        }
        // 位置合わせパターン（位置検出パターンと重なる 3 隅は除く）
        let al = ALIGN[self.version as usize - 1];
        let last = al.len().saturating_sub(1);
        for (i, &ay) in al.iter().enumerate() {
            for (j, &ax) in al.iter().enumerate() {
                if (i == 0 && j == 0) || (i == 0 && j == last) || (i == last && j == 0) {
                    continue;
                }
                for dy in -2i32..=2 {
                    for dx in -2i32..=2 {
                        let d = dx.abs().max(dy.abs());
                        self.set_fn((ax as i32 + dx) as usize, (ay as i32 + dy) as usize, d != 1);
                    }
                }
            }
        }
        // 形式情報の場所を確保（値は後で）と型番情報
        self.draw_format(0);
        if self.version >= 7 {
            let bits = version_bits(self.version);
            for i in 0..18 {
                let dark = (bits >> i) & 1 != 0;
                let a = n - 11 + i % 3;
                let b = i / 3;
                self.set_fn(a, b, dark);
                self.set_fn(b, a, dark);
            }
        }
    }

    fn draw_format(&mut self, mask: u8) {
        let bits = format_bits(mask);
        let bit = |i: usize| (bits >> i) & 1 != 0;
        let n = self.size;
        // 1 つ目（左上）
        for i in 0..=5 {
            self.set_fn(8, i, bit(i));
        }
        self.set_fn(8, 7, bit(6));
        self.set_fn(8, 8, bit(7));
        self.set_fn(7, 8, bit(8));
        for i in 9..15 {
            self.set_fn(14 - i, 8, bit(i));
        }
        // 2 つ目（右上・左下）
        for i in 0..8 {
            self.set_fn(n - 1 - i, 8, bit(i));
        }
        for i in 8..15 {
            self.set_fn(8, n - 15 + i, bit(i));
        }
        // 常に暗いモジュール
        self.set_fn(8, n - 8, true);
    }

    fn draw_codewords(&mut self, data: &[u8]) {
        let n = self.size;
        let mut i = 0usize;
        let mut right = n as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..n {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let upward = (right + 1) & 2 == 0;
                    let y = if upward { n - 1 - vert } else { vert };
                    if !self.function[y * n + x] && i < data.len() * 8 {
                        self.modules[y * n + x] = (data[i >> 3] >> (7 - (i & 7))) & 1 != 0;
                        i += 1;
                    }
                    // 残りのビット（remainder bits）は明のまま
                }
            }
            right -= 2;
        }
    }

    fn apply_mask(&mut self, mask: u8) {
        let n = self.size;
        for y in 0..n {
            for x in 0..n {
                if !self.function[y * n + x] && mask_bit(mask, x, y) {
                    self.modules[y * n + x] ^= true;
                }
            }
        }
    }

    /// 失点（ISO/IEC 18004 7.8.3）。
    fn penalty(&self) -> u32 {
        let n = self.size;
        let mut p = 0u32;
        let line = |get: &dyn Fn(usize) -> bool, p: &mut u32| {
            let mut run = 1;
            for i in 1..n {
                if get(i) == get(i - 1) {
                    run += 1;
                } else {
                    if run >= 5 {
                        *p += 3 + (run - 5);
                    }
                    run = 1;
                }
            }
            if run >= 5 {
                *p += 3 + (run - 5);
            }
            // 1:1:3:1:1 の並びの片側に 4 つの明
            const A: [bool; 11] = [
                true, false, true, true, true, false, true, false, false, false, false,
            ];
            for i in 0..n.saturating_sub(10) {
                let w: [bool; 11] = std::array::from_fn(|k| get(i + k));
                if w == A || w.iter().rev().eq(A.iter()) {
                    *p += 40;
                }
            }
        };
        for y in 0..n {
            line(&|x| self.get(x, y), &mut p);
        }
        for x in 0..n {
            line(&|y| self.get(x, y), &mut p);
        }
        for y in 0..n - 1 {
            for x in 0..n - 1 {
                let c = self.get(x, y);
                if c == self.get(x + 1, y) && c == self.get(x, y + 1) && c == self.get(x + 1, y + 1)
                {
                    p += 3;
                }
            }
        }
        let dark = self.modules.iter().filter(|&&m| m).count() as i64;
        let total = (n * n) as i64;
        let k = ((dark * 20 - total * 10).abs() + total - 1) / total - 1;
        p + k.max(0) as u32 * 10
    }

    /// SVG（`border` は静穏域のモジュール数、規格は 4 以上）。1 モジュール = 1 単位。
    pub fn to_svg(&self, border: u32) -> String {
        let dim = self.size as u32 + 2 * border;
        let mut path = String::new();
        for y in 0..self.size {
            let mut x = 0;
            while x < self.size {
                if self.get(x, y) {
                    let start = x;
                    while x < self.size && self.get(x, y) {
                        x += 1;
                    }
                    path.push_str(&format!(
                        "M{},{}h{}v1h-{}z",
                        start as u32 + border,
                        y as u32 + border,
                        x - start,
                        x - start
                    ));
                } else {
                    x += 1;
                }
            }
        }
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {dim} {dim}\" shape-rendering=\"crispEdges\">\
<rect width=\"100%\" height=\"100%\" fill=\"#fff\"/><path fill=\"#000\" d=\"{path}\"/></svg>\n"
        )
    }

    /// PNG（1 モジュール = `scale` 画素、`border` は静穏域のモジュール数）。
    pub fn to_png(&self, scale: u32, border: u32) -> Vec<u8> {
        let scale = scale.clamp(1, 64);
        let dim = (self.size as u32 + 2 * border) * scale;
        png::encode_bilevel(dim, dim, |px, py| {
            let (mx, my) = (px / scale, py / scale);
            mx >= border && my >= border && self.get((mx - border) as usize, (my - border) as usize)
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// ISO/IEC 18004 附属書 I の例（"01234567"、型番 1-M）。
    #[test]
    fn spec_annex_example_codewords() {
        let data = encode_data(b"01234567", Mode::Numeric, 1);
        let want_data = [
            0b0001_0000,
            0b0010_0000,
            0b0000_1100,
            0b0101_0110,
            0b0110_0001,
            0b1000_0000,
            0b1110_1100,
            0b0001_0001,
            0b1110_1100,
            0b0001_0001,
            0b1110_1100,
            0b0001_0001,
            0b1110_1100,
            0b0001_0001,
            0b1110_1100,
            0b0001_0001,
        ];
        assert_eq!(data, want_data);
        let ecc = rs_remainder(&data, &rs_divisor(10));
        let want_ecc = [
            0b1010_0101,
            0b0010_0100,
            0b1101_0100,
            0b1100_0001,
            0b1110_1101,
            0b0011_0110,
            0b1100_0111,
            0b1000_0111,
            0b0010_1100,
            0b0101_0101,
        ];
        assert_eq!(ecc, want_ecc);
        let q = QrCode::encode("01234567").unwrap();
        assert_eq!((q.version(), q.size()), (1, 21));
    }

    #[test]
    fn format_and_version_bits_known_values() {
        // 表 C.1: M・マスク 0 = 101010000010010、M・マスク 5 = 100000011001110
        assert_eq!(format_bits(0), 0b101_0100_0001_0010);
        assert_eq!(format_bits(5), 0b100_0000_1100_1110);
        // 表 D.1: 型番 7 = 000111110010010100
        assert_eq!(version_bits(7), 0b00_0111_1100_1001_0100);
        assert_eq!(version_bits(10), 0b00_1010_0100_1101_0011);
    }

    #[test]
    fn version_selection_and_capacity() {
        // バイトモード・M の容量: 型番 1 = 14、型番 10 = 213
        assert_eq!(QrCode::encode(&"a".repeat(14)).unwrap().version(), 1);
        assert_eq!(QrCode::encode(&"a".repeat(15)).unwrap().version(), 2);
        assert_eq!(QrCode::encode(&"a".repeat(213)).unwrap().version(), 10);
        assert_eq!(QrCode::encode(&"a".repeat(214)), Err(QrError::TooLong));
        assert_eq!(QrCode::encode_with(b"x", Some(8)), Err(QrError::BadMask));
    }

    /// 試験用の復号器: モジュールの並びから形式情報を読み、マスクを外し、コード語を集め、
    /// RS の検査（シンドロームが 0）をしてから中身を取り出す。
    pub(crate) fn decode(q: &QrCode) -> Result<Vec<u8>, &'static str> {
        let n = q.size();
        // 形式情報（1 つ目）を読んで、32 通りから最も近いものを選ぶ
        let mut raw = 0u32;
        let mut put = |i: usize, b: bool| raw |= u32::from(b) << i;
        for i in 0..=5 {
            put(i, q.get(8, i));
        }
        put(6, q.get(8, 7));
        put(7, q.get(8, 8));
        put(8, q.get(7, 8));
        for i in 9..15 {
            put(i, q.get(14 - i, 8));
        }
        let (fmt, dist) = (0..32u32)
            .map(|d| {
                let mut rem = d;
                for _ in 0..10 {
                    rem = (rem << 1) ^ ((rem >> 9) * 0x537);
                }
                let code = (d << 10 | rem) ^ 0x5412;
                (d, (code ^ raw).count_ones())
            })
            .min_by_key(|&(_, d)| d)
            .ok_or("format")?;
        if dist > 3 || fmt >> 3 != ECL_M_BITS {
            return Err("format info");
        }
        let mask = (fmt & 7) as u8;
        let version = ((n - 17) / 4) as u8;
        // 機能パターンの位置は同じ型番の空の符号で求める
        let mut tmpl = QrCode::blank(version);
        tmpl.draw_function_patterns();
        let mut bits = Vec::new();
        let mut right = n as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..n {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let y = if (right + 1) & 2 == 0 {
                        n - 1 - vert
                    } else {
                        vert
                    };
                    if !tmpl.is_function(x, y) {
                        bits.push(q.get(x, y) ^ mask_bit(mask, x, y));
                    }
                }
            }
            right -= 2;
        }
        let (total, ec, groups) = BLOCKS_M[version as usize - 1];
        let cw: Vec<u8> = bits
            .chunks(8)
            .take(total)
            .map(|c| c.iter().fold(0u8, |a, &b| a << 1 | u8::from(b)))
            .collect();
        // インターリーブを戻す
        let lens: Vec<usize> = groups
            .iter()
            .flat_map(|&(c, l)| std::iter::repeat_n(l, c))
            .collect();
        let mut blocks: Vec<Vec<u8>> = lens.iter().map(|_| Vec::new()).collect();
        let mut k = 0;
        for i in 0..*lens.iter().max().ok_or("blocks")? {
            for (b, &l) in blocks.iter_mut().zip(&lens) {
                if i < l {
                    b.push(cw[k]);
                    k += 1;
                }
            }
        }
        for _ in 0..ec {
            for b in blocks.iter_mut() {
                b.push(cw[k]);
                k += 1;
            }
        }
        // シンドローム: 各 α^i を根として評価して 0
        let mut data = Vec::new();
        for (b, &l) in blocks.iter().zip(&lens) {
            let mut root = 1u8;
            for _ in 0..ec {
                let s = b.iter().fold(0u8, |acc, &c| gf_mul(acc, root) ^ c);
                if s != 0 {
                    return Err("rs syndrome");
                }
                root = gf_mul(root, 2);
            }
            data.extend_from_slice(&b[..l]);
        }
        // 中身（1 セグメント）
        let bit = |i: usize| (data[i / 8] >> (7 - i % 8)) & 1 != 0;
        let read = |pos: &mut usize, nb: usize| {
            let mut v = 0u32;
            for _ in 0..nb {
                v = v << 1 | u32::from(bit(*pos));
                *pos += 1;
            }
            v
        };
        let mut pos = 0;
        let mode = match read(&mut pos, 4) {
            0b0001 => Mode::Numeric,
            0b0100 => Mode::Byte,
            _ => return Err("mode"),
        };
        let count = read(&mut pos, mode.count_bits(version)) as usize;
        let mut out = Vec::new();
        match mode {
            Mode::Byte => {
                for _ in 0..count {
                    out.push(read(&mut pos, 8) as u8);
                }
            }
            Mode::Numeric => {
                let mut left = count;
                while left > 0 {
                    let g = left.min(3);
                    let v = read(&mut pos, [0, 4, 7, 10][g]);
                    out.extend(format!("{v:0g$}").bytes());
                    left -= g;
                }
            }
        }
        Ok(out)
    }

    #[test]
    fn round_trip_all_versions_and_masks() {
        for len in [1usize, 14, 15, 30, 60, 84, 100, 122, 150, 180, 213] {
            let s: Vec<u8> = (0..len)
                .map(|i| b"senca://ingest?v=1&proto=srt"[i % 28])
                .collect();
            for mask in 0..8 {
                let q = QrCode::encode_with(&s, Some(mask)).unwrap();
                assert_eq!(decode(&q).unwrap(), s, "len {len} mask {mask}");
            }
            let q = QrCode::encode_with(&s, None).unwrap();
            assert_eq!(decode(&q).unwrap(), s);
        }
        let q = QrCode::encode("01234567").unwrap();
        assert_eq!(decode(&q).unwrap(), b"01234567");
    }

    #[test]
    fn structure_checks() {
        let q = QrCode::encode("senca://ingest?v=1&proto=rtmp&url=rtmp://h/live&key=sk_x").unwrap();
        let n = q.size();
        // 位置検出パターンの中心 3x3 と外周
        for (cx, cy) in [(3, 3), (n - 4, 3), (3, n - 4)] {
            for d in 0..7 {
                assert!(q.get(cx - 3 + d, cy - 3) && q.get(cx - 3 + d, cy + 3));
                assert!(q.get(cx - 3, cy - 3 + d) && q.get(cx + 3, cy - 3 + d));
            }
            assert!(q.get(cx, cy) && !q.get(cx - 2, cy) && !q.get(cx + 2, cy));
        }
        // 2 つの形式情報が同じ値
        let mut a = 0u32;
        let mut b = 0u32;
        for i in 0..8 {
            b |= u32::from(q.get(n - 1 - i, 8)) << i;
        }
        for i in 8..15 {
            b |= u32::from(q.get(8, n - 15 + i)) << i;
        }
        for i in 0..=5 {
            a |= u32::from(q.get(8, i)) << i;
        }
        a |=
            u32::from(q.get(8, 7)) << 6 | u32::from(q.get(8, 8)) << 7 | u32::from(q.get(7, 8)) << 8;
        for i in 9..15 {
            a |= u32::from(q.get(14 - i, 8)) << i;
        }
        assert_eq!(a, b);
        assert_eq!(a, format_bits(q.mask()));
        let svg = q.to_svg(4);
        assert!(
            svg.starts_with("<svg")
                && svg.contains(&format!("viewBox=\"0 0 {} {}\"", n + 8, n + 8))
        );
        let png = q.to_png(4, 4);
        assert_eq!(&png[1..4], b"PNG");
    }
}
