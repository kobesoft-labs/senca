//! 受信バイト列の読み取りと書き込み（MoQ の varint を使う）。

use bytes::Bytes;

use crate::varint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Short;

pub(crate) type R<T> = Result<T, Short>;

/// `Bytes` を前から読む（切り出しはコピーしない）。
#[derive(Debug, Clone)]
pub(crate) struct Reader {
    buf: Bytes,
    pos: usize,
}

impl Reader {
    pub(crate) fn new(buf: Bytes) -> Self {
        Self { buf, pos: 0 }
    }
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    pub(crate) fn u8(&mut self) -> R<u8> {
        let b = *self.buf.get(self.pos).ok_or(Short)?;
        self.pos += 1;
        Ok(b)
    }
    pub(crate) fn varint(&mut self) -> R<u64> {
        let (v, n) = varint::read(&self.buf[self.pos..]).ok_or(Short)?;
        self.pos += n;
        Ok(v)
    }
    pub(crate) fn bytes(&mut self, n: usize) -> R<Bytes> {
        if self.remaining() < n {
            return Err(Short);
        }
        let b = self.buf.slice(self.pos..self.pos + n);
        self.pos += n;
        Ok(b)
    }
    /// varint の長さ前置のバイト列。`max` を超えたら誤り。
    pub(crate) fn len_bytes(&mut self, max: usize) -> R<Bytes> {
        let n = self.varint()?;
        if n > max as u64 {
            return Err(Short);
        }
        self.bytes(n as usize)
    }
    pub(crate) fn rest(&mut self) -> Bytes {
        let b = self.buf.slice(self.pos..);
        self.pos = self.buf.len();
        b
    }
}

/// スライスを前から読む（バッファを写さずに位置だけ進める）。
#[derive(Debug, Clone)]
pub(crate) struct SliceReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> SliceReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }
    pub(crate) fn u8(&mut self) -> R<u8> {
        let b = *self.buf.get(self.pos).ok_or(Short)?;
        self.pos += 1;
        Ok(b)
    }
    pub(crate) fn varint(&mut self) -> R<u64> {
        let (v, n) = varint::read(&self.buf[self.pos..]).ok_or(Short)?;
        self.pos += n;
        Ok(v)
    }
    pub(crate) fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        if self.remaining() < n {
            return Err(Short);
        }
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }
}

pub(crate) fn put_varint(out: &mut Vec<u8>, v: u64) {
    varint::write(out, v);
}

pub(crate) fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    varint::write(out, b.len() as u64);
    out.extend_from_slice(b);
}
