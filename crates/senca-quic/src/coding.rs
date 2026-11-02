//! 受信バイト列の読み取り（境界検査つき）。

use crate::varint;

/// 読み取りの失敗（長さ不足・範囲外）。どの層の誤りにするかは呼び出し側が決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Short;

pub(crate) type R<T> = Result<T, Short>;

/// スライスを前から読む。
#[derive(Debug, Clone)]
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
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let r = &self.buf[self.pos..];
        self.pos = self.buf.len();
        r
    }
    pub(crate) fn u8(&mut self) -> R<u8> {
        let b = *self.buf.get(self.pos).ok_or(Short)?;
        self.pos += 1;
        Ok(b)
    }
    pub(crate) fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        if self.remaining() < n {
            return Err(Short);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub(crate) fn array<const N: usize>(&mut self) -> R<[u8; N]> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }
    pub(crate) fn u32(&mut self) -> R<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub(crate) fn varint(&mut self) -> R<u64> {
        let (v, n) = varint::read(&self.buf[self.pos..]).ok_or(Short)?;
        self.pos += n;
        Ok(v)
    }
    /// varint の長さ前置のバイト列。
    pub(crate) fn varint_bytes(&mut self) -> R<&'a [u8]> {
        let n = self.varint()?;
        let n = usize::try_from(n).map_err(|_| Short)?;
        self.bytes(n)
    }
}
