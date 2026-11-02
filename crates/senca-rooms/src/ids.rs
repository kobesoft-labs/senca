//! 推測できない ID（接続・イベント・部屋）。

const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

/// 16 byte の乱数を Crockford base32（小文字）の 26 文字にする。乱数が取れなければ `fallback` を混ぜた値。
pub(crate) fn random_id(fallback: &mut u64) -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        *fallback = fallback.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        b[..8].copy_from_slice(&fallback.to_le_bytes());
        b[8..].copy_from_slice(&fallback.rotate_left(29).to_le_bytes());
    }
    let v = u128::from_be_bytes(b);
    (0..26)
        .map(|i| ALPHABET[((v >> (i * 5)) & 31) as usize] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_are_distinct_and_well_formed() {
        let mut f = 1;
        let a = super::random_id(&mut f);
        let b = super::random_id(&mut f);
        assert_eq!(a.len(), 26);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric()));
    }
}
