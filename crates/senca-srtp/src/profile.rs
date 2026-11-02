//! 保護方式（protection profile）と master key。

use zeroize::{Zeroize, ZeroizeOnDrop};

/// SRTP の保護方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SrtpProfile {
    /// AES-128 カウンターモード + HMAC-SHA1 80 bit tag（RFC 3711）。
    Aes128CmHmacSha1_80,
    /// AES-128 カウンターモード + HMAC-SHA1 32 bit tag（SRTP だけ。SRTCP は 80 bit。RFC 5764 4.1.2）。
    Aes128CmHmacSha1_32,
    /// AEAD AES-128-GCM（RFC 7714）。
    AeadAes128Gcm,
    /// AEAD AES-256-GCM（RFC 7714）。
    AeadAes256Gcm,
}

impl SrtpProfile {
    /// DTLS use_srtp 拡張の識別子（RFC 5764 4.1.2・RFC 7714 14.2）。
    pub fn dtls_id(self) -> u16 {
        match self {
            Self::Aes128CmHmacSha1_80 => 0x0001,
            Self::Aes128CmHmacSha1_32 => 0x0002,
            Self::AeadAes128Gcm => 0x0007,
            Self::AeadAes256Gcm => 0x0008,
        }
    }

    /// DTLS use_srtp の識別子から引く。
    pub fn from_dtls_id(id: u16) -> Option<Self> {
        Some(match id {
            0x0001 => Self::Aes128CmHmacSha1_80,
            0x0002 => Self::Aes128CmHmacSha1_32,
            0x0007 => Self::AeadAes128Gcm,
            0x0008 => Self::AeadAes256Gcm,
            _ => return None,
        })
    }

    /// master key の長さ（byte）。
    pub fn master_key_len(self) -> usize {
        match self {
            Self::AeadAes256Gcm => 32,
            _ => 16,
        }
    }

    /// master salt の長さ（byte）。AES-CM は 112 bit、GCM は 96 bit。
    pub fn master_salt_len(self) -> usize {
        if self.is_aead() { 12 } else { 14 }
    }

    /// AEAD（GCM）かどうか。
    pub fn is_aead(self) -> bool {
        matches!(self, Self::AeadAes128Gcm | Self::AeadAes256Gcm)
    }

    /// SRTP の認証 tag の長さ（byte）。
    pub fn rtp_tag_len(self) -> usize {
        match self {
            Self::Aes128CmHmacSha1_80 => 10,
            Self::Aes128CmHmacSha1_32 => 4,
            Self::AeadAes128Gcm | Self::AeadAes256Gcm => 16,
        }
    }

    /// SRTCP の認証 tag の長さ（byte）。
    pub fn rtcp_tag_len(self) -> usize {
        match self {
            Self::Aes128CmHmacSha1_80 | Self::Aes128CmHmacSha1_32 => 10,
            Self::AeadAes128Gcm | Self::AeadAes256Gcm => 16,
        }
    }

    /// DTLS exporter から取り出す keying material の長さ（RFC 5764 4.2）。
    pub fn keying_material_len(self) -> usize {
        2 * (self.master_key_len() + self.master_salt_len())
    }
}

/// master key と master salt。破棄時に 0 で上書きする。Debug に値を出さない。
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct MasterKey {
    pub(crate) key: Vec<u8>,
    pub(crate) salt: Vec<u8>,
}

impl MasterKey {
    /// 長さを保護方式に照らして作る。
    pub fn new(profile: SrtpProfile, key: &[u8], salt: &[u8]) -> Option<Self> {
        if key.len() != profile.master_key_len() || salt.len() != profile.master_salt_len() {
            return None;
        }
        Some(Self {
            key: key.to_vec(),
            salt: salt.to_vec(),
        })
    }

    /// key と salt を連結した列（SDES の inline 形式など）から作る。
    pub fn from_concatenated(profile: SrtpProfile, key_and_salt: &[u8]) -> Option<Self> {
        let k = profile.master_key_len();
        if key_and_salt.len() != k + profile.master_salt_len() {
            return None;
        }
        Self::new(profile, &key_and_salt[..k], &key_and_salt[k..])
    }
}

impl core::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}
