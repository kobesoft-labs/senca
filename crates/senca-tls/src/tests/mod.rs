//! 内部の試験。RFC 8448 のトレースで鍵スケジュール・記録層をバイト単位で突き合わせる。

mod rfc8448;
mod rfc8448_data;

pub(crate) fn hex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2));
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}
