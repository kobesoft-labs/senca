//! DEFLATE（RFC 1951）と zlib（RFC 1950）の圧縮・展開。
//!
//! 圧縮: ハッシュ連鎖の LZ77（1 段の遅延一致）＋ ブロックごとに「格納・固定 Huffman・動的 Huffman」
//! のうち最も短いものを選ぶ。Huffman 符号の長さは 15 bit（符号長の符号は 7 bit）に制限する。
//! 展開: 9 bit の早引き表＋正規 Huffman の逐次復号。出力の上限を超えたら誤りにする（zip bomb 対策）。

use crate::checksum::adler32;
use crate::error::{ImageError, Result};

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];
const MAX_BITS: usize = 15;

fn len_code(len: u16) -> usize {
    LEN_BASE.partition_point(|&b| b <= len) - 1
}

fn dist_code(d: u16) -> usize {
    DIST_BASE.partition_point(|&b| b <= d) - 1
}

fn reverse_bits(mut v: u32, n: u32) -> u32 {
    let mut r = 0;
    for _ in 0..n {
        r = (r << 1) | (v & 1);
        v >>= 1;
    }
    r
}

// ---------------------------------------------------------------- 圧縮

struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            n: 0,
        }
    }
    #[inline]
    fn put(&mut self, bits: u32, len: u32) {
        self.acc |= (bits as u64) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    fn align(&mut self) {
        if self.n > 0 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.n = 0;
        }
    }
}

/// 頻度から長さ制限つきの Huffman 符号長を作る（使う記号は必ず 2 つ以上にする）。
fn build_lengths(freq: &[u32], max_len: usize) -> Vec<u8> {
    let n = freq.len();
    let mut f: Vec<u32> = freq.to_vec();
    let mut used = f.iter().filter(|&&v| v > 0).count();
    let mut i = 0;
    while used < 2 && i < n {
        if f[i] == 0 {
            f[i] = 1;
            used += 1;
        }
        i += 1;
    }
    // 木を作る（ノード: 葉 0..n、内部 n..）。
    let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u64, usize)>> =
        std::collections::BinaryHeap::new();
    let mut parent = vec![usize::MAX; 2 * n];
    for (s, &v) in f.iter().enumerate() {
        if v > 0 {
            heap.push(std::cmp::Reverse((v as u64, s)));
        }
    }
    let mut next = n;
    while heap.len() > 1 {
        let std::cmp::Reverse((fa, a)) = heap.pop().unwrap_or(std::cmp::Reverse((0, 0)));
        let std::cmp::Reverse((fb, b)) = heap.pop().unwrap_or(std::cmp::Reverse((0, 0)));
        parent[a] = next;
        parent[b] = next;
        heap.push(std::cmp::Reverse((fa + fb, next)));
        next += 1;
    }
    let mut depth = vec![0usize; n];
    let mut counts = vec![0usize; 64];
    for s in 0..n {
        if f[s] == 0 {
            continue;
        }
        let mut d = 0;
        let mut p = s;
        while parent[p] != usize::MAX {
            p = parent[p];
            d += 1;
        }
        depth[s] = d.max(1);
        counts[depth[s].min(63)] += 1;
    }
    // 長さの上限を守るよう、数を組み替える（miniz と同じ方法）。
    for l in max_len + 1..64 {
        counts[max_len] += counts[l];
        counts[l] = 0;
    }
    let mut total: u64 = (1..=max_len)
        .map(|l| (counts[l] as u64) << (max_len - l))
        .sum();
    while total > 1u64 << max_len {
        counts[max_len] -= 1;
        for l in (1..max_len).rev() {
            if counts[l] != 0 {
                counts[l] -= 1;
                counts[l + 1] += 2;
                break;
            }
        }
        total -= 1;
    }
    // 頻度の低い記号から長い符号を割り当てる。
    let mut syms: Vec<usize> = (0..n).filter(|&s| f[s] > 0).collect();
    syms.sort_by(|&a, &b| {
        f[a].cmp(&f[b])
            .then(depth[b].cmp(&depth[a]))
            .then(a.cmp(&b))
    });
    let mut lens = vec![0u8; n];
    let mut it = syms.into_iter();
    for l in (1..=max_len).rev() {
        for _ in 0..counts[l] {
            if let Some(s) = it.next() {
                lens[s] = l as u8;
            }
        }
    }
    lens
}

/// 符号長から正規 Huffman 符号（ビット反転済み）を作る。
fn canonical_codes(lens: &[u8]) -> Vec<u32> {
    let mut bl = [0u32; MAX_BITS + 2];
    for &l in lens {
        bl[l as usize] += 1;
    }
    bl[0] = 0;
    let mut next = [0u32; MAX_BITS + 2];
    let mut code = 0u32;
    for b in 1..=MAX_BITS {
        code = (code + bl[b - 1]) << 1;
        next[b] = code;
    }
    lens.iter()
        .map(|&l| {
            if l == 0 {
                0
            } else {
                let c = next[l as usize];
                next[l as usize] += 1;
                reverse_bits(c, l as u32)
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Sym {
    /// 一致長（0 なら literal）。
    len: u16,
    /// literal の値、または距離。
    val: u16,
}

struct Matcher<'a> {
    d: &'a [u8],
    head: Vec<i32>,
    prev: Vec<i32>,
    max_chain: usize,
}

const WSIZE: usize = 32768;
const HBITS: u32 = 15;

impl<'a> Matcher<'a> {
    fn new(d: &'a [u8], max_chain: usize) -> Self {
        Self {
            d,
            head: vec![-1; 1 << HBITS],
            prev: vec![-1; WSIZE],
            max_chain,
        }
    }
    #[inline]
    fn hash(&self, i: usize) -> usize {
        let v = (self.d[i] as u32) | ((self.d[i + 1] as u32) << 8) | ((self.d[i + 2] as u32) << 16);
        (v.wrapping_mul(2_654_435_761) >> (32 - HBITS)) as usize
    }
    fn insert(&mut self, i: usize) {
        if i + 3 > self.d.len() {
            return;
        }
        let h = self.hash(i);
        self.prev[i & (WSIZE - 1)] = self.head[h];
        self.head[h] = i as i32;
    }
    fn find(&self, i: usize) -> (usize, usize) {
        let d = self.d;
        if i + 3 > d.len() {
            return (0, 0);
        }
        let max = (d.len() - i).min(258);
        let mut best = (2usize, 0usize);
        let mut p = self.head[self.hash(i)];
        let mut chain = self.max_chain;
        while p >= 0 && chain > 0 {
            let pu = p as usize;
            if pu >= i {
                break;
            }
            let dist = i - pu;
            if dist > WSIZE {
                break;
            }
            if d[pu + best.0.min(max - 1)] == d[i + best.0.min(max - 1)] {
                let mut l = 0;
                while l < max && d[pu + l] == d[i + l] {
                    l += 1;
                }
                if l > best.0 {
                    best = (l, dist);
                    // 十分長ければ探すのをやめる
                    if l == max || l >= 128 {
                        break;
                    }
                }
            }
            let np = self.prev[pu & (WSIZE - 1)];
            if np >= p {
                break;
            }
            p = np;
            chain -= 1;
        }
        if best.0 >= 3 { best } else { (0, 0) }
    }
}

fn lz77(d: &[u8], level: u8) -> Vec<Sym> {
    let (chain, lazy) = match level {
        0 | 1 => (4, false),
        2..=3 => (16, false),
        4..=6 => (32, true),
        _ => (256, true),
    };
    let mut m = Matcher::new(d, chain);
    let mut out = Vec::with_capacity(d.len() / 2);
    let mut i = 0;
    while i < d.len() {
        let (l, dist) = m.find(i);
        m.insert(i);
        if l >= 3 {
            if lazy && l < 32 && i + 1 < d.len() {
                let (l2, _) = m.find(i + 1);
                if l2 > l {
                    out.push(Sym {
                        len: 0,
                        val: d[i] as u16,
                    });
                    i += 1;
                    continue;
                }
            }
            out.push(Sym {
                len: l as u16,
                val: dist as u16,
            });
            for k in 1..l {
                m.insert(i + k);
            }
            i += l;
        } else {
            out.push(Sym {
                len: 0,
                val: d[i] as u16,
            });
            i += 1;
        }
    }
    out
}

fn fixed_lengths() -> (Vec<u8>, Vec<u8>) {
    let mut ll = vec![0u8; 288];
    for (i, l) in ll.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    (ll, vec![5u8; 30])
}

/// 符号長の列を 16/17/18 で圧縮する。(記号, 追加ビット値)
fn rle_lengths(seq: &[u8]) -> Vec<(u8, u8)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < seq.len() {
        let v = seq[i];
        let mut run = 1;
        while i + run < seq.len() && seq[i + run] == v {
            run += 1;
        }
        i += run;
        if v == 0 {
            while run >= 11 {
                let r = run.min(138);
                out.push((18, (r - 11) as u8));
                run -= r;
            }
            if run >= 3 {
                out.push((17, (run - 3) as u8));
                run = 0;
            }
            for _ in 0..run {
                out.push((0, 0));
            }
        } else {
            out.push((v, 0));
            run -= 1;
            while run >= 3 {
                let r = run.min(6);
                out.push((16, (r - 3) as u8));
                run -= r;
            }
            for _ in 0..run {
                out.push((v, 0));
            }
        }
    }
    out
}

fn sym_cost(syms: &[Sym], ll: &[u8], dl: &[u8]) -> u64 {
    let mut bits = ll[256] as u64;
    for s in syms {
        if s.len == 0 {
            bits += ll[s.val as usize] as u64;
        } else {
            let lc = len_code(s.len);
            let dc = dist_code(s.val);
            bits +=
                ll[257 + lc] as u64 + LEN_EXTRA[lc] as u64 + dl[dc] as u64 + DIST_EXTRA[dc] as u64;
        }
    }
    bits
}

fn write_syms(w: &mut BitWriter, syms: &[Sym], ll: &[u8], dl: &[u8]) {
    let lc_codes = canonical_codes(ll);
    let dc_codes = canonical_codes(dl);
    for s in syms {
        if s.len == 0 {
            let v = s.val as usize;
            w.put(lc_codes[v], ll[v] as u32);
        } else {
            let lc = len_code(s.len);
            w.put(lc_codes[257 + lc], ll[257 + lc] as u32);
            w.put((s.len - LEN_BASE[lc]) as u32, LEN_EXTRA[lc] as u32);
            let dc = dist_code(s.val);
            w.put(dc_codes[dc], dl[dc] as u32);
            w.put((s.val - DIST_BASE[dc]) as u32, DIST_EXTRA[dc] as u32);
        }
    }
    w.put(lc_codes[256], ll[256] as u32);
}

fn write_block(w: &mut BitWriter, syms: &[Sym], raw: &[u8], last: bool, level: u8) {
    let bfinal = last as u32;
    let stored_bits = |w: &BitWriter| {
        let align = (8 - ((w.n + 3) % 8)) % 8;
        3 + align as u64 + (raw.len().div_ceil(65535).max(1) * 4 * 8) as u64 + raw.len() as u64 * 8
    };
    let write_stored = |w: &mut BitWriter| {
        let chunks: Vec<&[u8]> = if raw.is_empty() {
            vec![&[][..]]
        } else {
            raw.chunks(65535).collect()
        };
        let nchunks = chunks.len();
        for (ci, c) in chunks.into_iter().enumerate() {
            let fin = if ci + 1 == nchunks { bfinal } else { 0 };
            w.put(fin, 1);
            w.put(0, 2);
            w.align();
            let len = c.len() as u16;
            w.out.extend_from_slice(&len.to_le_bytes());
            w.out.extend_from_slice(&(!len).to_le_bytes());
            w.out.extend_from_slice(c);
        }
    };
    if level == 0 {
        write_stored(w);
        return;
    }
    // 頻度
    let mut lf = vec![0u32; 286];
    let mut df = vec![0u32; 30];
    for s in syms {
        if s.len == 0 {
            lf[s.val as usize] += 1;
        } else {
            lf[257 + len_code(s.len)] += 1;
            df[dist_code(s.val)] += 1;
        }
    }
    lf[256] = 1;
    let dyn_ll = build_lengths(&lf, MAX_BITS);
    let dyn_dl = build_lengths(&df, MAX_BITS);
    let hlit = (dyn_ll.iter().rposition(|&l| l > 0).unwrap_or(0) + 1).max(257);
    let hdist = (dyn_dl.iter().rposition(|&l| l > 0).unwrap_or(0) + 1).max(1);
    let mut seq = dyn_ll[..hlit].to_vec();
    seq.extend_from_slice(&dyn_dl[..hdist]);
    let rle = rle_lengths(&seq);
    let mut cf = vec![0u32; 19];
    for &(s, _) in &rle {
        cf[s as usize] += 1;
    }
    let cl = build_lengths(&cf, 7);
    let hclen = (CL_ORDER.iter().rposition(|&i| cl[i] > 0).unwrap_or(0) + 1).max(4);
    let extra = |s: u8| match s {
        16 => 2,
        17 => 3,
        18 => 7,
        _ => 0,
    };
    let header_bits: u64 = 3
        + 5
        + 5
        + 4
        + 3 * hclen as u64
        + rle
            .iter()
            .map(|&(s, _)| cl[s as usize] as u64 + extra(s))
            .sum::<u64>();
    let dyn_bits = header_bits + sym_cost(syms, &dyn_ll, &dyn_dl);
    let (fl, fd) = fixed_lengths();
    let fixed_bits = 3 + sym_cost(syms, &fl, &fd);
    let st = stored_bits(w);
    if st <= dyn_bits && st <= fixed_bits {
        write_stored(w);
    } else if fixed_bits <= dyn_bits {
        w.put(bfinal, 1);
        w.put(1, 2);
        write_syms(w, syms, &fl, &fd);
    } else {
        w.put(bfinal, 1);
        w.put(2, 2);
        w.put((hlit - 257) as u32, 5);
        w.put((hdist - 1) as u32, 5);
        w.put((hclen - 4) as u32, 4);
        for &i in CL_ORDER.iter().take(hclen) {
            w.put(cl[i] as u32, 3);
        }
        let cc = canonical_codes(&cl);
        for &(s, e) in &rle {
            w.put(cc[s as usize], cl[s as usize] as u32);
            w.put(e as u32, extra(s) as u32);
        }
        write_syms(w, syms, &dyn_ll, &dyn_dl);
    }
}

/// 生の DEFLATE で圧縮する。`level` 0 は格納のみ、1〜9 は速さと率の兼ね合い（既定 6）。
pub fn deflate(data: &[u8], level: u8) -> Vec<u8> {
    let mut w = BitWriter::new();
    if level == 0 || data.is_empty() {
        write_block(&mut w, &[], data, true, 0);
        w.align();
        return w.out;
    }
    let syms = lz77(data, level);
    const BLOCK_SYMS: usize = 32768;
    let mut pos = 0usize;
    let nblocks = syms.len().div_ceil(BLOCK_SYMS);
    for (bi, chunk) in syms.chunks(BLOCK_SYMS).enumerate() {
        let raw_len: usize = chunk
            .iter()
            .map(|s| if s.len == 0 { 1 } else { s.len as usize })
            .sum();
        write_block(
            &mut w,
            chunk,
            &data[pos..pos + raw_len],
            bi + 1 == nblocks,
            level,
        );
        pos += raw_len;
    }
    w.align();
    w.out
}

/// zlib 形式で圧縮する。
pub fn zlib_compress(data: &[u8], level: u8) -> Vec<u8> {
    let flg = if level <= 1 { 0x01 } else { 0x9C };
    let mut out = vec![0x78, flg];
    out.extend_from_slice(&deflate(data, level));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

// ---------------------------------------------------------------- 展開

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            buf: 0,
            cnt: 0,
        }
    }
    #[inline]
    fn refill(&mut self) {
        while self.cnt <= 56 && self.pos < self.data.len() {
            self.buf |= (self.data[self.pos] as u64) << self.cnt;
            self.pos += 1;
            self.cnt += 8;
        }
    }
    #[inline]
    fn bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        if self.cnt < n {
            self.refill();
            if self.cnt < n {
                return Err(ImageError::Truncated);
            }
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        Ok(v)
    }
    fn align(&mut self) {
        let r = self.cnt % 8;
        self.buf >>= r;
        self.cnt -= r;
    }
    /// 読み終えた位置（バイト単位、未使用のビットを返したもの）。
    fn byte_pos(&self) -> usize {
        self.pos - (self.cnt / 8) as usize
    }
}

const FAST: u32 = 9;

struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
    /// (長さ << 12) | 記号。0 は早引き不可。
    fast: Vec<u16>,
}

impl Huffman {
    fn new(lens: &[u8]) -> Result<Self> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lens {
            if l as usize > MAX_BITS {
                return Err(ImageError::Format("符号長が 15 を超える"));
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut left: i32 = 1;
        for &c in counts.iter().skip(1) {
            left <<= 1;
            left -= c as i32;
            if left < 0 {
                return Err(ImageError::Format("Huffman 符号が過剰"));
            }
        }
        let mut offs = [0u16; MAX_BITS + 2];
        for l in 1..=MAX_BITS {
            offs[l + 1] = offs[l] + counts[l];
        }
        let mut symbols = vec![0u16; lens.len()];
        for (s, &l) in lens.iter().enumerate() {
            if l > 0 {
                symbols[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        let codes = canonical_codes(lens);
        let mut fast = vec![0u16; 1 << FAST];
        for (s, &l) in lens.iter().enumerate() {
            if l > 0 && (l as u32) <= FAST {
                let mut i = codes[s] as usize;
                while i < (1 << FAST) {
                    fast[i] = ((l as u16) << 12) | s as u16;
                    i += 1 << l;
                }
            }
        }
        Ok(Self {
            counts,
            symbols,
            fast,
        })
    }

    #[inline]
    fn decode(&self, br: &mut BitReader) -> Result<u16> {
        if br.cnt < FAST {
            br.refill();
        }
        let e = self.fast[(br.buf & ((1 << FAST) - 1)) as usize];
        let l = (e >> 12) as u32;
        if e != 0 && l <= br.cnt {
            br.buf >>= l;
            br.cnt -= l;
            return Ok(e & 0xFFF);
        }
        // 逐次復号
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= br.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(ImageError::Format("Huffman 符号が不正"))
    }
}

/// 生の DEFLATE を展開する。戻り値は (出力, 消費したバイト数)。
pub fn inflate(data: &[u8], max_out: usize) -> Result<(Vec<u8>, usize)> {
    let mut br = BitReader::new(data);
    let mut out: Vec<u8> = Vec::with_capacity(max_out.min(1 << 20));
    let (fl, fd) = fixed_lengths();
    let mut fixed: Option<(Huffman, Huffman)> = None;
    loop {
        let last = br.bits(1)?;
        let ty = br.bits(2)?;
        match ty {
            0 => {
                br.align();
                let len = br.bits(16)? as usize;
                let nlen = br.bits(16)? as usize;
                if len != (!nlen & 0xFFFF) {
                    return Err(ImageError::Format("格納ブロックの長さ"));
                }
                if out.len() + len > max_out {
                    return Err(ImageError::OutputLimit);
                }
                // ビットバッファに残ったバイトを先に使う。
                let mut n = len;
                while n > 0 && br.cnt >= 8 {
                    out.push(br.bits(8)? as u8);
                    n -= 1;
                }
                if br.pos + n > data.len() {
                    return Err(ImageError::Truncated);
                }
                out.extend_from_slice(&data[br.pos..br.pos + n]);
                br.pos += n;
            }
            1 | 2 => {
                let dynamic;
                let (lit, dist) = if ty == 1 {
                    if fixed.is_none() {
                        fixed = Some((Huffman::new(&fl)?, Huffman::new(&fd)?));
                    }
                    let f = fixed.as_ref().ok_or(ImageError::Format("固定表"))?;
                    (&f.0, &f.1)
                } else {
                    dynamic = read_dynamic(&mut br)?;
                    (&dynamic.0, &dynamic.1)
                };
                loop {
                    let s = lit.decode(&mut br)? as usize;
                    if s < 256 {
                        if out.len() >= max_out {
                            return Err(ImageError::OutputLimit);
                        }
                        out.push(s as u8);
                    } else if s == 256 {
                        break;
                    } else {
                        let lc = s - 257;
                        if lc >= 29 {
                            return Err(ImageError::Format("長さの符号"));
                        }
                        let len = LEN_BASE[lc] as usize + br.bits(LEN_EXTRA[lc] as u32)? as usize;
                        let dc = dist.decode(&mut br)? as usize;
                        if dc >= 30 {
                            return Err(ImageError::Format("距離の符号"));
                        }
                        let d = DIST_BASE[dc] as usize + br.bits(DIST_EXTRA[dc] as u32)? as usize;
                        if d > out.len() {
                            return Err(ImageError::Format("距離が出力の先頭を越える"));
                        }
                        if out.len() + len > max_out {
                            return Err(ImageError::OutputLimit);
                        }
                        let start = out.len() - d;
                        if d >= len {
                            out.extend_from_within(start..start + len);
                        } else {
                            for k in 0..len {
                                let b = out[start + k];
                                out.push(b);
                            }
                        }
                    }
                }
            }
            _ => return Err(ImageError::Format("ブロック種別 3")),
        }
        if last == 1 {
            break;
        }
    }
    br.align();
    Ok((out, br.byte_pos()))
}

fn read_dynamic(br: &mut BitReader) -> Result<(Huffman, Huffman)> {
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(ImageError::Format("HLIT/HDIST"));
    }
    let mut cl = [0u8; 19];
    for &i in CL_ORDER.iter().take(hclen) {
        cl[i] = br.bits(3)? as u8;
    }
    let ch = Huffman::new(&cl)?;
    let mut lens = vec![0u8; hlit + hdist];
    let mut i = 0;
    while i < hlit + hdist {
        let s = ch.decode(br)?;
        let (v, rep) = match s {
            0..=15 => (s as u8, 1),
            16 => {
                if i == 0 {
                    return Err(ImageError::Format("繰り返す長さがない"));
                }
                (lens[i - 1], 3 + br.bits(2)? as usize)
            }
            17 => (0, 3 + br.bits(3)? as usize),
            18 => (0, 11 + br.bits(7)? as usize),
            _ => return Err(ImageError::Format("符号長の記号")),
        };
        if i + rep > hlit + hdist {
            return Err(ImageError::Format("符号長の繰り返しがはみ出す"));
        }
        lens[i..i + rep].fill(v);
        i += rep;
    }
    if lens[256] == 0 {
        return Err(ImageError::Format("終端記号の符号がない"));
    }
    Ok((Huffman::new(&lens[..hlit])?, Huffman::new(&lens[hlit..])?))
}

/// zlib 形式を展開する（Adler-32 を検査する）。
pub fn zlib_decompress(data: &[u8], max_out: usize) -> Result<Vec<u8>> {
    if data.len() < 6 {
        return Err(ImageError::Truncated);
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0F != 8 || (cmf >> 4) > 7 {
        return Err(ImageError::Format("zlib の圧縮方式"));
    }
    if !((cmf as u16) << 8 | flg as u16).is_multiple_of(31) {
        return Err(ImageError::Format("zlib の FCHECK"));
    }
    if flg & 0x20 != 0 {
        return Err(ImageError::Unsupported("zlib の辞書"));
    }
    let (out, used) = inflate(&data[2..], max_out)?;
    let p = 2 + used;
    if data.len() < p + 4 {
        return Err(ImageError::Truncated);
    }
    let want = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]);
    if want != adler32(&out) {
        return Err(ImageError::Checksum("Adler-32"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize, seed: u32) -> Vec<u8> {
        let words: [&[u8]; 6] = [
            b"senca ",
            b"media ",
            b"server ",
            b"\x00\x01\x02",
            b"board ",
            b"aaaaaaaaaaaa",
        ];
        let mut s = seed;
        let mut v = Vec::new();
        while v.len() < n {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            if s >> 30 == 0 {
                v.push((s >> 8) as u8);
            } else {
                v.extend_from_slice(words[(s >> 16) as usize % words.len()]);
            }
        }
        v.truncate(n);
        v
    }

    #[test]
    fn roundtrip_levels_and_sizes() {
        for n in [0, 1, 2, 3, 100, 70_000, 300_000] {
            let d = sample(n, n as u32);
            for level in [0, 1, 6, 9] {
                let c = zlib_compress(&d, level);
                let back = zlib_decompress(&c, n + 10).unwrap();
                assert_eq!(back, d, "n {n} level {level}");
                if level > 0 && n > 1000 {
                    assert!(c.len() < n / 2, "n {n} level {level} size {}", c.len());
                }
            }
        }
    }

    #[test]
    fn random_data_falls_back_to_stored() {
        let mut s = 1u32;
        let d: Vec<u8> = (0..100_000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                s as u8
            })
            .collect();
        let c = zlib_compress(&d, 6);
        assert!(c.len() <= d.len() + d.len() / 1000 + 20);
        assert_eq!(zlib_decompress(&c, d.len()).unwrap(), d);
    }

    #[test]
    fn length_limited_huffman() {
        // フィボナッチ頻度は無制限だと長い符号になる。
        let mut f = vec![0u32; 40];
        let (mut a, mut b) = (1u32, 1u32);
        for v in f.iter_mut() {
            *v = a;
            let c = a.saturating_add(b);
            a = b;
            b = c;
        }
        let l = build_lengths(&f, 15);
        assert!(l.iter().all(|&x| (1..=15).contains(&x)));
        let kraft: f64 = l.iter().map(|&x| 2f64.powi(-(x as i32))).sum();
        assert!((kraft - 1.0).abs() < 1e-12);
        let l7 = build_lengths(&f[..19], 7);
        assert!(l7.iter().all(|&x| x <= 7));
    }

    #[test]
    fn output_limit_and_corruption() {
        let d = vec![0u8; 1_000_000];
        let c = zlib_compress(&d, 6);
        assert!(c.len() < 5000);
        assert_eq!(zlib_decompress(&c, 999_999), Err(ImageError::OutputLimit));
        let mut bad = c.clone();
        let n = bad.len();
        bad[n - 1] ^= 1;
        assert_eq!(
            zlib_decompress(&bad, 1_000_000),
            Err(ImageError::Checksum("Adler-32"))
        );
        // 切り詰めと書き換えでパニックしない
        let src = zlib_compress(&sample(5000, 3), 6);
        for cut in 0..src.len() {
            let _ = zlib_decompress(&src[..cut], 10_000);
        }
        let mut s = 7u32;
        for _ in 0..2000 {
            let mut m = src.clone();
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let i = (s >> 8) as usize % m.len();
            m[i] ^= 1 << (s % 8);
            let _ = zlib_decompress(&m, 10_000);
        }
    }

    #[test]
    fn known_stream_from_zlib() {
        // python3 -c "import zlib;print(zlib.compress(b'hello hello hello hello', 9).hex())"
        let c = [
            0x78, 0xda, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0xc8, 0x40, 0x27, 0x01, 0x68, 0x03,
            0x08, 0xb1,
        ];
        assert_eq!(
            zlib_decompress(&c, 100).unwrap(),
            b"hello hello hello hello"
        );
    }
}
