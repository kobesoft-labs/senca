//! 境界を検査するバイト列の読み書き。

/// 読み取りの失敗（decode_error）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecodeError;

pub(crate) type DResult<T> = Result<T, DecodeError>;

pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub(crate) fn bytes(&mut self, n: usize) -> DResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(DecodeError);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> DResult<u8> {
        Ok(self.bytes(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> DResult<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn u24(&mut self) -> DResult<u32> {
        let b = self.bytes(3)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    pub(crate) fn u48(&mut self) -> DResult<u64> {
        let b = self.bytes(6)?;
        Ok(u64::from_be_bytes([
            0, 0, b[0], b[1], b[2], b[3], b[4], b[5],
        ]))
    }

    pub(crate) fn vec8(&mut self) -> DResult<&'a [u8]> {
        let n = self.u8()? as usize;
        self.bytes(n)
    }

    pub(crate) fn vec16(&mut self) -> DResult<&'a [u8]> {
        let n = self.u16()? as usize;
        self.bytes(n)
    }

    pub(crate) fn vec24(&mut self) -> DResult<&'a [u8]> {
        let n = self.u24()? as usize;
        self.bytes(n)
    }

    /// 余りがあれば decode_error。
    pub(crate) fn finish(&self) -> DResult<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(DecodeError)
        }
    }
}

/// u16 の列（長さは byte 数、偶数でなければ誤り）。
pub(crate) fn u16_list(b: &[u8]) -> DResult<Vec<u16>> {
    let (pairs, rest) = b.as_chunks::<2>();
    if !rest.is_empty() {
        return Err(DecodeError);
    }
    Ok(pairs.iter().map(|c| u16::from_be_bytes(*c)).collect())
}

pub(crate) fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_u24(out: &mut Vec<u8>, v: u32) {
    debug_assert!(v < 1 << 24);
    out.extend_from_slice(&v.to_be_bytes()[1..]);
}

pub(crate) fn put_u48(out: &mut Vec<u8>, v: u64) {
    debug_assert!(v < 1 << 48);
    out.extend_from_slice(&v.to_be_bytes()[2..]);
}

pub(crate) fn put_vec8(out: &mut Vec<u8>, v: &[u8]) {
    debug_assert!(v.len() < 1 << 8);
    out.push(v.len() as u8);
    out.extend_from_slice(v);
}

pub(crate) fn put_vec16(out: &mut Vec<u8>, v: &[u8]) {
    debug_assert!(v.len() < 1 << 16);
    put_u16(out, v.len() as u16);
    out.extend_from_slice(v);
}

pub(crate) fn put_vec24(out: &mut Vec<u8>, v: &[u8]) {
    put_u24(out, v.len() as u32);
    out.extend_from_slice(v);
}

pub(crate) fn u16s(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_be_bytes()).collect()
}
