//! 推測困難な 128 bit の ID（仕様 4.2）。OS の乱数源から作る。

use std::fmt;
use std::str::FromStr;

/// 128 bit の ID。文字列表記は小文字 16 進 32 文字。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id128(pub [u8; 16]);

impl Id128 {
    /// OS の乱数源から作る。乱数源が使えないときは `Err`（仕様 9.3: 乱数の取得に失敗したら作らない）。
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b)?;
        Ok(Id128(b))
    }

    pub const fn from_bytes(b: [u8; 16]) -> Self {
        Id128(b)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for Id128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Id128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id128({self})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseIdError;

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid id")
    }
}

impl std::error::Error for ParseIdError {}

impl FromStr for Id128 {
    type Err = ParseIdError;

    /// 小文字 16 進 32 文字だけを受け付ける（大文字や区切りは拒否。表記を一つに保つ）。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.as_bytes();
        if s.len() != 32 {
            return Err(ParseIdError);
        }
        let mut out = [0u8; 16];
        for (i, pair) in s.as_chunks::<2>().0.iter().enumerate() {
            out[i] = (hex(pair[0])? << 4) | hex(pair[1])?;
        }
        Ok(Id128(out))
    }
}

fn hex(c: u8) -> Result<u8, ParseIdError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(ParseIdError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let id = Id128::random().unwrap();
        let s = id.to_string();
        assert_eq!(s.len(), 32);
        assert_eq!(s.parse::<Id128>().unwrap(), id);
    }

    #[test]
    fn rejects_uppercase_and_length() {
        assert!("ABCDEF00000000000000000000000000".parse::<Id128>().is_err());
        assert!("abc".parse::<Id128>().is_err());
        assert!("zz000000000000000000000000000000".parse::<Id128>().is_err());
    }

    #[test]
    fn random_differs() {
        assert_ne!(Id128::random().unwrap(), Id128::random().unwrap());
    }
}
