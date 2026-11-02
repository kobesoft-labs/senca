//! Connection ID（RFC 9000 5.1）。

use std::fmt;

/// QUIC v1 の connection ID の最大長。
pub const MAX_CID_LEN: usize = 20;

/// connection ID（0〜20 バイト）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId {
    len: u8,
    bytes: [u8; MAX_CID_LEN],
}

impl ConnectionId {
    /// 20 バイトを超えれば `None`。
    pub fn new(b: &[u8]) -> Option<Self> {
        if b.len() > MAX_CID_LEN {
            return None;
        }
        let mut bytes = [0u8; MAX_CID_LEN];
        bytes[..b.len()].copy_from_slice(b);
        Some(Self { len: b.len() as u8, bytes })
    }

    /// OS の乱数で作る。
    pub fn random(len: usize) -> Result<Self, getrandom::Error> {
        let len = len.min(MAX_CID_LEN);
        let mut bytes = [0u8; MAX_CID_LEN];
        getrandom::fill(&mut bytes[..len])?;
        Ok(Self { len: len as u8, bytes })
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl fmt::Debug for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.as_slice() {
            write!(f, "{b:02x}")?;
        }
        if self.is_empty() {
            f.write_str("(empty)")?;
        }
        Ok(())
    }
}

/// stateless reset token（16 バイト）。
pub(crate) type ResetToken = [u8; 16];
