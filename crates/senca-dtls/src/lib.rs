//! DTLS 1.2（RFC 6347）。WebRTC の DTLS-SRTP（RFC 5763・RFC 5764・RFC 8827・RFC 8842）に要る範囲。
//!
//! I/O・スレッド・時計を持たない。受け取った UDP datagram を [`DtlsEndpoint::handle_datagram`] に入れ、
//! [`DtlsEndpoint::poll_transmit`] で送る datagram を、[`DtlsEndpoint::poll_event`] で出来事を取り出す。
//! 時刻は引数で渡し、[`DtlsEndpoint::poll_timeout`] の時刻に [`DtlsEndpoint::handle_timeout`] を呼ぶ。
//!
//! ```no_run
//! use std::time::Instant;
//! use senca_dtls::{DtlsCertificate, DtlsConfig, DtlsEndpoint, DtlsEvent};
//! # fn recv() -> Option<Vec<u8>> { None }
//! # fn send(_: &[u8]) {}
//! let cert = DtlsCertificate::generate_self_signed("senca").unwrap();
//! let mut ep = DtlsEndpoint::new(DtlsConfig::client(cert), Instant::now()).unwrap();
//! loop {
//!     while let Some(d) = ep.poll_transmit() { send(&d); }
//!     while let Some(ev) = ep.poll_event() {
//!         if let DtlsEvent::Connected = ev {
//!             // SDP の a=fingerprint と照合してから使う
//!             let _ok = ep.verify_peer_fingerprint(&[0u8; 32]);
//!             let _km = ep.export_srtp_keying_material();
//!         }
//!     }
//!     match recv() {
//!         Some(d) => { let _ = ep.handle_datagram(&d, Instant::now()); }
//!         None => { let _ = ep.handle_timeout(Instant::now()); }
//!     }
//! #   break;
//! }
//! ```
//!
//! 対応するもの:
//! - cipher suite: `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`（必須）・`..._AES_256_GCM_SHA384`・
//!   `..._CHACHA20_POLY1305_SHA256`。鍵交換は X25519 と P-256、証明書は ECDSA P-256。
//! - extended master secret（RFC 7627）は必須。無い相手は拒否する。
//! - renegotiation_info は空だけ。再ネゴシエーションは拒否（no_renegotiation）。圧縮は拒否。
//! - use_srtp（RFC 5764）と exporter（RFC 5705）。
//! - HelloVerifyRequest の cookie（HMAC、状態を持たない）、handshake の分割と再構成、
//!   flight の再送（RFC 6347 4.2.4、指数 backoff）、replay window（64）、alert、close_notify。
//!
//! 暗号の演算（AES-GCM・ChaCha20-Poly1305・SHA-2・HMAC・ECDSA・ECDH）は RustCrypto と dalek を使い、
//! handshake の状態機械・record 層・鍵スケジュール・再送は自前（ADR-0002）。公開前に独立監査を受ける。

mod cert;
mod codec;
mod cookie;
mod endpoint;
mod handshake;
mod prf;
mod record;

pub use cert::{DtlsCertificate, fingerprint_matches, format_fingerprint, parse_fingerprint};
pub use cookie::{CookieDecision, CookieKey, check_client_hello};
pub use endpoint::{DtlsEndpoint, DtlsEvent};
pub use prf::{PrfHash, prf};

use std::time::Duration;

/// DTLS-SRTP の exporter label（RFC 5764 4.2）。
pub const SRTP_EXPORTER_LABEL: &[u8] = b"EXTRACTOR-dtls_srtp";
/// 既定の MTU（UDP payload の上限）。
pub const DEFAULT_MTU: usize = 1200;
/// 設定できる MTU の下限。
pub const MIN_MTU: usize = 256;

/// 役割。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// handshake を始める側。
    Client,
    /// ClientHello を待つ側。
    Server,
}

/// cipher suite。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CipherSuite {
    /// TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256（0xC02B）。WebRTC の必須。
    EcdheEcdsaAes128GcmSha256,
    /// TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384（0xC02C）。
    EcdheEcdsaAes256GcmSha384,
    /// TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256（0xCCA9、RFC 7905）。
    EcdheEcdsaChacha20Poly1305Sha256,
}

impl CipherSuite {
    /// IANA の値。
    pub fn id(self) -> u16 {
        match self {
            Self::EcdheEcdsaAes128GcmSha256 => 0xC02B,
            Self::EcdheEcdsaAes256GcmSha384 => 0xC02C,
            Self::EcdheEcdsaChacha20Poly1305Sha256 => 0xCCA9,
        }
    }

    /// IANA の値から引く。
    pub fn from_id(id: u16) -> Option<Self> {
        Some(match id {
            0xC02B => Self::EcdheEcdsaAes128GcmSha256,
            0xC02C => Self::EcdheEcdsaAes256GcmSha384,
            0xCCA9 => Self::EcdheEcdsaChacha20Poly1305Sha256,
            _ => return None,
        })
    }

    /// PRF のハッシュ。
    pub fn prf_hash(self) -> PrfHash {
        match self {
            Self::EcdheEcdsaAes256GcmSha384 => PrfHash::Sha384,
            _ => PrfHash::Sha256,
        }
    }

    pub(crate) fn key_len(self) -> usize {
        match self {
            Self::EcdheEcdsaAes128GcmSha256 => 16,
            _ => 32,
        }
    }

    /// 鍵 block の fixed IV の長さ（GCM は 4、ChaCha20-Poly1305 は 12）。
    pub(crate) fn fixed_iv_len(self) -> usize {
        match self {
            Self::EcdheEcdsaChacha20Poly1305Sha256 => 12,
            _ => 4,
        }
    }

    /// 暗号化で増える byte 数（explicit nonce + tag）。
    pub(crate) fn overhead(self) -> usize {
        match self {
            Self::EcdheEcdsaChacha20Poly1305Sha256 => 16,
            _ => 8 + 16,
        }
    }
}

/// ECDHE の曲線（supported_groups）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NamedGroup {
    /// x25519（29）。
    X25519,
    /// secp256r1（23）。
    Secp256r1,
}

impl NamedGroup {
    /// IANA の値。
    pub fn id(self) -> u16 {
        match self {
            Self::X25519 => 29,
            Self::Secp256r1 => 23,
        }
    }

    /// IANA の値から引く。
    pub fn from_id(id: u16) -> Option<Self> {
        match id {
            29 => Some(Self::X25519),
            23 => Some(Self::Secp256r1),
            _ => None,
        }
    }
}

/// DTLS-SRTP の保護方式（RFC 5764 4.1.2・RFC 7714 14.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SrtpProtectionProfile {
    /// SRTP_AES128_CM_HMAC_SHA1_80（0x0001）。
    Aes128CmHmacSha1_80,
    /// SRTP_AES128_CM_HMAC_SHA1_32（0x0002）。
    Aes128CmHmacSha1_32,
    /// SRTP_AEAD_AES_128_GCM（0x0007）。
    AeadAes128Gcm,
    /// SRTP_AEAD_AES_256_GCM（0x0008）。
    AeadAes256Gcm,
}

impl SrtpProtectionProfile {
    /// IANA の値。
    pub fn id(self) -> u16 {
        match self {
            Self::Aes128CmHmacSha1_80 => 0x0001,
            Self::Aes128CmHmacSha1_32 => 0x0002,
            Self::AeadAes128Gcm => 0x0007,
            Self::AeadAes256Gcm => 0x0008,
        }
    }

    /// IANA の値から引く。
    pub fn from_id(id: u16) -> Option<Self> {
        Some(match id {
            0x0001 => Self::Aes128CmHmacSha1_80,
            0x0002 => Self::Aes128CmHmacSha1_32,
            0x0007 => Self::AeadAes128Gcm,
            0x0008 => Self::AeadAes256Gcm,
            _ => return None,
        })
    }

    /// exporter から取り出す長さ: 2 × (master key + master salt)。
    pub fn keying_material_len(self) -> usize {
        match self {
            Self::Aes128CmHmacSha1_80 | Self::Aes128CmHmacSha1_32 => 2 * (16 + 14),
            Self::AeadAes128Gcm => 2 * (16 + 12),
            Self::AeadAes256Gcm => 2 * (32 + 12),
        }
    }
}

/// alert の種類（RFC 5246 7.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertDescription {
    CloseNotify,
    UnexpectedMessage,
    BadRecordMac,
    HandshakeFailure,
    BadCertificate,
    UnsupportedCertificate,
    CertificateUnknown,
    IllegalParameter,
    DecodeError,
    DecryptError,
    ProtocolVersion,
    InsufficientSecurity,
    InternalError,
    NoRenegotiation,
    UnsupportedExtension,
    Other(u8),
}

impl AlertDescription {
    /// 値。
    pub fn code(self) -> u8 {
        match self {
            Self::CloseNotify => 0,
            Self::UnexpectedMessage => 10,
            Self::BadRecordMac => 20,
            Self::HandshakeFailure => 40,
            Self::BadCertificate => 42,
            Self::UnsupportedCertificate => 43,
            Self::CertificateUnknown => 46,
            Self::IllegalParameter => 47,
            Self::DecodeError => 50,
            Self::DecryptError => 51,
            Self::ProtocolVersion => 70,
            Self::InsufficientSecurity => 71,
            Self::InternalError => 80,
            Self::NoRenegotiation => 100,
            Self::UnsupportedExtension => 110,
            Self::Other(c) => c,
        }
    }

    /// 値から引く。
    pub fn from_code(c: u8) -> Self {
        match c {
            0 => Self::CloseNotify,
            10 => Self::UnexpectedMessage,
            20 => Self::BadRecordMac,
            40 => Self::HandshakeFailure,
            42 => Self::BadCertificate,
            43 => Self::UnsupportedCertificate,
            46 => Self::CertificateUnknown,
            47 => Self::IllegalParameter,
            50 => Self::DecodeError,
            51 => Self::DecryptError,
            70 => Self::ProtocolVersion,
            71 => Self::InsufficientSecurity,
            80 => Self::InternalError,
            100 => Self::NoRenegotiation,
            110 => Self::UnsupportedExtension,
            c => Self::Other(c),
        }
    }
}

/// DTLS の誤り。秘密は含めない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DtlsError {
    /// 設定が不正。
    InvalidConfig(&'static str),
    /// OS の乱数源が使えない（セッションを確立しない。仕様 9.3）。
    RandomUnavailable,
    /// 証明書・秘密鍵の扱いに失敗した。
    Certificate(&'static str),
    /// こちらが handshake を中断した（相手に fatal alert を送った）。
    Fatal {
        alert: AlertDescription,
        reason: &'static str,
    },
    /// 相手から fatal alert を受けた。
    PeerAlert(AlertDescription),
    /// handshake が時間内に終わらない・再送の上限に達した。
    Timeout,
    /// 接続していない。
    NotConnected,
    /// 閉じている（close_notify の後・失敗の後）。
    Closed,
    /// 送るデータが大きすぎる。
    TooLarge,
    /// record の sequence number を使い切った（鍵を変えずに送り続けない）。
    SequenceExhausted,
    /// exporter の label・長さが不正。
    InvalidExporter,
}

impl core::fmt::Display for DtlsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidConfig(s) => write!(f, "設定が不正: {s}"),
            Self::RandomUnavailable => f.write_str("乱数源が使えない"),
            Self::Certificate(s) => write!(f, "証明書: {s}"),
            Self::Fatal { alert, reason } => write!(f, "handshake を中断: {reason} ({alert:?})"),
            Self::PeerAlert(a) => write!(f, "相手の fatal alert: {a:?}"),
            Self::Timeout => f.write_str("時間切れ"),
            Self::NotConnected => f.write_str("接続していない"),
            Self::Closed => f.write_str("閉じている"),
            Self::TooLarge => f.write_str("データが大きすぎる"),
            Self::SequenceExhausted => f.write_str("sequence number を使い切った"),
            Self::InvalidExporter => f.write_str("exporter の指定が不正"),
        }
    }
}

impl std::error::Error for DtlsError {}

/// endpoint の設定。
#[derive(Debug, Clone)]
pub struct DtlsConfig {
    /// 役割。
    pub role: Role,
    /// 自分の証明書。
    pub certificate: DtlsCertificate,
    /// cipher suite（優先順）。
    pub cipher_suites: Vec<CipherSuite>,
    /// ECDHE の曲線（優先順）。client は secp256r1 を必ず含める（ECDSA P-256 の証明書のため。RFC 8422 5.1）。
    pub groups: Vec<NamedGroup>,
    /// SRTP の保護方式（優先順）。空なら use_srtp を使わない。
    pub srtp_profiles: Vec<SrtpProtectionProfile>,
    /// datagram の上限（byte）。
    pub mtu: usize,
    /// 最初の再送待ち時間。
    pub initial_retransmit_timeout: Duration,
    /// 再送待ち時間の上限（RFC 6347 4.2.4.1 は 60 秒）。
    pub max_retransmit_timeout: Duration,
    /// 1 つの flight を再送する回数の上限。
    pub max_retransmissions: u32,
    /// handshake 全体の制限時間。
    pub handshake_timeout: Duration,
    /// server: 相手の証明書を求める（WebRTC では必須）。
    pub require_peer_certificate: bool,
    /// server: HelloVerifyRequest の cookie を求める。`None` なら cookie 交換をしない。
    pub cookie_key: Option<CookieKey>,
    /// server: cookie に結び付ける相手の輸送識別子（IP とポートなど）。
    pub peer_transport_id: Vec<u8>,
    /// 相手証明書の SHA-256 fingerprint（SDP の a=fingerprint）。指定すると handshake 中に照合し、
    /// 一致しなければ bad_certificate で中断する。
    pub expected_peer_fingerprint: Option<[u8; 32]>,
}

impl DtlsConfig {
    fn base(role: Role, certificate: DtlsCertificate) -> Self {
        Self {
            role,
            certificate,
            cipher_suites: vec![
                CipherSuite::EcdheEcdsaAes128GcmSha256,
                CipherSuite::EcdheEcdsaAes256GcmSha384,
                CipherSuite::EcdheEcdsaChacha20Poly1305Sha256,
            ],
            groups: vec![NamedGroup::X25519, NamedGroup::Secp256r1],
            srtp_profiles: vec![
                SrtpProtectionProfile::AeadAes128Gcm,
                SrtpProtectionProfile::Aes128CmHmacSha1_80,
            ],
            mtu: DEFAULT_MTU,
            initial_retransmit_timeout: Duration::from_secs(1),
            max_retransmit_timeout: Duration::from_secs(60),
            max_retransmissions: 8,
            handshake_timeout: Duration::from_secs(30),
            require_peer_certificate: true,
            cookie_key: None,
            peer_transport_id: Vec::new(),
            expected_peer_fingerprint: None,
        }
    }

    /// client の既定。
    pub fn client(certificate: DtlsCertificate) -> Self {
        Self::base(Role::Client, certificate)
    }

    /// server の既定（cookie 交換なし）。
    pub fn server(certificate: DtlsCertificate) -> Self {
        Self::base(Role::Server, certificate)
    }

    pub(crate) fn validate(&self) -> Result<(), DtlsError> {
        if self.cipher_suites.is_empty() {
            return Err(DtlsError::InvalidConfig("cipher suite が空"));
        }
        if self.groups.is_empty() {
            return Err(DtlsError::InvalidConfig("曲線が空"));
        }
        // supported_groups は ECDSA の曲線も表す（RFC 8422 5.1）。P-256 証明書を使うので client は必ず示す。
        if self.role == Role::Client && !self.groups.contains(&NamedGroup::Secp256r1) {
            return Err(DtlsError::InvalidConfig(
                "client の曲線に secp256r1 が要る（ECDSA P-256 証明書）",
            ));
        }
        if !(MIN_MTU..=65507).contains(&self.mtu) {
            return Err(DtlsError::InvalidConfig("MTU が範囲外"));
        }
        if self.initial_retransmit_timeout.is_zero()
            || self.max_retransmit_timeout < self.initial_retransmit_timeout
        {
            return Err(DtlsError::InvalidConfig("再送時間が不正"));
        }
        if self.srtp_profiles.len() > 16 || self.cipher_suites.len() > 16 || self.groups.len() > 16
        {
            return Err(DtlsError::InvalidConfig("一覧が長すぎる"));
        }
        Ok(())
    }
}
