//! profile_tier_level（H.265 7.3.3）と codecs 文字列。

use crate::bits::Bits;
use crate::error::{Error, Result};

/// profile_tier_level のうち general_* の部分。サブレイヤーの値は読み飛ばす。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProfileTierLevel {
    pub general_profile_space: u8,
    pub general_tier_flag: bool,
    pub general_profile_idc: u8,
    /// general_profile_compatibility_flag[0..32]。flag[0] が最上位ビット（ビット列の順）。
    pub general_profile_compatibility_flags: u32,
    /// general_progressive_source_flag から始まる 48 ビット（下位 48 ビットに入れる）。
    pub general_constraint_indicator_flags: u64,
    pub general_level_idc: u8,
}

impl ProfileTierLevel {
    /// profilePresentFlag = 1 の profile_tier_level を読む。
    pub(crate) fn parse(r: &mut Bits<'_>, max_sub_layers_minus1: u8) -> Result<Self> {
        let general_profile_space = r.u8(2)?;
        let general_tier_flag = r.flag()?;
        let general_profile_idc = r.u8(5)?;
        let general_profile_compatibility_flags = r.u(32)?;
        let hi = r.u(16)? as u64;
        let lo = r.u(32)? as u64;
        let general_constraint_indicator_flags = (hi << 32) | lo;
        let general_level_idc = r.u8(8)?;
        let n = max_sub_layers_minus1 as usize;
        let mut profile_present = [false; 8];
        let mut level_present = [false; 8];
        for i in 0..n {
            profile_present[i] = r.flag()?;
            level_present[i] = r.flag()?;
        }
        if n > 0 {
            r.skip(2 * (8 - n))?; // reserved_zero_2bits
        }
        for i in 0..n {
            if profile_present[i] {
                r.skip(88)?;
            }
            if level_present[i] {
                r.skip(8)?;
            }
        }
        Ok(ProfileTierLevel {
            general_profile_space,
            general_tier_flag,
            general_profile_idc,
            general_profile_compatibility_flags,
            general_constraint_indicator_flags,
            general_level_idc,
        })
    }

    /// 6 バイトの general_constraint_indicator_flags。
    pub fn constraint_bytes(&self) -> [u8; 6] {
        let b = self.general_constraint_indicator_flags.to_be_bytes();
        [b[2], b[3], b[4], b[5], b[6], b[7]]
    }

    /// ISO/IEC 14496-15 附属書 E の codecs 文字列（`hvc1.1.6.L93.B0` の形）。
    pub fn codecs_string(&self) -> String {
        self.codecs_string_with("hvc1")
    }

    /// サンプルエントリ名（`hvc1` か `hev1`）を指定して codecs 文字列を作る。
    pub fn codecs_string_with(&self, fourcc: &str) -> String {
        let space = ["", "A", "B", "C"][(self.general_profile_space & 3) as usize];
        // 互換フラグはビットを逆順にして 16 進（先頭の 0 なし）
        let compat = self.general_profile_compatibility_flags.reverse_bits();
        let tier = if self.general_tier_flag { 'H' } else { 'L' };
        let mut s = format!(
            "{fourcc}.{space}{}.{compat:X}.{tier}{}",
            self.general_profile_idc, self.general_level_idc
        );
        let bytes = self.constraint_bytes();
        let used = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        for b in &bytes[..used] {
            s.push_str(&format!(".{b:X}"));
        }
        s
    }
}

/// codecs 文字列を解析して [`ProfileTierLevel`] に戻す（試験・設定値の検証用）。
pub fn parse_codecs_string(s: &str) -> Result<ProfileTierLevel> {
    let bad = Error::Invalid("hvc1/hev1 の codecs 文字列");
    let mut it = s.split('.');
    let fourcc = it.next().ok_or(bad)?;
    if fourcc != "hvc1" && fourcc != "hev1" {
        return Err(bad);
    }
    let p = it.next().ok_or(bad)?;
    let (space, idc) = match p.as_bytes().first() {
        Some(b'A') => (1, &p[1..]),
        Some(b'B') => (2, &p[1..]),
        Some(b'C') => (3, &p[1..]),
        _ => (0, p),
    };
    let general_profile_idc: u8 = idc.parse().map_err(|_| bad)?;
    if general_profile_idc > 31 {
        return Err(bad);
    }
    let compat = u32::from_str_radix(it.next().ok_or(bad)?, 16).map_err(|_| bad)?;
    let tl = it.next().ok_or(bad)?;
    let tier = match tl.as_bytes().first() {
        Some(b'L') => false,
        Some(b'H') => true,
        _ => return Err(bad),
    };
    let general_level_idc: u8 = tl[1..].parse().map_err(|_| bad)?;
    let mut constraint = 0u64;
    for (n, part) in it.enumerate() {
        if n >= 6 || part.is_empty() || part.len() > 2 {
            return Err(bad);
        }
        let b = u8::from_str_radix(part, 16).map_err(|_| bad)?;
        constraint |= (b as u64) << (8 * (5 - n));
    }
    Ok(ProfileTierLevel {
        general_profile_space: space,
        general_tier_flag: tier,
        general_profile_idc,
        general_profile_compatibility_flags: compat.reverse_bits(),
        general_constraint_indicator_flags: constraint,
        general_level_idc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_strings() {
        // Main、Level 3.1、progressive + frame_only（0x90）
        let p = ProfileTierLevel {
            general_profile_space: 0,
            general_tier_flag: false,
            general_profile_idc: 1,
            general_profile_compatibility_flags: 0x6000_0000,
            general_constraint_indicator_flags: 0x9000_0000_0000,
            general_level_idc: 93,
        };
        assert_eq!(p.codecs_string(), "hvc1.1.6.L93.90");
        assert_eq!(parse_codecs_string("hvc1.1.6.L93.90").unwrap(), p);
        // 14496-15 附属書 E の例
        let p = parse_codecs_string("hev1.A4.41.H120.B0.23").unwrap();
        assert_eq!(p.general_profile_space, 1);
        assert_eq!(p.general_profile_idc, 4);
        assert!(p.general_tier_flag);
        assert_eq!(p.codecs_string_with("hev1"), "hev1.A4.41.H120.B0.23");
        let p = ProfileTierLevel {
            general_profile_idc: 2,
            general_profile_compatibility_flags: 0x2000_0000,
            general_level_idc: 153,
            ..Default::default()
        };
        assert_eq!(p.codecs_string(), "hvc1.2.4.L153");
        for bad in [
            "avc1.1.6.L93",
            "hvc1.1.6",
            "hvc1.x.6.L93",
            "hvc1.1.6.X93",
            "hvc1.1.6.L93.1.2.3.4.5.6.7",
            "hvc1.1.6.L93.100",
            "hvc1.40.6.L93",
        ] {
            assert!(parse_codecs_string(bad).is_err(), "{bad}");
        }
    }
}
