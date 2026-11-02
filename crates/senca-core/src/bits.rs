//! ビット単位の読み書き。範囲外の読み取りは `None`（パニックしない）。

/// MSB から順に読む。
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize, // ビット位置
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    pub fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn is_byte_aligned(&self) -> bool {
        self.pos.is_multiple_of(8)
    }

    pub fn read_bit(&mut self) -> Option<bool> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(bit == 1)
    }

    /// 最大 64 ビット。
    pub fn read_bits(&mut self, n: u32) -> Option<u64> {
        if n > 64 || (n as usize) > self.bits_left() {
            return None;
        }
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as u64;
        }
        Some(v)
    }

    pub fn read_u8(&mut self, n: u32) -> Option<u8> {
        debug_assert!(n <= 8);
        self.read_bits(n).map(|v| v as u8)
    }

    pub fn read_u32(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        self.read_bits(n).map(|v| v as u32)
    }

    pub fn skip(&mut self, n: usize) -> Option<()> {
        if n > self.bits_left() {
            return None;
        }
        self.pos += n;
        Some(())
    }

    /// ue(v)。値が u32 に収まらない符号（先頭の 0 が 32 個以上）は `None`。
    pub fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while !self.read_bit()? {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = self.read_bits(zeros)?;
        let v = (1u64 << zeros) - 1 + rest;
        u32::try_from(v).ok()
    }

    /// se(v)。
    pub fn read_se(&mut self) -> Option<i32> {
        let k = self.read_ue()? as i64;
        let v = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        i32::try_from(v).ok()
    }

    pub fn byte_align(&mut self) {
        self.pos = self.pos.div_ceil(8) * 8;
    }

    /// rbsp_trailing_bits の前にまだデータがあるか（H.264/H.265 の more_rbsp_data）。
    pub fn more_rbsp_data(&self) -> bool {
        let total = self.data.len() * 8;
        if self.pos >= total {
            return false;
        }
        // 最後の 1 のビット位置を探す
        let mut last_one = None;
        for i in (0..self.data.len()).rev() {
            let b = self.data[i];
            if b != 0 {
                last_one = Some(i * 8 + 7 - b.trailing_zeros() as usize);
                break;
            }
        }
        match last_one {
            Some(p) => self.pos < p,
            None => false,
        }
    }
}

/// MSB から順に書く。
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    buf: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_bit(&mut self, b: bool) {
        if self.nbits.is_multiple_of(8) {
            self.buf.push(0);
        }
        if b {
            let last = self.buf.last_mut().unwrap();
            *last |= 1 << (7 - (self.nbits % 8));
        }
        self.nbits += 1;
    }

    pub fn write_bits(&mut self, v: u64, n: u32) {
        assert!(n <= 64);
        for i in (0..n).rev() {
            self.write_bit((v >> i) & 1 == 1);
        }
    }

    pub fn write_ue(&mut self, v: u32) {
        let v = v as u64 + 1;
        let len = 64 - v.leading_zeros();
        self.write_bits(0, len - 1);
        self.write_bits(v, len);
    }

    pub fn write_se(&mut self, v: i32) {
        let k = if v > 0 { 2 * v as i64 - 1 } else { -2 * v as i64 };
        self.write_ue(k as u32);
    }

    pub fn bit_len(&self) -> usize {
        self.nbits
    }

    pub fn is_byte_aligned(&self) -> bool {
        self.nbits.is_multiple_of(8)
    }

    /// 残りを 0 で埋めて返す。
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_roundtrip() {
        let mut w = BitWriter::new();
        let vals = [0u32, 1, 2, 3, 7, 8, 255, 65535, u32::MAX - 1];
        for v in vals {
            w.write_ue(v);
        }
        for v in [-5i32, 0, 5, i32::MIN + 1, i32::MAX] {
            w.write_se(v);
        }
        let data = w.finish();
        let mut r = BitReader::new(&data);
        for v in vals {
            assert_eq!(r.read_ue(), Some(v));
        }
        for v in [-5i32, 0, 5, i32::MIN + 1, i32::MAX] {
            assert_eq!(r.read_se(), Some(v));
        }
    }

    #[test]
    fn out_of_range() {
        let mut r = BitReader::new(&[0xff]);
        assert_eq!(r.read_bits(9), None);
        assert_eq!(r.read_bits(8), Some(0xff));
        assert_eq!(r.read_bit(), None);
        let mut r = BitReader::new(&[0, 0, 0, 0, 0]);
        assert_eq!(r.read_ue(), None);
    }

    #[test]
    fn more_rbsp() {
        let d = [0b1010_0000u8, 0b1000_0000];
        let mut r = BitReader::new(&d);
        r.read_bits(3).unwrap();
        assert!(r.more_rbsp_data());
        r.read_bits(5).unwrap();
        assert!(!r.more_rbsp_data());
    }
}
