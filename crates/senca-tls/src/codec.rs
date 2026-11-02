//! TLS の表現言語（RFC 8446 3 章）の読み書き。長さはすべて検査する。

use crate::error::Error;

/// 境界を検査する読み取り器。
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::decode("長さが溢れる"))?;
        if end > self.buf.len() {
            return Err(Error::decode("データが足りない"));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, Error> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub(crate) fn u24(&mut self) -> Result<u32, Error> {
        let b = self.take(3)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, Error> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    /// 1 バイト長の可変長ベクタ。
    pub(crate) fn vec8(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u8()? as usize;
        self.take(n)
    }

    /// 2 バイト長の可変長ベクタ。
    pub(crate) fn vec16(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u16()? as usize;
        self.take(n)
    }

    /// 3 バイト長の可変長ベクタ。
    pub(crate) fn vec24(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u24()? as usize;
        self.take(n)
    }

    /// 末尾まで読み切ったことを確かめる。
    pub(crate) fn finish(&self) -> Result<(), Error> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(Error::decode("余分なデータがある"))
        }
    }
}

pub(crate) fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

pub(crate) fn put_u24(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes()[1..]);
}

pub(crate) fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// 長さ欄（`width` バイト）を前置して `f` の書いた中身を包む。
///
/// 中身が欄に収まらない場合は内部エラー（自分の作るメッセージなので通常起きない）。
pub(crate) fn len_prefixed(
    out: &mut Vec<u8>,
    width: usize,
    f: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
) -> Result<(), Error> {
    let at = out.len();
    out.resize(at + width, 0);
    f(out)?;
    let n = out.len() - at - width;
    let max = match width {
        1 => 0xff,
        2 => 0xffff,
        3 => 0xff_ffff,
        _ => return Err(Error::internal("長さ欄の幅が不正")),
    };
    if n > max {
        return Err(Error::internal("長さ欄に収まらない"));
    }
    let bytes = (n as u32).to_be_bytes();
    out[at..at + width].copy_from_slice(&bytes[4 - width..]);
    Ok(())
}

pub(crate) fn put_vec8(out: &mut Vec<u8>, data: &[u8]) -> Result<(), Error> {
    len_prefixed(out, 1, |o| {
        o.extend_from_slice(data);
        Ok(())
    })
}

pub(crate) fn put_vec16(out: &mut Vec<u8>, data: &[u8]) -> Result<(), Error> {
    len_prefixed(out, 2, |o| {
        o.extend_from_slice(data);
        Ok(())
    })
}

pub(crate) fn put_vec24(out: &mut Vec<u8>, data: &[u8]) -> Result<(), Error> {
    len_prefixed(out, 3, |o| {
        o.extend_from_slice(data);
        Ok(())
    })
}
