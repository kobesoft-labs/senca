//! `senca_core::bits::BitReader` を `Result` で使うための薄い包み。

use crate::error::{Error, Result};
use senca_core::bits::BitReader;

pub(crate) struct Bits<'a>(BitReader<'a>);

impl<'a> Bits<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Bits(BitReader::new(data))
    }

    pub(crate) fn u(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        self.0
            .read_bits(n)
            .map(|v| v as u32)
            .ok_or(Error::Truncated)
    }

    pub(crate) fn u8(&mut self, n: u32) -> Result<u8> {
        debug_assert!(n <= 8);
        self.0.read_bits(n).map(|v| v as u8).ok_or(Error::Truncated)
    }

    pub(crate) fn flag(&mut self) -> Result<bool> {
        self.0.read_bit().ok_or(Error::Truncated)
    }

    pub(crate) fn ue(&mut self) -> Result<u32> {
        self.0.read_ue().ok_or(Error::Truncated)
    }

    /// 上限付きの ue(v)。
    pub(crate) fn ue_max(&mut self, max: u32, what: &'static str) -> Result<u32> {
        let v = self.ue()?;
        if v > max {
            return Err(Error::Invalid(what));
        }
        Ok(v)
    }

    pub(crate) fn se(&mut self) -> Result<i32> {
        self.0.read_se().ok_or(Error::Truncated)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        self.0.skip(n).ok_or(Error::Truncated)
    }
}
