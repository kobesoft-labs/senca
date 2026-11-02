//! SRTP/SRTCP の暗号 context（RFC 3711 3.2）。

use std::collections::HashMap;

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit, Nonce, Tag};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::kdf::{self, aes_cm_prf, aes_cm_xor};
use crate::profile::{MasterKey, SrtpProfile};
use crate::replay::{ReplayCheck, ReplayWindow};

/// 1 つの master key で保護できる SRTP packet 数の上限（RFC 3711 9.2）。
const SRTP_PACKET_LIMIT: u64 = 1 << 48;
/// 1 つの master key で保護できる SRTCP packet 数の上限（RFC 3711 9.2）。SRTCP index は 31 bit。
const SRTCP_PACKET_LIMIT: u64 = 1 << 31;
/// HMAC-SHA1 の session 認証鍵の長さ。
const AUTH_KEY_LEN: usize = 20;
/// SRTCP の E flag。
const E_FLAG: u32 = 0x8000_0000;

/// context の方向。DTLS-SRTP では方向ごとに鍵が違う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// 送信（protect だけ）。
    Outbound,
    /// 受信（unprotect だけ）。
    Inbound,
}

/// SRTP の誤り。秘密は含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SrtpError {
    /// 鍵・salt の長さが保護方式に合わない。
    InvalidKey,
    /// packet が短い。
    TooShort,
    /// RTP/RTCP header が壊れている。
    InvalidHeader,
    /// 認証 tag が一致しない。
    AuthenticationFailed,
    /// 既に受け付けた packet（replay）。
    Replayed,
    /// replay window より古い。
    TooOld,
    /// 同じ鍵で同じ index（nonce）を再び使おうとした。
    IndexReuse,
    /// 鍵の寿命（packet 数・ROC）に達した。鍵を更新するまで保護しない。
    KeyLifetimeExceeded,
    /// SSRC の数が上限を超える。
    TooManyStreams,
    /// context の方向と違う操作。
    WrongDirection,
}

impl core::fmt::Display for SrtpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            Self::InvalidKey => "鍵の長さが保護方式に合わない",
            Self::TooShort => "packet が短い",
            Self::InvalidHeader => "header が不正",
            Self::AuthenticationFailed => "認証に失敗",
            Self::Replayed => "replay",
            Self::TooOld => "replay window より古い",
            Self::IndexReuse => "index の再使用",
            Self::KeyLifetimeExceeded => "鍵の寿命に達した",
            Self::TooManyStreams => "SSRC が多すぎる",
            Self::WrongDirection => "context の方向が違う",
        };
        f.write_str(s)
    }
}

impl std::error::Error for SrtpError {}

/// 導出済みの session key。
#[derive(Zeroize, ZeroizeOnDrop)]
struct SessionKeys {
    enc: Vec<u8>,
    /// HMAC-SHA1 の鍵。AEAD では空。
    auth: Vec<u8>,
    /// AES-CM は 14 byte、GCM は 12 byte。
    salt: Vec<u8>,
}

impl SessionKeys {
    fn derive(profile: SrtpProfile, mk: &MasterKey, labels: [u8; 3]) -> Option<Self> {
        let mut enc = vec![0u8; profile.master_key_len()];
        let mut salt = vec![0u8; profile.master_salt_len()];
        let mut auth = if profile.is_aead() {
            Vec::new()
        } else {
            vec![0u8; AUTH_KEY_LEN]
        };
        let ok = aes_cm_prf(&mk.key, &mk.salt, labels[0], &mut enc)
            && aes_cm_prf(&mk.key, &mk.salt, labels[2], &mut salt)
            && (auth.is_empty() || aes_cm_prf(&mk.key, &mk.salt, labels[1], &mut auth));
        ok.then_some(Self { enc, auth, salt })
    }
}

/// SSRC ごとの SRTP の状態。
struct RtpStream {
    roc: u32,
    /// これまでの最大の sequence number（s_l）。
    s_l: u16,
    /// 受信側は replay、送信側は index の再使用の検出に使う。
    window: ReplayWindow,
}

/// SSRC ごとの SRTCP の状態。
struct RtcpStream {
    /// 送信側: 次に使う SRTCP index。
    next_index: u32,
    /// 受信側: replay window。
    window: ReplayWindow,
}

/// SRTP/SRTCP の暗号 context。1 方向だけに使う。
pub struct SrtpContext {
    profile: SrtpProfile,
    direction: Direction,
    rtp_keys: SessionKeys,
    rtcp_keys: SessionKeys,
    rtp_streams: HashMap<u32, RtpStream>,
    rtcp_streams: HashMap<u32, RtcpStream>,
    window_size: usize,
    max_streams: usize,
    rtp_count: u64,
    rtcp_count: u64,
}

impl core::fmt::Debug for SrtpContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SrtpContext")
            .field("profile", &self.profile)
            .field("direction", &self.direction)
            .field("rtp_streams", &self.rtp_streams.len())
            .field("rtcp_streams", &self.rtcp_streams.len())
            .finish_non_exhaustive()
    }
}

/// RTP header の長さを返す（RFC 3550 5.1、拡張 5.3.1）。
fn rtp_header_len(p: &[u8]) -> Result<usize, SrtpError> {
    if p.len() < 12 {
        return Err(SrtpError::TooShort);
    }
    if p[0] >> 6 != 2 {
        return Err(SrtpError::InvalidHeader);
    }
    let cc = (p[0] & 0x0f) as usize;
    let mut len = 12 + 4 * cc;
    if p[0] & 0x10 != 0 {
        if p.len() < len + 4 {
            return Err(SrtpError::InvalidHeader);
        }
        let words = u16::from_be_bytes([p[len + 2], p[len + 3]]) as usize;
        len += 4 + 4 * words;
    }
    if p.len() < len {
        return Err(SrtpError::InvalidHeader);
    }
    Ok(len)
}

fn rtcp_check_header(p: &[u8]) -> Result<u32, SrtpError> {
    if p.len() < 8 {
        return Err(SrtpError::TooShort);
    }
    if p[0] >> 6 != 2 {
        return Err(SrtpError::InvalidHeader);
    }
    Ok(u32::from_be_bytes([p[4], p[5], p[6], p[7]]))
}

/// RFC 3711 3.3.1 の ROC 推定。戻り値は推定した ROC（範囲外は `None`）。
fn estimate_roc(roc: u32, s_l: u16, seq: u16) -> Option<u32> {
    let (roc, s_l, seq) = (roc as i64, s_l as i64, seq as i64);
    let v = if s_l < 32768 {
        if seq - s_l > 32768 { roc - 1 } else { roc }
    } else if s_l - 32768 > seq {
        roc + 1
    } else {
        roc
    };
    u32::try_from(v).ok()
}

/// AES-CM の IV: k_s * 2^16 XOR SSRC * 2^64 XOR index * 2^16（RFC 3711 4.1.1）。
fn cm_iv(salt: &[u8], ssrc: u32, index: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[..14].copy_from_slice(&salt[..14]);
    for (d, s) in iv[4..8].iter_mut().zip(ssrc.to_be_bytes()) {
        *d ^= s;
    }
    let idx = index.to_be_bytes();
    for (d, s) in iv[8..14].iter_mut().zip(&idx[2..8]) {
        *d ^= s;
    }
    iv
}

/// GCM の IV（RFC 7714 8.1・9.1）: 00 00 || SSRC || hi(4) || lo(2) XOR salt。
fn gcm_iv(salt: &[u8], ssrc: u32, mid: [u8; 4], lo: [u8; 2]) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[2..6].copy_from_slice(&ssrc.to_be_bytes());
    iv[6..10].copy_from_slice(&mid);
    iv[10..12].copy_from_slice(&lo);
    for (d, s) in iv.iter_mut().zip(salt) {
        *d ^= s;
    }
    iv
}

fn hmac_sha1(key: &[u8], parts: &[&[u8]]) -> [u8; 20] {
    let mut m = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("HMAC は任意長の鍵を受ける");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// GCM で暗号化して tag を返す。
fn gcm_seal(key: &[u8], iv: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> Result<[u8; 16], SrtpError> {
    let nonce = Nonce::from(*iv);
    let nonce = &nonce;
    let tag = match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .map_err(|_| SrtpError::InvalidKey)?
            .encrypt_in_place_detached(nonce, aad, buf),
        32 => Aes256Gcm::new_from_slice(key)
            .map_err(|_| SrtpError::InvalidKey)?
            .encrypt_in_place_detached(nonce, aad, buf),
        _ => return Err(SrtpError::InvalidKey),
    }
    .map_err(|_| SrtpError::TooShort)?;
    Ok(tag.into())
}

/// GCM で認証してから復号する。失敗時の `buf` は呼び出し側で捨てる。
fn gcm_open(
    key: &[u8],
    iv: &[u8; 12],
    aad: &[u8],
    buf: &mut [u8],
    tag: &[u8],
) -> Result<(), SrtpError> {
    let nonce = Nonce::from(*iv);
    let nonce = &nonce;
    let tag: [u8; 16] = tag
        .try_into()
        .map_err(|_| SrtpError::AuthenticationFailed)?;
    let tag = &Tag::from(tag);
    match key.len() {
        16 => Aes128Gcm::new_from_slice(key)
            .map_err(|_| SrtpError::InvalidKey)?
            .decrypt_in_place_detached(nonce, aad, buf, tag),
        32 => Aes256Gcm::new_from_slice(key)
            .map_err(|_| SrtpError::InvalidKey)?
            .decrypt_in_place_detached(nonce, aad, buf, tag),
        _ => return Err(SrtpError::InvalidKey),
    }
    .map_err(|_| SrtpError::AuthenticationFailed)
}

impl SrtpContext {
    /// master key から context を作る（session key は kdr = 0 で一度だけ導出する）。
    pub fn new(
        profile: SrtpProfile,
        master: &MasterKey,
        direction: Direction,
    ) -> Result<Self, SrtpError> {
        if master.key.len() != profile.master_key_len()
            || master.salt.len() != profile.master_salt_len()
        {
            return Err(SrtpError::InvalidKey);
        }
        let rtp_keys = SessionKeys::derive(
            profile,
            master,
            [
                kdf::LABEL_RTP_ENCRYPTION,
                kdf::LABEL_RTP_AUTH,
                kdf::LABEL_RTP_SALT,
            ],
        )
        .ok_or(SrtpError::InvalidKey)?;
        let rtcp_keys = SessionKeys::derive(
            profile,
            master,
            [
                kdf::LABEL_RTCP_ENCRYPTION,
                kdf::LABEL_RTCP_AUTH,
                kdf::LABEL_RTCP_SALT,
            ],
        )
        .ok_or(SrtpError::InvalidKey)?;
        Ok(Self::from_session_keys(
            profile, direction, rtp_keys, rtcp_keys,
        ))
    }

    fn from_session_keys(
        profile: SrtpProfile,
        direction: Direction,
        rtp_keys: SessionKeys,
        rtcp_keys: SessionKeys,
    ) -> Self {
        Self {
            profile,
            direction,
            rtp_keys,
            rtcp_keys,
            rtp_streams: HashMap::new(),
            rtcp_streams: HashMap::new(),
            window_size: crate::DEFAULT_REPLAY_WINDOW,
            max_streams: crate::DEFAULT_MAX_STREAMS,
            rtp_count: 0,
            rtcp_count: 0,
        }
    }

    /// replay window の幅（64〜1024）を設定する。packet を処理する前に呼ぶ。
    pub fn with_replay_window(mut self, size: usize) -> Self {
        self.window_size = ReplayWindow::new(size).size();
        self
    }

    /// SSRC の上限を設定する。
    pub fn with_max_streams(mut self, n: usize) -> Self {
        self.max_streams = n.max(1);
        self
    }

    /// 保護方式。
    pub fn profile(&self) -> SrtpProfile {
        self.profile
    }

    /// 方向。
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// SSRC の現在の ROC。
    pub fn roc(&self, ssrc: u32) -> Option<u32> {
        self.rtp_streams.get(&ssrc).map(|s| s.roc)
    }

    /// 送信側の SSRC の ROC と最大 sequence number を設定する（鍵の引き継ぎ・試験用）。
    pub fn set_roc(&mut self, ssrc: u32, roc: u32, last_seq: u16) {
        let mut window = ReplayWindow::new(self.window_size);
        if self.direction == Direction::Outbound {
            window.accept(((roc as u64) << 16) | last_seq as u64);
        }
        self.rtp_streams.insert(
            ssrc,
            RtpStream {
                roc,
                s_l: last_seq,
                window,
            },
        );
    }

    fn need(&self, d: Direction) -> Result<(), SrtpError> {
        if self.direction == d {
            Ok(())
        } else {
            Err(SrtpError::WrongDirection)
        }
    }

    fn stream_room(&self, exists: bool, len: usize) -> Result<(), SrtpError> {
        if !exists && len >= self.max_streams {
            Err(SrtpError::TooManyStreams)
        } else {
            Ok(())
        }
    }

    /// 推定した ROC と 48 bit の index。
    fn rtp_index(&self, ssrc: u32, seq: u16) -> Result<(u32, u64), SrtpError> {
        let roc = match self.rtp_streams.get(&ssrc) {
            Some(s) => estimate_roc(s.roc, s.s_l, seq).ok_or(SrtpError::TooOld)?,
            None => 0,
        };
        Ok((roc, ((roc as u64) << 16) | seq as u64))
    }

    fn rtp_commit(&mut self, ssrc: u32, roc: u32, seq: u16, index: u64) {
        let size = self.window_size;
        let s = self.rtp_streams.entry(ssrc).or_insert_with(|| RtpStream {
            roc,
            s_l: seq,
            window: ReplayWindow::new(size),
        });
        let newer = s.window.top().is_none_or(|t| index > t);
        s.window.accept(index);
        if newer {
            s.roc = roc;
            s.s_l = seq;
        }
    }

    /// RTP packet を SRTP に保護する。
    ///
    /// 同じ SSRC で既に使った index（ROC と sequence number の組）は拒否する。
    pub fn protect_rtp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        self.need(Direction::Outbound)?;
        let hlen = rtp_header_len(packet)?;
        if self.rtp_count >= SRTP_PACKET_LIMIT {
            return Err(SrtpError::KeyLifetimeExceeded);
        }
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
        let exists = self.rtp_streams.contains_key(&ssrc);
        self.stream_room(exists, self.rtp_streams.len())?;
        let (roc, index) = match self.rtp_streams.get(&ssrc) {
            None => (0, seq as u64),
            Some(s) => match estimate_roc(s.roc, s.s_l, seq) {
                Some(r) => (r, ((r as u64) << 16) | seq as u64),
                // ROC が 2^32 を超える = 鍵の寿命。
                None if s.s_l >= 32768 && s.s_l - 32768 > seq => {
                    return Err(SrtpError::KeyLifetimeExceeded);
                }
                // ROC が 0 未満 = 既に使った範囲。
                None => return Err(SrtpError::IndexReuse),
            },
        };
        if let Some(s) = self.rtp_streams.get(&ssrc)
            && s.window.check(index) != ReplayCheck::Fresh
        {
            return Err(SrtpError::IndexReuse);
        }
        let tag_len = self.profile.rtp_tag_len();
        let mut out = Vec::with_capacity(packet.len() + tag_len);
        out.extend_from_slice(packet);
        let keys = &self.rtp_keys;
        if self.profile.is_aead() {
            let iv = gcm_iv(&keys.salt, ssrc, roc.to_be_bytes(), seq.to_be_bytes());
            let (hdr, body) = out.split_at_mut(hlen);
            let tag = gcm_seal(&keys.enc, &iv, hdr, body)?;
            out.extend_from_slice(&tag);
        } else {
            let iv = cm_iv(&keys.salt, ssrc, index);
            aes_cm_xor(&keys.enc, &iv, &mut out[hlen..]);
            let mac = hmac_sha1(&keys.auth, &[&out, &roc.to_be_bytes()]);
            out.extend_from_slice(&mac[..tag_len]);
        }
        self.rtp_count += 1;
        self.rtp_commit(ssrc, roc, seq, index);
        Ok(out)
    }

    /// SRTP packet を検証して RTP に戻す。認証に通らない packet の内容は返さない。
    pub fn unprotect_rtp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        self.need(Direction::Inbound)?;
        let tag_len = self.profile.rtp_tag_len();
        let hlen = rtp_header_len(packet)?;
        if packet.len() < hlen + tag_len {
            return Err(SrtpError::TooShort);
        }
        let seq = u16::from_be_bytes([packet[2], packet[3]]);
        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);
        let exists = self.rtp_streams.contains_key(&ssrc);
        self.stream_room(exists, self.rtp_streams.len())?;
        let (roc, index) = self.rtp_index(ssrc, seq)?;
        if let Some(s) = self.rtp_streams.get(&ssrc) {
            match s.window.check(index) {
                ReplayCheck::Fresh => {}
                ReplayCheck::Duplicate => return Err(SrtpError::Replayed),
                ReplayCheck::TooOld => return Err(SrtpError::TooOld),
            }
        }
        let body_end = packet.len() - tag_len;
        let keys = &self.rtp_keys;
        let mut out;
        if self.profile.is_aead() {
            let iv = gcm_iv(&keys.salt, ssrc, roc.to_be_bytes(), seq.to_be_bytes());
            out = packet[..body_end].to_vec();
            let (hdr, body) = out.split_at_mut(hlen);
            gcm_open(&keys.enc, &iv, hdr, body, &packet[body_end..])?;
        } else {
            let mac = hmac_sha1(&keys.auth, &[&packet[..body_end], &roc.to_be_bytes()]);
            if !bool::from(mac[..tag_len].ct_eq(&packet[body_end..])) {
                return Err(SrtpError::AuthenticationFailed);
            }
            out = packet[..body_end].to_vec();
            let iv = cm_iv(&keys.salt, ssrc, index);
            aes_cm_xor(&keys.enc, &iv, &mut out[hlen..]);
        }
        self.rtp_commit(ssrc, roc, seq, index);
        Ok(out)
    }

    /// RTCP（compound）packet を SRTCP に保護する。常に暗号化する（E = 1）。
    pub fn protect_rtcp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        self.need(Direction::Outbound)?;
        let ssrc = rtcp_check_header(packet)?;
        if self.rtcp_count >= SRTCP_PACKET_LIMIT {
            return Err(SrtpError::KeyLifetimeExceeded);
        }
        let exists = self.rtcp_streams.contains_key(&ssrc);
        self.stream_room(exists, self.rtcp_streams.len())?;
        let index = self.rtcp_streams.get(&ssrc).map_or(0, |s| s.next_index);
        if index as u64 >= SRTCP_PACKET_LIMIT {
            return Err(SrtpError::KeyLifetimeExceeded);
        }
        let eword = (E_FLAG | index).to_be_bytes();
        let keys = &self.rtcp_keys;
        let mut out = Vec::with_capacity(packet.len() + 4 + self.profile.rtcp_tag_len());
        out.extend_from_slice(packet);
        if self.profile.is_aead() {
            let iv = gcm_iv(
                &keys.salt,
                ssrc,
                [0, 0, (index >> 24) as u8, (index >> 16) as u8],
                [(index >> 8) as u8, index as u8],
            );
            let mut aad = [0u8; 12];
            aad[..8].copy_from_slice(&packet[..8]);
            aad[8..].copy_from_slice(&eword);
            let tag = gcm_seal(&keys.enc, &iv, &aad, &mut out[8..])?;
            out.extend_from_slice(&tag);
            out.extend_from_slice(&eword);
        } else {
            let iv = cm_iv(&keys.salt, ssrc, index as u64);
            aes_cm_xor(&keys.enc, &iv, &mut out[8..]);
            out.extend_from_slice(&eword);
            let mac = hmac_sha1(&keys.auth, &[&out]);
            out.extend_from_slice(&mac[..self.profile.rtcp_tag_len()]);
        }
        self.rtcp_count += 1;
        let size = self.window_size;
        let s = self.rtcp_streams.entry(ssrc).or_insert_with(|| RtcpStream {
            next_index: 0,
            window: ReplayWindow::new(size),
        });
        s.next_index = index + 1;
        Ok(out)
    }

    /// SRTCP packet を検証して RTCP に戻す。E = 0（認証だけ）の packet も受ける。
    pub fn unprotect_rtcp(&mut self, packet: &[u8]) -> Result<Vec<u8>, SrtpError> {
        self.need(Direction::Inbound)?;
        let ssrc = rtcp_check_header(packet)?;
        let tag_len = self.profile.rtcp_tag_len();
        if packet.len() < 8 + 4 + tag_len {
            return Err(SrtpError::TooShort);
        }
        let aead = self.profile.is_aead();
        // HMAC: header || 本体 || E+index || tag。GCM: header || 本体 || tag || E+index。
        let eword_at = if aead {
            packet.len() - 4
        } else {
            packet.len() - 4 - tag_len
        };
        let ew = u32::from_be_bytes([
            packet[eword_at],
            packet[eword_at + 1],
            packet[eword_at + 2],
            packet[eword_at + 3],
        ]);
        let encrypted = ew & E_FLAG != 0;
        let index = ew & !E_FLAG;
        let exists = self.rtcp_streams.contains_key(&ssrc);
        self.stream_room(exists, self.rtcp_streams.len())?;
        if let Some(s) = self.rtcp_streams.get(&ssrc) {
            match s.window.check(index as u64) {
                ReplayCheck::Fresh => {}
                ReplayCheck::Duplicate => return Err(SrtpError::Replayed),
                ReplayCheck::TooOld => return Err(SrtpError::TooOld),
            }
        }
        let keys = &self.rtcp_keys;
        let mut out;
        if aead {
            let iv = gcm_iv(
                &keys.salt,
                ssrc,
                [0, 0, (index >> 24) as u8, (index >> 16) as u8],
                [(index >> 8) as u8, index as u8],
            );
            let tag_at = eword_at - tag_len;
            let tag = &packet[tag_at..eword_at];
            if encrypted {
                let mut aad = [0u8; 12];
                aad[..8].copy_from_slice(&packet[..8]);
                aad[8..].copy_from_slice(&ew.to_be_bytes());
                out = packet[..tag_at].to_vec();
                gcm_open(&keys.enc, &iv, &aad, &mut out[8..], tag)?;
            } else {
                let mut aad = Vec::with_capacity(tag_at + 4);
                aad.extend_from_slice(&packet[..tag_at]);
                aad.extend_from_slice(&ew.to_be_bytes());
                gcm_open(&keys.enc, &iv, &aad, &mut [], tag)?;
                out = packet[..tag_at].to_vec();
            }
        } else {
            let tag_at = packet.len() - tag_len;
            let mac = hmac_sha1(&keys.auth, &[&packet[..tag_at]]);
            if !bool::from(mac[..tag_len].ct_eq(&packet[tag_at..])) {
                return Err(SrtpError::AuthenticationFailed);
            }
            out = packet[..eword_at].to_vec();
            if encrypted {
                let iv = cm_iv(&keys.salt, ssrc, index as u64);
                aes_cm_xor(&keys.enc, &iv, &mut out[8..]);
            }
        }
        let size = self.window_size;
        self.rtcp_streams
            .entry(ssrc)
            .or_insert_with(|| RtcpStream {
                next_index: 0,
                window: ReplayWindow::new(size),
            })
            .window
            .accept(index as u64);
        Ok(out)
    }
}

/// DTLS-SRTP の keying material（RFC 5764 4.2）から送信用と受信用の context を作る。
///
/// keying material の並びは client_write_key | server_write_key | client_write_salt | server_write_salt。
/// `is_client` は DTLS の役割（client なら client_write_* で送る）。戻り値は `(送信用, 受信用)`。
pub fn dtls_srtp_contexts(
    profile: SrtpProfile,
    keying_material: &[u8],
    is_client: bool,
) -> Result<(SrtpContext, SrtpContext), SrtpError> {
    let k = profile.master_key_len();
    let s = profile.master_salt_len();
    if keying_material.len() != 2 * (k + s) {
        return Err(SrtpError::InvalidKey);
    }
    let ck = &keying_material[..k];
    let sk = &keying_material[k..2 * k];
    let cs = &keying_material[2 * k..2 * k + s];
    let ss = &keying_material[2 * k + s..];
    let client = MasterKey::new(profile, ck, cs).ok_or(SrtpError::InvalidKey)?;
    let server = MasterKey::new(profile, sk, ss).ok_or(SrtpError::InvalidKey)?;
    let (tx, rx) = if is_client {
        (client, server)
    } else {
        (server, client)
    };
    Ok((
        SrtpContext::new(profile, &tx, Direction::Outbound)?,
        SrtpContext::new(profile, &rx, Direction::Inbound)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 7714 の試験では session key を直接与える。
    fn vector_ctx(profile: SrtpProfile, dir: Direction, key: &[u8]) -> SrtpContext {
        let salt = hex("517569642070726f2071756f");
        let mk = || SessionKeys {
            enc: key.to_vec(),
            auth: Vec::new(),
            salt: salt.clone(),
        };
        SrtpContext::from_session_keys(profile, dir, mk(), mk())
    }

    const RTP_PLAIN: &str =
        "8040f17b 8041f8d3 5501a0b2 47616c6c 69612065 7374206f 6d6e6973 20646976
        69736120 696e2070 61727465 73207472 6573";

    fn key128() -> Vec<u8> {
        (0u8..16).collect()
    }
    fn key256() -> Vec<u8> {
        (0u8..32).collect()
    }

    /// RFC 7714 16.1.1・16.1.2。
    #[test]
    fn rfc7714_rtp_aes128_gcm() {
        let expect = hex(
            "8040f17b 8041f8d3 5501a0b2 f24de3a3 fb34de6c acba861c 9d7e4bca be633bd5
             0d294e6f 42a5f47a 51c7d19b 36de3adf 8833899d 7f27beb1 6a9152cf 765ee439 0cce",
        );
        let mut tx = vector_ctx(SrtpProfile::AeadAes128Gcm, Direction::Outbound, &key128());
        assert_eq!(tx.protect_rtp(&hex(RTP_PLAIN)).unwrap(), expect);
        let mut rx = vector_ctx(SrtpProfile::AeadAes128Gcm, Direction::Inbound, &key128());
        assert_eq!(rx.unprotect_rtp(&expect).unwrap(), hex(RTP_PLAIN));
    }

    /// RFC 7714 16.2.1・16.2.2。
    #[test]
    fn rfc7714_rtp_aes256_gcm() {
        let expect = hex(
            "8040f17b 8041f8d3 5501a0b2 32b1de78 a822fe12 ef9f78fa 332e33aa b1801238
             9a58e2f3 b50b2a02 76ffae0f 1ba63799 b87b7aa3 db36dfff d6b0f9bb 7878d7a7 6c13",
        );
        let mut tx = vector_ctx(SrtpProfile::AeadAes256Gcm, Direction::Outbound, &key256());
        assert_eq!(tx.protect_rtp(&hex(RTP_PLAIN)).unwrap(), expect);
        let mut rx = vector_ctx(SrtpProfile::AeadAes256Gcm, Direction::Inbound, &key256());
        assert_eq!(rx.unprotect_rtp(&expect).unwrap(), hex(RTP_PLAIN));
    }

    const RTCP_PLAIN: &str =
        "81c8000d 4d617273 4e545031 4e545032 52545020 0000042a 0000e930 4c756e61
        deadbeef deadbeef deadbeef deadbeef deadbeef";

    /// RFC 7714 17.1（暗号化と tag）。
    #[test]
    fn rfc7714_rtcp_aes128_gcm() {
        let expect = hex(
            "81c8000d 4d617273 63e94885 dcdab67c a727d766 2f6b7e99 7ff5c0f7 6c06f32d
             c676a5f1 730d6fda 4ce09b46 86303ded 0bb9275b c84aa458 96cf4d2f c5abf872
             45d9eade 800005d4",
        );
        let mut tx = vector_ctx(SrtpProfile::AeadAes128Gcm, Direction::Outbound, &key128());
        tx.rtcp_streams.insert(
            0x4d617273,
            RtcpStream {
                next_index: 0x5d4,
                window: ReplayWindow::new(64),
            },
        );
        assert_eq!(tx.protect_rtcp(&hex(RTCP_PLAIN)).unwrap(), expect);
        let mut rx = vector_ctx(SrtpProfile::AeadAes128Gcm, Direction::Inbound, &key128());
        assert_eq!(rx.unprotect_rtcp(&expect).unwrap(), hex(RTCP_PLAIN));
        assert_eq!(rx.unprotect_rtcp(&expect), Err(SrtpError::Replayed));
    }

    /// RFC 7714 17.2（AES-256 の検証と復号）。
    #[test]
    fn rfc7714_rtcp_aes256_gcm_decrypt() {
        let pkt = hex(
            "81c8000d 4d617273 d50ae4d1 f5ce5d30 4ba297e4 7d470c28 2c3ece5d bffe0a50
             a2eaa5c1 110555be 8415f658 c61de047 6f1b6fad 1d1eb30c 4446839f 57ff6f6c
             b26ac3be 800005d4",
        );
        let mut rx = vector_ctx(SrtpProfile::AeadAes256Gcm, Direction::Inbound, &key256());
        assert_eq!(rx.unprotect_rtcp(&pkt).unwrap(), hex(RTCP_PLAIN));
    }

    /// RFC 7714 17.3・17.4（E = 0、tag だけ）。
    #[test]
    fn rfc7714_rtcp_tag_only() {
        let p128 = hex(&format!(
            "{RTCP_PLAIN} 841dd968 3dd78ec9 2ae58790 125f62b3 000005d4"
        ));
        let mut rx = vector_ctx(SrtpProfile::AeadAes128Gcm, Direction::Inbound, &key128());
        assert_eq!(rx.unprotect_rtcp(&p128).unwrap(), hex(RTCP_PLAIN));
        let p256 = hex(&format!(
            "{RTCP_PLAIN} 91db4afb feee5a97 8fab4393 ed2615fe 000005d4"
        ));
        let mut rx = vector_ctx(SrtpProfile::AeadAes256Gcm, Direction::Inbound, &key256());
        assert_eq!(rx.unprotect_rtcp(&p256).unwrap(), hex(RTCP_PLAIN));
        let mut bad = p256.clone();
        bad[20] ^= 1;
        let mut rx = vector_ctx(SrtpProfile::AeadAes256Gcm, Direction::Inbound, &key256());
        assert_eq!(
            rx.unprotect_rtcp(&bad),
            Err(SrtpError::AuthenticationFailed)
        );
    }

    #[test]
    fn roc_estimation() {
        assert_eq!(estimate_roc(0, 65535, 0), Some(1));
        assert_eq!(estimate_roc(1, 0, 65535), Some(0));
        assert_eq!(estimate_roc(0, 0, 65535), None);
        assert_eq!(estimate_roc(5, 40000, 100), Some(6));
        assert_eq!(estimate_roc(5, 40000, 20000), Some(5));
        assert_eq!(estimate_roc(5, 100, 30000), Some(5));
        assert_eq!(estimate_roc(5, 100, 50000), Some(4));
        assert_eq!(estimate_roc(u32::MAX, 65000, 10), None);
    }
}
