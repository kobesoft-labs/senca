//! SRT の暗号（HaiCrypt 互換）。
//!
//! - KEK = PBKDF2-HMAC-SHA1(passphrase, salt の末尾 8 バイト, 2048 回, 鍵長)
//! - SEK は RFC 3394 の AES 鍵ラップで KM メッセージに載せる（偶数鍵・奇数鍵）。
//! - 本体は AES-CTR（IV = salt[0..14] XOR (0^10 ‖ パケット番号 ‖ 0^2)）か、
//!   AES-GCM（SRT 1.5.4 形式: 96 ビット IV = salt[0..12] XOR (0^8 ‖ パケット番号)、
//!   AAD = 再送フラグを 0 にした 16 バイトのヘッダ、タグ 16 バイトを末尾に付ける）。
//!
//! AES-CTR は機密性だけで、完全性・送信者認証は持たない（仕様 6.8）。
//! 暗号の演算は RustCrypto の実装を使い、ここでは組み立てだけを行う。

use aes::cipher::{KeyInit, StreamCipher};
use aes_gcm::aead::AeadInPlace;
use aes_gcm::aead::consts::U12;
use aes_gcm::{AesGcm, Nonce, Tag};

use crate::packet::HEADER_LEN;

/// KM メッセージの塩の長さ。
pub const SALT_LEN: usize = 16;
/// AES-GCM の認証タグの長さ。
pub const GCM_TAG_LEN: usize = 16;
/// PBKDF2 の反復回数（HaiCrypt と同じ）。
const PBKDF2_ITER: u32 = 2048;
/// KM メッセージの固定部。
const KM_HDR_LEN: usize = 16;
const KM_SIGN: u16 = 0x2029; // 'HAI' の PnP ベンダー ID
const CIPHER_AES_CTR: u8 = 2;
const CIPHER_AES_GCM: u8 = 4;
const AUTH_NONE: u8 = 0;
const AUTH_AES_GCM: u8 = 1;
const SE_TSSRT: u8 = 2;

/// 鍵長。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyLen {
    Aes128,
    Aes192,
    Aes256,
}

impl KeyLen {
    pub fn bytes(self) -> usize {
        match self {
            KeyLen::Aes128 => 16,
            KeyLen::Aes192 => 24,
            KeyLen::Aes256 => 32,
        }
    }

    pub fn from_bytes(n: usize) -> Option<Self> {
        match n {
            16 => Some(KeyLen::Aes128),
            24 => Some(KeyLen::Aes192),
            32 => Some(KeyLen::Aes256),
            _ => None,
        }
    }
}

/// 暗号方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CipherMode {
    /// 従来の AES-CTR。機密性のみ（認証付き暗号ではない）。
    AesCtr,
    /// AES-GCM（SRT 1.5.4 以降の形式）。認証付き。
    AesGcm,
}

/// 共有パスフレーズ。Debug・Display に中身を出さない。
#[derive(Clone, PartialEq, Eq)]
pub struct Passphrase(String);

impl Passphrase {
    /// libsrt と同じく 10〜79 バイトだけを受け付ける。
    pub fn new(s: impl Into<String>) -> Result<Self, CryptoError> {
        let s = s.into();
        if !(10..=79).contains(&s.len()) {
            return Err(CryptoError::BadPassphraseLength);
        }
        Ok(Passphrase(s))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Passphrase(***)")
    }
}

/// 暗号の失敗。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CryptoError {
    BadPassphraseLength,
    /// KM メッセージの形式が不正。
    MalformedKm,
    /// 対応しない方式（暗号・認証・カプセル化）。
    Unsupported,
    /// 鍵のアンラップに失敗（パスフレーズ違い）。
    BadSecret,
    /// 本体の復号・認証に失敗。
    Decrypt,
    /// 乱数源の失敗。
    Rng,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            CryptoError::BadPassphraseLength => "passphrase must be 10..79 bytes",
            CryptoError::MalformedKm => "malformed keying material",
            CryptoError::Unsupported => "unsupported cipher parameters",
            CryptoError::BadSecret => "wrong passphrase",
            CryptoError::Decrypt => "payload decryption failed",
            CryptoError::Rng => "random source failed",
        })
    }
}

impl std::error::Error for CryptoError {}

pub(crate) fn random_bytes(out: &mut [u8]) -> Result<(), CryptoError> {
    getrandom::fill(out).map_err(|_| CryptoError::Rng)
}

/// 鍵暗号化鍵（AES 鍵ラップ用）。
#[derive(Clone)]
struct Kek {
    key: Vec<u8>,
}

impl Kek {
    fn derive(pass: &Passphrase, salt: &[u8; SALT_LEN], len: KeyLen) -> Kek {
        let mut key = vec![0u8; len.bytes()];
        pbkdf2::pbkdf2_hmac::<sha1::Sha1>(pass.as_bytes(), &salt[SALT_LEN - 8..], PBKDF2_ITER, &mut key);
        Kek { key }
    }

    fn wrap(&self, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut out = vec![0u8; data.len() + 8];
        let r = match self.key.len() {
            16 => aes_kw::KekAes128::try_from(self.key.as_slice()).map(|k| k.wrap(data, &mut out)),
            24 => aes_kw::KekAes192::try_from(self.key.as_slice()).map(|k| k.wrap(data, &mut out)),
            _ => aes_kw::KekAes256::try_from(self.key.as_slice()).map(|k| k.wrap(data, &mut out)),
        };
        match r {
            Ok(Ok(())) => Ok(out),
            _ => Err(CryptoError::Unsupported),
        }
    }

    fn unwrap(&self, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if data.len() < 24 || !data.len().is_multiple_of(8) {
            return Err(CryptoError::MalformedKm);
        }
        let mut out = vec![0u8; data.len() - 8];
        let r = match self.key.len() {
            16 => aes_kw::KekAes128::try_from(self.key.as_slice()).map(|k| k.unwrap(data, &mut out)),
            24 => aes_kw::KekAes192::try_from(self.key.as_slice()).map(|k| k.unwrap(data, &mut out)),
            _ => aes_kw::KekAes256::try_from(self.key.as_slice()).map(|k| k.unwrap(data, &mut out)),
        };
        match r {
            Ok(Ok(())) => Ok(out),
            Ok(Err(_)) => Err(CryptoError::BadSecret),
            Err(_) => Err(CryptoError::Unsupported),
        }
    }
}

type Aes192Gcm = AesGcm<aes::Aes192, U12>;

/// 1 本の SEK で本体を暗号化・復号する。
#[derive(Clone)]
enum SekCipher {
    Ctr128(aes::Aes128),
    Ctr192(aes::Aes192),
    Ctr256(aes::Aes256),
    Gcm128(Box<aes_gcm::Aes128Gcm>),
    Gcm192(Box<Aes192Gcm>),
    Gcm256(Box<aes_gcm::Aes256Gcm>),
}

fn ctr_iv(salt: &[u8; SALT_LEN], pki: u32) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[10..14].copy_from_slice(&pki.to_be_bytes());
    for i in 0..14 {
        iv[i] ^= salt[i];
    }
    iv
}

fn gcm_iv(salt: &[u8; SALT_LEN], pki: u32) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[8..12].copy_from_slice(&pki.to_be_bytes());
    for i in 0..12 {
        iv[i] ^= salt[i];
    }
    iv
}

/// GCM の AAD：再送フラグ（第 2 語のビット 26）を 0 にしたヘッダ。
fn gcm_aad(header: &[u8; HEADER_LEN]) -> [u8; HEADER_LEN] {
    let mut a = *header;
    a[4] &= !0x04;
    a
}

impl SekCipher {
    fn new(mode: CipherMode, key: &[u8]) -> Result<Self, CryptoError> {
        let e = |_| CryptoError::Unsupported;
        Ok(match (mode, key.len()) {
            (CipherMode::AesCtr, 16) => SekCipher::Ctr128(aes::Aes128::new_from_slice(key).map_err(e)?),
            (CipherMode::AesCtr, 24) => SekCipher::Ctr192(aes::Aes192::new_from_slice(key).map_err(e)?),
            (CipherMode::AesCtr, 32) => SekCipher::Ctr256(aes::Aes256::new_from_slice(key).map_err(e)?),
            (CipherMode::AesGcm, 16) => {
                SekCipher::Gcm128(Box::new(aes_gcm::Aes128Gcm::new_from_slice(key).map_err(e)?))
            }
            (CipherMode::AesGcm, 24) => SekCipher::Gcm192(Box::new(Aes192Gcm::new_from_slice(key).map_err(e)?)),
            (CipherMode::AesGcm, 32) => {
                SekCipher::Gcm256(Box::new(aes_gcm::Aes256Gcm::new_from_slice(key).map_err(e)?))
            }
            _ => return Err(CryptoError::Unsupported),
        })
    }

    /// 本体を暗号化する（GCM はタグを末尾に足す）。`header` はこのパケットの送信ヘッダ。
    fn encrypt(&self, salt: &[u8; SALT_LEN], header: &[u8; HEADER_LEN], buf: &mut Vec<u8>) -> Result<(), CryptoError> {
        let pki = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        match self {
            SekCipher::Ctr128(c) => ctr_apply::<aes::Aes128>(c, salt, pki, buf),
            SekCipher::Ctr192(c) => ctr_apply::<aes::Aes192>(c, salt, pki, buf),
            SekCipher::Ctr256(c) => ctr_apply::<aes::Aes256>(c, salt, pki, buf),
            SekCipher::Gcm128(c) => gcm_seal(c.as_ref(), salt, pki, header, buf)?,
            SekCipher::Gcm192(c) => gcm_seal(c.as_ref(), salt, pki, header, buf)?,
            SekCipher::Gcm256(c) => gcm_seal(c.as_ref(), salt, pki, header, buf)?,
        }
        Ok(())
    }

    fn decrypt(&self, salt: &[u8; SALT_LEN], header: &[u8; HEADER_LEN], buf: &mut Vec<u8>) -> Result<(), CryptoError> {
        let pki = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        match self {
            SekCipher::Ctr128(c) => ctr_apply::<aes::Aes128>(c, salt, pki, buf),
            SekCipher::Ctr192(c) => ctr_apply::<aes::Aes192>(c, salt, pki, buf),
            SekCipher::Ctr256(c) => ctr_apply::<aes::Aes256>(c, salt, pki, buf),
            SekCipher::Gcm128(c) => gcm_open(c.as_ref(), salt, pki, header, buf)?,
            SekCipher::Gcm192(c) => gcm_open(c.as_ref(), salt, pki, header, buf)?,
            SekCipher::Gcm256(c) => gcm_open(c.as_ref(), salt, pki, header, buf)?,
        }
        Ok(())
    }
}

fn ctr_apply<C>(c: &C, salt: &[u8; SALT_LEN], pki: u32, buf: &mut [u8])
where
    C: aes::cipher::BlockCipher + aes::cipher::BlockEncrypt + aes::cipher::BlockSizeUser<BlockSize = aes::cipher::consts::U16> + Clone,
{
    use aes::cipher::InnerIvInit;
    let iv = ctr_iv(salt, pki);
    let core = ctr::CtrCore::<C, ctr::flavors::Ctr128BE>::inner_iv_init(c.clone(), &iv.into());
    let mut s = ctr::Ctr128BE::<C>::from_core(core);
    s.apply_keystream(buf);
}

fn gcm_seal<A>(
    c: &A,
    salt: &[u8; SALT_LEN],
    pki: u32,
    header: &[u8; HEADER_LEN],
    buf: &mut Vec<u8>,
) -> Result<(), CryptoError>
where
    A: AeadInPlace + aes_gcm::aead::AeadCore<NonceSize = U12, TagSize = aes_gcm::aead::consts::U16>,
{
    let iv = gcm_iv(salt, pki);
    let tag = c
        .encrypt_in_place_detached(&Nonce::from(iv), &gcm_aad(header), buf)
        .map_err(|_| CryptoError::Decrypt)?;
    buf.extend_from_slice(&tag);
    Ok(())
}

fn gcm_open<A>(
    c: &A,
    salt: &[u8; SALT_LEN],
    pki: u32,
    header: &[u8; HEADER_LEN],
    buf: &mut Vec<u8>,
) -> Result<(), CryptoError>
where
    A: AeadInPlace + aes_gcm::aead::AeadCore<NonceSize = U12, TagSize = aes_gcm::aead::consts::U16>,
{
    if buf.len() < GCM_TAG_LEN {
        return Err(CryptoError::Decrypt);
    }
    let n = buf.len() - GCM_TAG_LEN;
    let mut t = [0u8; GCM_TAG_LEN];
    t.copy_from_slice(&buf[n..]);
    let tag = Tag::from(t);
    buf.truncate(n);
    let iv = gcm_iv(salt, pki);
    c.decrypt_in_place_detached(&Nonce::from(iv), &gcm_aad(header), buf, &tag)
        .map_err(|_| CryptoError::Decrypt)
}

/// 解析した KM メッセージ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KmMsg {
    pub mode: CipherMode,
    pub key_len: KeyLen,
    pub salt: [u8; SALT_LEN],
    /// 1=偶数鍵のみ、2=奇数鍵のみ、3=両方。
    pub kk: u8,
    pub wrapped: Vec<u8>,
}

impl KmMsg {
    pub fn parse(b: &[u8]) -> Result<Self, CryptoError> {
        if b.len() < KM_HDR_LEN {
            return Err(CryptoError::MalformedKm);
        }
        let version = b[0] >> 4;
        let pt = b[0] & 0x0F;
        let sign = u16::from_be_bytes([b[1], b[2]]);
        if version != 1 || pt != 2 || sign != KM_SIGN {
            return Err(CryptoError::MalformedKm);
        }
        let kk = b[3] & 3;
        if kk == 0 || b[3] & !3 != 0 {
            return Err(CryptoError::MalformedKm);
        }
        if b[4..8] != [0, 0, 0, 0] {
            // KEKI は 0（パスフレーズ由来の KEK）だけ。
            return Err(CryptoError::Unsupported);
        }
        let mode = match (b[8], b[9]) {
            (CIPHER_AES_CTR, AUTH_NONE) => CipherMode::AesCtr,
            (CIPHER_AES_GCM, AUTH_AES_GCM) => CipherMode::AesGcm,
            _ => return Err(CryptoError::Unsupported),
        };
        if b[10] != SE_TSSRT {
            return Err(CryptoError::Unsupported);
        }
        let slen = b[14] as usize * 4;
        let klen = b[15] as usize * 4;
        if slen != SALT_LEN {
            return Err(CryptoError::MalformedKm);
        }
        let key_len = KeyLen::from_bytes(klen).ok_or(CryptoError::MalformedKm)?;
        let nkeys = if kk == 3 { 2 } else { 1 };
        if b.len() != KM_HDR_LEN + slen + nkeys * klen + 8 {
            return Err(CryptoError::MalformedKm);
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&b[KM_HDR_LEN..KM_HDR_LEN + SALT_LEN]);
        Ok(KmMsg { mode, key_len, salt, kk, wrapped: b[KM_HDR_LEN + SALT_LEN..].to_vec() })
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(KM_HDR_LEN + SALT_LEN + self.wrapped.len());
        v.push(0x12);
        v.extend_from_slice(&KM_SIGN.to_be_bytes());
        v.push(self.kk);
        v.extend_from_slice(&[0, 0, 0, 0]);
        let (c, a) = match self.mode {
            CipherMode::AesCtr => (CIPHER_AES_CTR, AUTH_NONE),
            CipherMode::AesGcm => (CIPHER_AES_GCM, AUTH_AES_GCM),
        };
        v.extend_from_slice(&[c, a, SE_TSSRT, 0, 0, 0]);
        v.push((SALT_LEN / 4) as u8);
        v.push((self.key_len.bytes() / 4) as u8);
        v.extend_from_slice(&self.salt);
        v.extend_from_slice(&self.wrapped);
        v
    }
}

#[derive(Clone)]
struct Sek {
    cipher: SekCipher,
}

/// 送信側の暗号状態（鍵の更新を含む）。
pub(crate) struct TxCrypto {
    mode: CipherMode,
    key_len: KeyLen,
    salt: [u8; SALT_LEN],
    kek: Kek,
    /// [偶数, 奇数] の SEK（生の鍵バイトも KM 組み立て用に持つ）。
    keys: [Option<(Vec<u8>, Sek)>; 2],
    active: usize,
    pkt_cnt: u64,
    refresh_rate: u64,
    preannounce: u64,
    phase: TxPhase,
    /// 相手へ告知する最新の KM メッセージ。
    km: Vec<u8>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TxPhase {
    /// 現行鍵だけ。
    Steady,
    /// 次の鍵を告知済み（両方の鍵を含む KM）。
    Announced,
    /// 切り替え後、古い鍵の廃止を待つ。
    Switched,
}

/// 送信で起きた鍵の出来事。
pub(crate) enum TxKmEvent {
    /// 新しい KM を相手へ送る（KMREQ）。
    Announce(Vec<u8>),
}

impl TxCrypto {
    /// 新しい塩と SEK を作る（開始側）。
    pub fn new(
        pass: &Passphrase,
        key_len: KeyLen,
        mode: CipherMode,
        refresh_rate: u64,
        preannounce: u64,
    ) -> Result<Self, CryptoError> {
        let mut salt = [0u8; SALT_LEN];
        random_bytes(&mut salt)?;
        let kek = Kek::derive(pass, &salt, key_len);
        let mut key = vec![0u8; key_len.bytes()];
        random_bytes(&mut key)?;
        let sek = Sek { cipher: SekCipher::new(mode, &key)? };
        let (refresh_rate, preannounce) = normalize_refresh(refresh_rate, preannounce);
        let mut t = TxCrypto {
            mode,
            key_len,
            salt,
            kek,
            keys: [Some((key, sek)), None],
            active: 0,
            pkt_cnt: 0,
            refresh_rate,
            preannounce,
            phase: TxPhase::Steady,
            km: Vec::new(),
        };
        t.km = t.assemble_km()?;
        Ok(t)
    }

    /// 応答側：相手の KMREQ と同じ鍵で送信する（libsrt の HSv5 と同じ）。
    fn from_rx(rx: &RxCrypto, km: &[u8], refresh_rate: u64, preannounce: u64) -> Option<Self> {
        let kek = rx.kek.as_ref()?.2.clone();
        let (idx, key) = rx.raw.iter().enumerate().find_map(|(i, k)| k.as_ref().map(|k| (i, k.clone())))?;
        let sek = Sek { cipher: SekCipher::new(rx.mode, &key).ok()? };
        let mut keys: [Option<(Vec<u8>, Sek)>; 2] = [None, None];
        keys[idx] = Some((key, sek));
        let (refresh_rate, preannounce) = normalize_refresh(refresh_rate, preannounce);
        Some(TxCrypto {
            mode: rx.mode,
            key_len: rx.key_len,
            salt: rx.salt,
            kek,
            keys,
            active: idx,
            pkt_cnt: 0,
            refresh_rate,
            preannounce,
            phase: TxPhase::Steady,
            km: km.to_vec(),
        })
    }

    pub fn km_msg(&self) -> &[u8] {
        &self.km
    }

    pub fn mode(&self) -> CipherMode {
        self.mode
    }

    pub fn key_len(&self) -> KeyLen {
        self.key_len
    }

    fn assemble_km(&self) -> Result<Vec<u8>, CryptoError> {
        let mut plain = Vec::with_capacity(2 * self.key_len.bytes());
        let mut kk = 0u8;
        for (i, k) in self.keys.iter().enumerate() {
            if let Some((raw, _)) = k {
                plain.extend_from_slice(raw);
                kk |= 1 << i;
            }
        }
        let wrapped = self.kek.wrap(&plain)?;
        Ok(KmMsg { mode: self.mode, key_len: self.key_len, salt: self.salt, kk, wrapped }.to_bytes())
    }

    /// 1 パケットを暗号化し、ヘッダの KK を返す。鍵の更新が要るときは出来事も返す。
    pub fn encrypt(
        &mut self,
        header: &mut [u8; HEADER_LEN],
        buf: &mut Vec<u8>,
    ) -> Result<Option<TxKmEvent>, CryptoError> {
        let mut ev = None;
        self.pkt_cnt += 1;
        if self.refresh_rate > 0 {
            match self.phase {
                TxPhase::Steady if self.pkt_cnt >= self.refresh_rate - self.preannounce => {
                    let alt = 1 - self.active;
                    let mut key = vec![0u8; self.key_len.bytes()];
                    random_bytes(&mut key)?;
                    let sek = Sek { cipher: SekCipher::new(self.mode, &key)? };
                    self.keys[alt] = Some((key, sek));
                    self.km = self.assemble_km()?;
                    self.phase = TxPhase::Announced;
                    ev = Some(TxKmEvent::Announce(self.km.clone()));
                }
                TxPhase::Announced if self.pkt_cnt >= self.refresh_rate => {
                    self.active = 1 - self.active;
                    self.pkt_cnt = 1;
                    self.phase = TxPhase::Switched;
                }
                TxPhase::Switched if self.pkt_cnt >= self.preannounce => {
                    self.keys[1 - self.active] = None;
                    self.km = self.assemble_km()?;
                    self.phase = TxPhase::Steady;
                    ev = Some(TxKmEvent::Announce(self.km.clone()));
                }
                _ => {}
            }
        }
        let kk = (self.active + 1) as u8;
        // KK はヘッダの第 2 語のビット 28..27。
        header[4] = (header[4] & !0x18) | (kk << 3);
        let sek = self.keys[self.active].as_ref().ok_or(CryptoError::Unsupported)?;
        sek.1.cipher.encrypt(&self.salt, header, buf)?;
        Ok(ev)
    }
}

fn normalize_refresh(refresh_rate: u64, preannounce: u64) -> (u64, u64) {
    if refresh_rate == 0 {
        return (0, 0);
    }
    let pre = preannounce.clamp(1, (refresh_rate / 2).max(1));
    (refresh_rate.max(2), pre)
}

/// 受信側の暗号状態。
pub(crate) struct RxCrypto {
    pass: Passphrase,
    mode: CipherMode,
    key_len: KeyLen,
    salt: [u8; SALT_LEN],
    /// (塩, 鍵長, KEK) の 1 件だけのキャッシュ。
    kek: Option<([u8; SALT_LEN], KeyLen, Kek)>,
    raw: [Option<Vec<u8>>; 2],
    keys: [Option<SekCipher>; 2],
}

impl RxCrypto {
    /// KM メッセージ（相手の KMREQ）から作る。
    pub fn from_km(pass: &Passphrase, km: &[u8]) -> Result<Self, CryptoError> {
        let msg = KmMsg::parse(km)?;
        let mut rx = RxCrypto {
            pass: pass.clone(),
            mode: msg.mode,
            key_len: msg.key_len,
            salt: msg.salt,
            kek: None,
            raw: [None, None],
            keys: [None, None],
        };
        rx.apply(&msg)?;
        Ok(rx)
    }

    /// 自分の送信鍵と同じ鍵で受信する（開始側。libsrt の HSv5 と同じ）。
    pub fn from_tx(pass: &Passphrase, tx: &TxCrypto) -> Self {
        let mut raw: [Option<Vec<u8>>; 2] = [None, None];
        let mut keys: [Option<SekCipher>; 2] = [None, None];
        for i in 0..2 {
            if let Some((r, s)) = &tx.keys[i] {
                raw[i] = Some(r.clone());
                keys[i] = Some(s.cipher.clone());
            }
        }
        RxCrypto {
            pass: pass.clone(),
            mode: tx.mode,
            key_len: tx.key_len,
            salt: tx.salt,
            kek: Some((tx.salt, tx.key_len, tx.kek.clone())),
            raw,
            keys,
        }
    }

    pub fn mode(&self) -> CipherMode {
        self.mode
    }

    /// 接続中に届いた KMREQ を処理する。方式の変更は受け付けない。
    pub fn process_km(&mut self, km: &[u8]) -> Result<(), CryptoError> {
        let msg = KmMsg::parse(km)?;
        if msg.mode != self.mode {
            return Err(CryptoError::Unsupported);
        }
        self.apply(&msg)
    }

    fn apply(&mut self, msg: &KmMsg) -> Result<(), CryptoError> {
        let kek = match &self.kek {
            Some((s, l, k)) if *s == msg.salt && *l == msg.key_len => k.clone(),
            _ => {
                let k = Kek::derive(&self.pass, &msg.salt, msg.key_len);
                self.kek = Some((msg.salt, msg.key_len, k.clone()));
                k
            }
        };
        let plain = kek.unwrap(&msg.wrapped)?;
        let klen = msg.key_len.bytes();
        let mut it = plain.chunks(klen);
        for i in 0..2 {
            if msg.kk & (1 << i) != 0 {
                let raw = it.next().ok_or(CryptoError::MalformedKm)?;
                self.keys[i] = Some(SekCipher::new(msg.mode, raw)?);
                self.raw[i] = Some(raw.to_vec());
            }
        }
        self.salt = msg.salt;
        self.key_len = msg.key_len;
        Ok(())
    }

    /// 本体を復号する。`kk` はヘッダの鍵フラグ（1=偶数、2=奇数）。
    pub fn decrypt(&self, kk: u8, header: &[u8; HEADER_LEN], buf: &mut Vec<u8>) -> Result<(), CryptoError> {
        let idx = match kk {
            1 => 0,
            2 => 1,
            _ => return Err(CryptoError::Decrypt),
        };
        let c = self.keys[idx].as_ref().ok_or(CryptoError::Decrypt)?;
        c.decrypt(&self.salt, header, buf)
    }

    /// 応答側の送信状態を作る。
    pub fn make_tx(&self, km: &[u8], refresh_rate: u64, preannounce: u64) -> Option<TxCrypto> {
        TxCrypto::from_rx(self, km, refresh_rate, preannounce)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(seq: u32) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..4].copy_from_slice(&seq.to_be_bytes());
        h[4] = 0xC0;
        h
    }

    #[test]
    fn rfc3394_vector() {
        // RFC 3394 4.1: 128 ビット KEK で 128 ビットの鍵をラップ
        let kek = Kek { key: (0u8..16).collect() };
        let data: Vec<u8> = vec![
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        ];
        let w = kek.wrap(&data).unwrap();
        assert_eq!(
            w,
            vec![
                0x1F, 0xA6, 0x8B, 0x0A, 0x81, 0x12, 0xB4, 0x47, 0xAE, 0xF3, 0x4B, 0xD8, 0xFB, 0x5A, 0x7B, 0x82, 0x9D,
                0x3E, 0x86, 0x23, 0x71, 0xD2, 0xCF, 0xE5
            ]
        );
        assert_eq!(kek.unwrap(&w).unwrap(), data);
    }

    #[test]
    fn km_roundtrip_and_bad_secret() {
        for (kl, mode) in [
            (KeyLen::Aes128, CipherMode::AesCtr),
            (KeyLen::Aes192, CipherMode::AesCtr),
            (KeyLen::Aes256, CipherMode::AesGcm),
        ] {
            let pass = Passphrase::new("0123456789abc").unwrap();
            let mut tx = TxCrypto::new(&pass, kl, mode, 0, 0).unwrap();
            let km = tx.km_msg().to_vec();
            assert_eq!(km.len(), 16 + 16 + kl.bytes() + 8);
            let rx = RxCrypto::from_km(&pass, &km).unwrap();
            let mut h = hdr(42);
            let mut body = b"hello world, this is a ts payload".to_vec();
            tx.encrypt(&mut h, &mut body).unwrap();
            assert_ne!(&body[..5], b"hello");
            let kk = (h[4] >> 3) & 3;
            assert_eq!(kk, 1);
            // 再送フラグは GCM の AAD に入らない
            h[4] |= 0x04;
            rx.decrypt(kk, &h, &mut body).unwrap();
            assert_eq!(body, b"hello world, this is a ts payload");

            let wrong = Passphrase::new("0123456789abd").unwrap();
            assert_eq!(RxCrypto::from_km(&wrong, &km).err(), Some(CryptoError::BadSecret));
        }
    }

    #[test]
    fn gcm_tamper_detected() {
        let pass = Passphrase::new("0123456789abc").unwrap();
        let mut tx = TxCrypto::new(&pass, KeyLen::Aes128, CipherMode::AesGcm, 0, 0).unwrap();
        let rx = RxCrypto::from_km(&pass, tx.km_msg()).unwrap();
        let mut h = hdr(7);
        let mut body = vec![9u8; 100];
        tx.encrypt(&mut h, &mut body).unwrap();
        assert_eq!(body.len(), 116);
        let mut b2 = body.clone();
        b2[3] ^= 1;
        assert!(rx.decrypt(1, &h, &mut b2).is_err());
        let mut h2 = h;
        h2[11] ^= 1; // タイムスタンプの改ざん
        let mut b3 = body.clone();
        assert!(rx.decrypt(1, &h2, &mut b3).is_err());
    }

    #[test]
    fn rotation_even_odd() {
        let pass = Passphrase::new("0123456789abc").unwrap();
        let mut tx = TxCrypto::new(&pass, KeyLen::Aes128, CipherMode::AesCtr, 20, 5).unwrap();
        let mut rx = RxCrypto::from_km(&pass, tx.km_msg()).unwrap();
        let mut announced = 0;
        let mut seen_kk = std::collections::BTreeSet::new();
        for i in 0..100u32 {
            let mut h = hdr(i);
            let mut b = i.to_be_bytes().to_vec();
            if let Some(TxKmEvent::Announce(km)) = tx.encrypt(&mut h, &mut b).unwrap() {
                announced += 1;
                rx.process_km(&km).unwrap();
            }
            let kk = (h[4] >> 3) & 3;
            seen_kk.insert(kk);
            rx.decrypt(kk, &h, &mut b).unwrap();
            assert_eq!(b, i.to_be_bytes());
        }
        assert!(announced >= 8);
        assert_eq!(seen_kk.len(), 2);
    }

    #[test]
    fn km_parse_rejects_garbage() {
        let pass = Passphrase::new("0123456789abc").unwrap();
        let tx = TxCrypto::new(&pass, KeyLen::Aes128, CipherMode::AesCtr, 0, 0).unwrap();
        let km = tx.km_msg().to_vec();
        for n in 0..km.len() {
            assert!(KmMsg::parse(&km[..n]).is_err());
        }
        for i in 0..16 {
            let mut k = km.clone();
            k[i] ^= 0xFF;
            let _ = RxCrypto::from_km(&pass, &k);
        }
        assert!(Passphrase::new("short").is_err());
    }
}
