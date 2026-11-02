//! 可変長整数（LEB128、最大 10 バイト）と zigzag。

use bytes::BufMut;

/// 符号なしの値を書く。
#[inline]
pub fn put_u(b: &mut impl BufMut, mut v: u64) {
    while v >= 0x80 {
        b.put_u8((v as u8) | 0x80);
        v >>= 7;
    }
    b.put_u8(v as u8);
}

/// 符号つきの値を zigzag で書く。
#[inline]
pub fn put_i(b: &mut impl BufMut, v: i64) {
    put_u(b, zigzag(v));
}

#[inline]
pub fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
pub fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// 書いたときのバイト数。
#[inline]
pub fn len_u(v: u64) -> usize {
    let bits = 64 - (v | 1).leading_zeros() as usize;
    bits.div_ceil(7)
}

/// 長すぎる・64 bit を超える表現。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadVarint;

/// 先頭の値を読む。`Ok(None)` はバイトが足りない。長すぎる・溢れる表現は `Err`。
#[inline]
pub fn get_u(buf: &[u8]) -> Result<Option<(u64, usize)>, BadVarint> {
    let mut v: u64 = 0;
    for (i, &b) in buf.iter().enumerate() {
        if i == 9 && b > 1 {
            return Err(BadVarint); // 64 bit を超える
        }
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            if i > 0 && b == 0 {
                return Err(BadVarint); // 余計な 0 の続き（表現は 1 つだけ）
            }
            return Ok(Some((v, i + 1)));
        }
        if i == 9 {
            return Err(BadVarint);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let vals = [
            0u64,
            1,
            127,
            128,
            255,
            300,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
        ];
        for v in vals {
            let mut b = Vec::new();
            put_u(&mut b, v);
            assert_eq!(b.len(), len_u(v), "{v}");
            assert_eq!(get_u(&b).unwrap(), Some((v, b.len())));
            assert_eq!(get_u(&b[..b.len() - 1]).unwrap(), None);
        }
        for v in [0i64, 1, -1, 63, -64, 64, i64::MAX, i64::MIN] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn rejects_overlong() {
        assert!(get_u(&[0x80, 0x00]).is_err());
        assert!(get_u(&[0xff; 10]).is_err());
        assert!(get_u(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]).is_err());
        assert_eq!(
            get_u(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]).unwrap(),
            Some((u64::MAX, 10))
        );
    }
}
