//! DTLS 1.2 の endpoint（handshake の状態機械・flight の再送・record の送受信）。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use bytes::Bytes;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::cert::{certificate_public_key, fingerprint_matches, random_p256_secret};
use crate::cookie::hello_verify_datagram;
use crate::handshake::{self as hs, Feed, Fragment, Message, Reassembler};
use crate::prf::prf;
use crate::record::{
    self, CT_ALERT, CT_APPLICATION_DATA, CT_CHANGE_CIPHER_SPEC, CT_HANDSHAKE, CipherState, DTLS10,
    DTLS12, MAX_PLAINTEXT, MAX_SEQ, RECORD_HEADER_LEN, ReplayWindow,
};
use crate::{
    AlertDescription, CipherSuite, DtlsConfig, DtlsError, NamedGroup, Role, SrtpProtectionProfile,
};

const ALERT_WARNING: u8 = 1;
const ALERT_FATAL: u8 = 2;
/// HelloVerifyRequest を受ける回数の上限。
const MAX_HELLO_VERIFY: u32 = 4;
/// 鍵がまだ無い epoch の record を取っておく数と量。
const MAX_DEFERRED: usize = 16;
const MAX_DEFERRED_BYTES: usize = 64 * 1024;
/// 分割された最初の ClientHello を組み立てる上限（byte）。Chrome は DTLS 1.3 の鍵共有（ML-KEM など）を載せた
/// 1.4 KB ほどの ClientHello を 2 つの record に分けて送る。
const MAX_SPLIT_CLIENT_HELLO: usize = 8 * 1024;
/// handshake 完了前に届いた application data を取っておく数。
const MAX_EARLY_APP: usize = 16;
/// exporter で使えない label（RFC 5705 4）。
const RESERVED_LABELS: [&[u8]; 5] = [
    b"client finished",
    b"server finished",
    b"master secret",
    b"extended master secret",
    b"key expansion",
];

/// endpoint から取り出す出来事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DtlsEvent {
    /// handshake が終わった。fingerprint を照合し、exporter で SRTP の鍵を取り出せる。
    Connected,
    /// 受け取った application data（SCTP など）。
    ApplicationData(Bytes),
    /// 相手が close_notify を送った。
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Handshaking,
    Connected,
    Closed,
    Failed,
}

/// handshake で次に待つもの。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hs {
    ClientWaitHello,
    ClientWaitCertificate,
    ClientWaitKeyExchange,
    ClientWaitCertRequestOrDone,
    ClientWaitDone,
    ClientWaitFinished,
    ServerWaitClientHello,
    ServerWaitCertificate,
    ServerWaitKeyExchange,
    ServerWaitCertificateVerify,
    ServerWaitFinished,
    Done,
}

/// flight の要素。再送のたびに新しい record sequence number で組み直す。
enum FlightItem {
    Handshake {
        epoch: u16,
        msg_type: u8,
        seq: u16,
        body: Vec<u8>,
    },
    ChangeCipherSpec,
}

/// ECDHE の一時鍵。
enum Ephemeral {
    X25519(Zeroizing<[u8; 32]>),
    P256(p256::SecretKey),
}

impl Ephemeral {
    fn generate(group: NamedGroup) -> Result<(Self, Vec<u8>), DtlsError> {
        match group {
            NamedGroup::X25519 => {
                let mut s = Zeroizing::new([0u8; 32]);
                getrandom::fill(&mut s[..]).map_err(|_| DtlsError::RandomUnavailable)?;
                let public = x25519_dalek::x25519(*s, x25519_dalek::X25519_BASEPOINT_BYTES);
                Ok((Self::X25519(s), public.to_vec()))
            }
            NamedGroup::Secp256r1 => {
                use p256::elliptic_curve::sec1::ToEncodedPoint;
                let sk = random_p256_secret()?;
                let public = sk.public_key().to_encoded_point(false).as_bytes().to_vec();
                Ok((Self::P256(sk), public))
            }
        }
    }

    /// 共有秘密（pre-master secret）。相手の公開値が不正なら `None`。
    fn agree(&self, peer: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        match self {
            Self::X25519(s) => {
                let u: [u8; 32] = peer.try_into().ok()?;
                let out = Zeroizing::new(x25519_dalek::x25519(**s, u));
                // 位数の小さい点では共有秘密が 0 になる（RFC 7748 6.1）。
                if bool::from(out.ct_eq(&[0u8; 32])) {
                    return None;
                }
                Some(Zeroizing::new(out.to_vec()))
            }
            Self::P256(sk) => {
                // 非圧縮形式だけ（ec_point_formats は uncompressed だけを示している）。
                if peer.len() != 65 || peer[0] != 0x04 {
                    return None;
                }
                let pk = p256::PublicKey::from_sec1_bytes(peer).ok()?;
                let shared = p256::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
                Some(Zeroizing::new(shared.raw_secret_bytes().to_vec()))
            }
        }
    }
}

fn fatal(alert: AlertDescription, reason: &'static str) -> DtlsError {
    DtlsError::Fatal { alert, reason }
}

fn decode_err(_: crate::codec::DecodeError) -> DtlsError {
    fatal(AlertDescription::DecodeError, "message を解析できない")
}

/// DTLS 1.2 の endpoint。1 つの相手（5-tuple）に 1 つ作る。
pub struct DtlsEndpoint {
    cfg: DtlsConfig,
    state: State,
    hs: Hs,
    client_random: [u8; 32],
    server_random: [u8; 32],
    suite: Option<CipherSuite>,
    group: Option<NamedGroup>,
    srtp: Option<SrtpProtectionProfile>,
    ephemeral: Option<Ephemeral>,
    peer_ecdhe_public: Vec<u8>,
    peer_cert: Option<Vec<u8>>,
    peer_key: Option<VerifyingKey>,
    peer_fp: Option<[u8; 32]>,
    cert_requested: bool,
    transcript: Vec<u8>,
    master: Option<Zeroizing<[u8; 48]>>,
    read_cipher: Option<CipherState>,
    write_cipher: Option<CipherState>,
    write_epoch: u16,
    send_seq: [u64; 2],
    replay: ReplayWindow,
    send_msg_seq: u16,
    reasm: Reassembler,
    peer_flight_start: u16,
    flight: Vec<FlightItem>,
    retransmit_at: Option<Instant>,
    rto: Duration,
    retransmits: u32,
    last_reactive: Option<Instant>,
    deadline: Instant,
    deferred: Vec<Vec<u8>>,
    deferred_bytes: usize,
    /// 分割された最初の ClientHello（message seq, 全体の長さ, 中身, 届いた範囲, 最初の record の番号）。
    split_hello: Option<SplitHello>,
    early_app: Vec<Bytes>,
    out: VecDeque<Vec<u8>>,
    events: VecDeque<DtlsEvent>,
    cookie: Vec<u8>,
    hello_verify_count: u32,
}

impl core::fmt::Debug for DtlsEndpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DtlsEndpoint")
            .field("role", &self.cfg.role)
            .field("state", &self.state)
            .field("handshake", &self.hs)
            .field("suite", &self.suite)
            .finish_non_exhaustive()
    }
}

impl DtlsEndpoint {
    /// endpoint を作る。client は ClientHello を送る用意をする（[`Self::poll_transmit`] で取り出す）。
    pub fn new(cfg: DtlsConfig, now: Instant) -> Result<Self, DtlsError> {
        cfg.validate()?;
        let role = cfg.role;
        let deadline = now + cfg.handshake_timeout;
        let rto = cfg.initial_retransmit_timeout;
        let mut ep = Self {
            cfg,
            state: State::Handshaking,
            hs: if role == Role::Client {
                Hs::ClientWaitHello
            } else {
                Hs::ServerWaitClientHello
            },
            client_random: [0; 32],
            server_random: [0; 32],
            suite: None,
            group: None,
            srtp: None,
            ephemeral: None,
            peer_ecdhe_public: Vec::new(),
            peer_cert: None,
            peer_key: None,
            peer_fp: None,
            cert_requested: false,
            transcript: Vec::new(),
            master: None,
            read_cipher: None,
            write_cipher: None,
            write_epoch: 0,
            send_seq: [0; 2],
            replay: ReplayWindow::default(),
            send_msg_seq: 0,
            reasm: Reassembler::new(),
            peer_flight_start: 0,
            flight: Vec::new(),
            retransmit_at: None,
            rto,
            retransmits: 0,
            last_reactive: None,
            deadline,
            deferred: Vec::new(),
            deferred_bytes: 0,
            split_hello: None,
            early_app: Vec::new(),
            out: VecDeque::new(),
            events: VecDeque::new(),
            cookie: Vec::new(),
            hello_verify_count: 0,
        };
        if role == Role::Client {
            getrandom::fill(&mut ep.client_random).map_err(|_| DtlsError::RandomUnavailable)?;
            ep.send_client_hello(now)?;
        }
        Ok(ep)
    }

    // ---- 公開の問い合わせ ----

    /// 役割。
    pub fn role(&self) -> Role {
        self.cfg.role
    }

    /// handshake が終わり、application data を送受信できる。
    pub fn is_connected(&self) -> bool {
        self.state == State::Connected
    }

    /// 閉じた（close_notify・失敗）。
    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed | State::Failed)
    }

    /// 決まった cipher suite。
    pub fn cipher_suite(&self) -> Option<CipherSuite> {
        self.suite
    }

    /// 決まった ECDHE の曲線。
    pub fn named_group(&self) -> Option<NamedGroup> {
        self.group
    }

    /// use_srtp で決まった保護方式。相手が use_srtp を使わなければ `None`。
    pub fn srtp_profile(&self) -> Option<SrtpProtectionProfile> {
        self.srtp
    }

    /// 相手の証明書（DER）。
    pub fn peer_certificate(&self) -> Option<&[u8]> {
        self.peer_cert.as_deref()
    }

    /// 相手の証明書の SHA-256 fingerprint。
    pub fn peer_fingerprint_sha256(&self) -> Option<[u8; 32]> {
        self.peer_fp
    }

    /// 相手の証明書の fingerprint が `expected`（SDP の a=fingerprint）と一致するか（定数時間）。
    pub fn verify_peer_fingerprint(&self, expected: &[u8]) -> bool {
        self.peer_fp
            .is_some_and(|fp| fingerprint_matches(&fp, expected))
    }

    /// 自分の証明書の SHA-256 fingerprint。
    pub fn local_fingerprint_sha256(&self) -> [u8; 32] {
        self.cfg.certificate.fingerprint_sha256()
    }

    /// RFC 5705 の exporter。接続後だけ使える。
    pub fn export_keying_material(
        &self,
        label: &[u8],
        context: Option<&[u8]>,
        len: usize,
    ) -> Result<Zeroizing<Vec<u8>>, DtlsError> {
        if self.state != State::Connected {
            return Err(DtlsError::NotConnected);
        }
        if len == 0
            || len > 4096
            || RESERVED_LABELS.contains(&label)
            || context.is_some_and(|c| c.len() > 0xffff)
        {
            return Err(DtlsError::InvalidExporter);
        }
        let (Some(master), Some(suite)) = (&self.master, self.suite) else {
            return Err(DtlsError::NotConnected);
        };
        let mut seed = Vec::with_capacity(64 + context.map_or(0, |c| c.len() + 2));
        seed.extend_from_slice(&self.client_random);
        seed.extend_from_slice(&self.server_random);
        if let Some(c) = context {
            seed.extend_from_slice(&(c.len() as u16).to_be_bytes());
            seed.extend_from_slice(c);
        }
        let mut out = Zeroizing::new(vec![0u8; len]);
        prf(suite.prf_hash(), &master[..], label, &seed, &mut out);
        Ok(out)
    }

    /// DTLS-SRTP の keying material（RFC 5764 4.2、label "EXTRACTOR-dtls_srtp"、context なし）。
    ///
    /// 並びは client_write_key | server_write_key | client_write_salt | server_write_salt。
    pub fn export_srtp_keying_material(
        &self,
    ) -> Result<(SrtpProtectionProfile, Zeroizing<Vec<u8>>), DtlsError> {
        let profile = self.srtp.ok_or(DtlsError::InvalidExporter)?;
        let km = self.export_keying_material(
            crate::SRTP_EXPORTER_LABEL,
            None,
            profile.keying_material_len(),
        )?;
        Ok((profile, km))
    }

    // ---- sans-io の入出力 ----

    /// 送る datagram を 1 つ取り出す。
    pub fn poll_transmit(&mut self) -> Option<Vec<u8>> {
        self.out.pop_front()
    }

    /// 出来事を 1 つ取り出す。
    pub fn poll_event(&mut self) -> Option<DtlsEvent> {
        self.events.pop_front()
    }

    /// 次に [`Self::handle_timeout`] を呼ぶ時刻。
    pub fn poll_timeout(&self) -> Option<Instant> {
        match self.state {
            State::Handshaking => Some(
                self.retransmit_at
                    .map_or(self.deadline, |t| t.min(self.deadline)),
            ),
            State::Connected => self.retransmit_at,
            _ => None,
        }
    }

    /// 時刻を進める（再送・handshake の時間切れ）。
    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), DtlsError> {
        if self.is_closed() {
            return Ok(());
        }
        let r = self.on_timeout(now);
        self.finish(r)
    }

    /// 受け取った datagram を処理する。認証に通らない record・壊れた record は黙って捨てる
    /// （RFC 6347 4.1.2.7）。handshake を続けられない場合は fatal alert を送って `Err` を返す。
    pub fn handle_datagram(&mut self, datagram: &[u8], now: Instant) -> Result<(), DtlsError> {
        match self.state {
            State::Failed => return Err(DtlsError::Closed),
            State::Closed => return Ok(()),
            _ => {}
        }
        let r = self.process_datagram(datagram, now);
        self.finish(r)
    }

    /// application data を送る。
    pub fn send(&mut self, data: &[u8]) -> Result<(), DtlsError> {
        match self.state {
            State::Connected => {}
            State::Handshaking => return Err(DtlsError::NotConnected),
            _ => return Err(DtlsError::Closed),
        }
        if data.len() > MAX_PLAINTEXT {
            return Err(DtlsError::TooLarge);
        }
        let rec = self.seal_record(1, CT_APPLICATION_DATA, data)?;
        self.out.push_back(rec);
        Ok(())
    }

    /// close_notify を送って閉じる。
    pub fn close(&mut self) {
        if matches!(self.state, State::Connected | State::Handshaking) {
            let _ = self.send_alert(ALERT_WARNING, AlertDescription::CloseNotify);
        }
        self.state = State::Closed;
        self.retransmit_at = None;
    }

    // ---- 内部 ----

    fn finish(&mut self, r: Result<(), DtlsError>) -> Result<(), DtlsError> {
        if let Err(e) = &r {
            if let DtlsError::Fatal { alert, .. } = e {
                let _ = self.send_alert(ALERT_FATAL, *alert);
            }
            self.state = State::Failed;
            self.retransmit_at = None;
            self.flight.clear();
            self.ephemeral = None;
        }
        r
    }

    fn on_timeout(&mut self, now: Instant) -> Result<(), DtlsError> {
        if self.state == State::Handshaking && now >= self.deadline {
            return Err(DtlsError::Timeout);
        }
        if let Some(t) = self.retransmit_at
            && now >= t
        {
            if self.retransmits >= self.cfg.max_retransmissions {
                return Err(DtlsError::Timeout);
            }
            self.retransmits += 1;
            self.rto = (self.rto * 2).min(self.cfg.max_retransmit_timeout);
            self.transmit_flight()?;
            self.retransmit_at = Some(now + self.rto);
        }
        Ok(())
    }

    fn process_datagram(&mut self, datagram: &[u8], now: Instant) -> Result<(), DtlsError> {
        for rec in record::split_records(datagram) {
            self.process_record(&rec, now)?;
            if self.is_closed() {
                break;
            }
        }
        self.process_deferred(now)
    }

    fn process_deferred(&mut self, now: Instant) -> Result<(), DtlsError> {
        if self.read_cipher.is_none() || self.deferred.is_empty() {
            return Ok(());
        }
        let deferred = std::mem::take(&mut self.deferred);
        self.deferred_bytes = 0;
        for raw in deferred {
            for rec in record::split_records(&raw) {
                self.process_record(&rec, now)?;
                if self.is_closed() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn process_record(&mut self, rec: &record::Record<'_>, now: Instant) -> Result<(), DtlsError> {
        if rec.version != DTLS12 && !(rec.epoch == 0 && rec.version == DTLS10) {
            return Ok(());
        }
        let plaintext: Vec<u8>;
        let body: &[u8] = match rec.epoch {
            0 => {
                // epoch 0 は平文。application data は受けない。
                if rec.content_type == CT_APPLICATION_DATA || rec.body.len() > MAX_PLAINTEXT {
                    return Ok(());
                }
                rec.body
            }
            1 => {
                let Some(c) = &self.read_cipher else {
                    // 鍵がまだ無い（並べ替えで Finished が先に届いた）。後で処理する。
                    if self.deferred.len() < MAX_DEFERRED
                        && self.deferred_bytes + rec.raw.len() <= MAX_DEFERRED_BYTES
                    {
                        self.deferred_bytes += rec.raw.len();
                        self.deferred.push(rec.raw.to_vec());
                    }
                    return Ok(());
                };
                if !self.replay.is_fresh(rec.seq) {
                    return Ok(());
                }
                let Some(p) = c.open(1, rec.seq, rec.content_type, rec.version, rec.body) else {
                    return Ok(());
                };
                self.replay.accept(rec.seq);
                plaintext = p;
                &plaintext
            }
            _ => return Ok(()),
        };
        match rec.content_type {
            CT_HANDSHAKE => self.on_handshake_record(rec.epoch, rec.seq, body, now),
            // ChangeCipherSpec は状態を変えない。epoch 1 の鍵は handshake の進行で用意し、
            // Finished の検証が transcript 全体を守る（早すぎる CCS による攻撃を受けない）。
            CT_CHANGE_CIPHER_SPEC => Ok(()),
            CT_ALERT => self.on_alert(rec.epoch, body),
            CT_APPLICATION_DATA => {
                // ここに来るのは epoch 1（認証済み）だけ。
                let data = Bytes::copy_from_slice(body);
                if self.state == State::Connected {
                    self.events.push_back(DtlsEvent::ApplicationData(data));
                } else if self.early_app.len() < MAX_EARLY_APP {
                    self.early_app.push(data);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn on_alert(&mut self, epoch: u16, body: &[u8]) -> Result<(), DtlsError> {
        if body.len() != 2 {
            return if epoch == 0 {
                Ok(())
            } else {
                Err(fatal(AlertDescription::DecodeError, "alert の長さ"))
            };
        }
        // 接続後の平文 alert は認証されていないので無視する（偽の切断を受けない）。
        if epoch == 0 && self.state == State::Connected {
            return Ok(());
        }
        let desc = AlertDescription::from_code(body[1]);
        if desc == AlertDescription::CloseNotify {
            if self.state == State::Connected {
                let _ = self.send_alert(ALERT_WARNING, AlertDescription::CloseNotify);
            }
            self.state = State::Closed;
            self.retransmit_at = None;
            self.events.push_back(DtlsEvent::Closed);
            return Ok(());
        }
        if body[0] == ALERT_FATAL {
            return Err(DtlsError::PeerAlert(desc));
        }
        Ok(())
    }

    fn on_handshake_record(
        &mut self,
        epoch: u16,
        rseq: u64,
        body: &[u8],
        now: Instant,
    ) -> Result<(), DtlsError> {
        let frags = match hs::parse_fragments(body) {
            Ok(f) => f,
            // 平文の壊れた record は偽造かもしれないので捨てる。認証済みなら相手の誤り。
            Err(_) if epoch == 0 => return Ok(()),
            Err(e) => return Err(decode_err(e)),
        };
        for f in &frags {
            if self.hs == Hs::ServerWaitClientHello {
                if epoch == 0 && f.msg_type == hs::HT_CLIENT_HELLO {
                    if f.offset == 0 && f.data.len() == f.length as usize {
                        self.on_initial_client_hello(rseq, f, now)?;
                    } else if let Some((seq, rs, data)) = self.feed_split_hello(rseq, f) {
                        let whole = Fragment {
                            msg_type: hs::HT_CLIENT_HELLO,
                            length: data.len() as u32,
                            seq,
                            offset: 0,
                            data: &data,
                        };
                        self.on_initial_client_hello(rs, &whole, now)?;
                    }
                }
                continue;
            }
            if let Feed::Old(seq) = self.reasm.feed(f, epoch) {
                self.on_old_message(seq, now)?;
            }
        }
        while let Some(m) = self.reasm.pop() {
            if self.state != State::Handshaking {
                self.on_post_handshake_message(&m)?;
                continue;
            }
            // 相手が次の flight を送ってきた = こちらの flight は届いた。
            self.retransmit_at = None;
            self.on_message(m, now)?;
        }
        Ok(())
    }

    /// 分割された最初の ClientHello の断片を入れる。揃ったら (message seq, 最大の record 番号, 中身) を返す。
    ///
    /// 状態は 1 つの ClientHello 分（上限 [`MAX_SPLIT_CLIENT_HELLO`]）だけ持つ。別の長さ・番号の断片が来たら
    /// 作り直す（偽の断片で埋められても、正しい ClientHello の再送で回復する）。
    fn feed_split_hello(&mut self, rseq: u64, f: &Fragment<'_>) -> Option<(u16, u64, Vec<u8>)> {
        let len = f.length as usize;
        let off = f.offset as usize;
        if len == 0 || len > MAX_SPLIT_CLIENT_HELLO || off.checked_add(f.data.len())? > len {
            return None;
        }
        let fresh = self
            .split_hello
            .as_ref()
            .is_none_or(|h| h.seq != f.seq || h.data.len() != len);
        if fresh {
            self.split_hello = Some(SplitHello {
                seq: f.seq,
                data: vec![0; len],
                have: vec![false; len],
                missing: len,
                rseq,
            });
        }
        let h = self.split_hello.as_mut()?;
        h.data[off..off + f.data.len()].copy_from_slice(f.data);
        for b in &mut h.have[off..off + f.data.len()] {
            if !*b {
                *b = true;
                h.missing -= 1;
            }
        }
        h.rseq = h.rseq.max(rseq);
        if h.missing > 0 {
            return None;
        }
        let h = self.split_hello.take()?;
        Some((h.seq, h.rseq, h.data))
    }

    /// 相手が前の flight を再送してきた = こちらの最後の flight が失われた。
    fn on_old_message(&mut self, seq: u16, now: Instant) -> Result<(), DtlsError> {
        if seq >= self.peer_flight_start || self.flight.is_empty() {
            return Ok(());
        }
        let gap = self.cfg.initial_retransmit_timeout / 2;
        if self.last_reactive.is_some_and(|t| now < t + gap) {
            return Ok(());
        }
        self.last_reactive = Some(now);
        self.transmit_flight()
    }

    fn on_post_handshake_message(&mut self, m: &Message) -> Result<(), DtlsError> {
        if m.epoch == 0 {
            // 認証されていない新しい handshake は無視する。
            return Ok(());
        }
        match (self.cfg.role, m.msg_type) {
            (Role::Client, hs::HT_HELLO_REQUEST) | (Role::Server, hs::HT_CLIENT_HELLO) => {
                // 再ネゴシエーションは受けない。
                self.send_alert(ALERT_WARNING, AlertDescription::NoRenegotiation)
            }
            _ => Err(fatal(
                AlertDescription::UnexpectedMessage,
                "handshake 後の message",
            )),
        }
    }

    fn add_transcript(&mut self, m: &Message) {
        self.transcript
            .extend_from_slice(&hs::message_bytes(m.msg_type, m.seq, &m.body));
    }

    fn on_message(&mut self, m: Message, now: Instant) -> Result<(), DtlsError> {
        // Finished だけが epoch 1、他は epoch 0。
        let want_epoch = if m.msg_type == hs::HT_FINISHED { 1 } else { 0 };
        if m.epoch != want_epoch {
            return Err(fatal(
                AlertDescription::UnexpectedMessage,
                "epoch が合わない",
            ));
        }
        match (self.hs, m.msg_type) {
            (Hs::ClientWaitHello, hs::HT_HELLO_VERIFY_REQUEST) => {
                self.on_hello_verify_request(&m, now)
            }
            (Hs::ClientWaitHello, hs::HT_SERVER_HELLO) => self.on_server_hello(&m),
            (Hs::ClientWaitCertificate, hs::HT_CERTIFICATE) => {
                self.on_certificate(&m)?;
                self.hs = Hs::ClientWaitKeyExchange;
                Ok(())
            }
            (Hs::ClientWaitKeyExchange, hs::HT_SERVER_KEY_EXCHANGE) => {
                self.on_server_key_exchange(&m)
            }
            (Hs::ClientWaitCertRequestOrDone, hs::HT_CERTIFICATE_REQUEST) => {
                self.on_certificate_request(&m)
            }
            (Hs::ClientWaitCertRequestOrDone | Hs::ClientWaitDone, hs::HT_SERVER_HELLO_DONE) => {
                self.on_server_hello_done(&m, now)
            }
            (Hs::ClientWaitFinished, hs::HT_FINISHED) => self.on_server_finished(&m),
            (Hs::ServerWaitCertificate, hs::HT_CERTIFICATE) => {
                self.on_certificate(&m)?;
                self.hs = Hs::ServerWaitKeyExchange;
                Ok(())
            }
            (Hs::ServerWaitKeyExchange, hs::HT_CLIENT_KEY_EXCHANGE) => {
                self.on_client_key_exchange(&m)
            }
            (Hs::ServerWaitCertificateVerify, hs::HT_CERTIFICATE_VERIFY) => {
                self.on_certificate_verify(&m)
            }
            (Hs::ServerWaitFinished, hs::HT_FINISHED) => self.on_client_finished(&m, now),
            _ => Err(fatal(
                AlertDescription::UnexpectedMessage,
                "順序に合わない handshake message",
            )),
        }
    }

    // ---- client ----

    fn send_client_hello(&mut self, now: Instant) -> Result<(), DtlsError> {
        let suites: Vec<u16> = self.cfg.cipher_suites.iter().map(|s| s.id()).collect();
        let groups: Vec<u16> = self.cfg.groups.iter().map(|g| g.id()).collect();
        let srtp: Vec<u16> = self.cfg.srtp_profiles.iter().map(|p| p.id()).collect();
        let body = hs::encode_client_hello(&hs::ClientHelloParams {
            random: &self.client_random,
            cookie: &self.cookie,
            suites: &suites,
            groups: &groups,
            srtp: &srtp,
        });
        // cookie 交換の前の ClientHello と HelloVerifyRequest は transcript に入れない（RFC 6347 4.2.1）。
        self.transcript.clear();
        let mut flight = Vec::new();
        self.push_message(&mut flight, 0, hs::HT_CLIENT_HELLO, body);
        self.send_new_flight(flight, false, now)
    }

    fn on_hello_verify_request(&mut self, m: &Message, now: Instant) -> Result<(), DtlsError> {
        let (version, cookie) = hs::parse_hello_verify_request(&m.body).map_err(decode_err)?;
        if version != DTLS12 && version != DTLS10 {
            return Err(fatal(
                AlertDescription::ProtocolVersion,
                "HelloVerifyRequest の版",
            ));
        }
        if cookie.is_empty() {
            return Err(fatal(AlertDescription::IllegalParameter, "空の cookie"));
        }
        self.hello_verify_count += 1;
        if self.hello_verify_count > MAX_HELLO_VERIFY {
            return Err(fatal(
                AlertDescription::HandshakeFailure,
                "HelloVerifyRequest が多すぎる",
            ));
        }
        self.cookie = cookie;
        self.send_client_hello(now)
    }

    fn on_server_hello(&mut self, m: &Message) -> Result<(), DtlsError> {
        let sh = hs::ServerHello::parse(&m.body).map_err(decode_err)?;
        if sh.version != DTLS12 {
            return Err(fatal(
                AlertDescription::ProtocolVersion,
                "DTLS 1.2 ではない",
            ));
        }
        let suite = CipherSuite::from_id(sh.cipher_suite)
            .filter(|s| self.cfg.cipher_suites.contains(s))
            .ok_or(fatal(
                AlertDescription::IllegalParameter,
                "申し出ていない cipher suite",
            ))?;
        if sh.compression != 0 {
            return Err(fatal(AlertDescription::IllegalParameter, "圧縮"));
        }
        if !sh.ext.ems {
            return Err(fatal(
                AlertDescription::HandshakeFailure,
                "extended master secret が無い",
            ));
        }
        if sh.ext.reneg.as_ref().is_some_and(|r| !r.is_empty()) {
            return Err(fatal(
                AlertDescription::HandshakeFailure,
                "renegotiation_info が空でない",
            ));
        }
        if sh
            .ext
            .point_formats
            .as_ref()
            .is_some_and(|p| !p.contains(&0))
        {
            return Err(fatal(
                AlertDescription::IllegalParameter,
                "ec_point_formats",
            ));
        }
        if sh.ext.groups.is_some() || sh.ext.sig_algs.is_some() || !sh.ext.others.is_empty() {
            return Err(fatal(
                AlertDescription::UnsupportedExtension,
                "申し出ていない拡張",
            ));
        }
        if let Some((profiles, mki)) = &sh.ext.use_srtp {
            if self.cfg.srtp_profiles.is_empty() {
                return Err(fatal(
                    AlertDescription::UnsupportedExtension,
                    "申し出ていない use_srtp",
                ));
            }
            let [id] = profiles[..] else {
                return Err(fatal(
                    AlertDescription::IllegalParameter,
                    "use_srtp の保護方式が 1 つでない",
                ));
            };
            let p = SrtpProtectionProfile::from_id(id)
                .filter(|p| self.cfg.srtp_profiles.contains(p))
                .ok_or(fatal(
                    AlertDescription::IllegalParameter,
                    "申し出ていない SRTP 保護方式",
                ))?;
            if !mki.is_empty() {
                return Err(fatal(AlertDescription::IllegalParameter, "SRTP MKI"));
            }
            self.srtp = Some(p);
        }
        self.suite = Some(suite);
        self.server_random = sh.random;
        self.add_transcript(m);
        self.hs = Hs::ClientWaitCertificate;
        Ok(())
    }

    fn on_server_key_exchange(&mut self, m: &Message) -> Result<(), DtlsError> {
        let ske = hs::ServerKeyExchange::parse(&m.body).map_err(decode_err)?;
        if ske.curve_type != hs::CURVE_TYPE_NAMED {
            return Err(fatal(
                AlertDescription::IllegalParameter,
                "named_curve ではない",
            ));
        }
        let group = NamedGroup::from_id(ske.group)
            .filter(|g| self.cfg.groups.contains(g))
            .ok_or(fatal(
                AlertDescription::IllegalParameter,
                "申し出ていない曲線",
            ))?;
        if ske.sig_alg != hs::SIG_ECDSA_P256_SHA256 {
            return Err(fatal(AlertDescription::IllegalParameter, "署名方式"));
        }
        let mut signed = Vec::with_capacity(64 + ske.params.len());
        signed.extend_from_slice(&self.client_random);
        signed.extend_from_slice(&self.server_random);
        signed.extend_from_slice(&ske.params);
        self.verify_peer_signature(&signed, &ske.signature)?;
        self.group = Some(group);
        self.peer_ecdhe_public = ske.public;
        self.add_transcript(m);
        self.hs = Hs::ClientWaitCertRequestOrDone;
        Ok(())
    }

    fn on_certificate_request(&mut self, m: &Message) -> Result<(), DtlsError> {
        let cr = hs::CertificateRequest::parse(&m.body).map_err(decode_err)?;
        if !cr.cert_types.contains(&hs::CERT_TYPE_ECDSA_SIGN)
            || !cr.sig_algs.contains(&hs::SIG_ECDSA_P256_SHA256)
        {
            return Err(fatal(
                AlertDescription::HandshakeFailure,
                "証明書の要求に応えられない",
            ));
        }
        self.cert_requested = true;
        self.add_transcript(m);
        self.hs = Hs::ClientWaitDone;
        Ok(())
    }

    fn on_server_hello_done(&mut self, m: &Message, now: Instant) -> Result<(), DtlsError> {
        if !m.body.is_empty() {
            return Err(fatal(
                AlertDescription::DecodeError,
                "ServerHelloDone が空でない",
            ));
        }
        self.add_transcript(m);
        let group = self
            .group
            .ok_or(fatal(AlertDescription::InternalError, "曲線が無い"))?;
        let (eph, public) = Ephemeral::generate(group)?;
        let pms = eph.agree(&self.peer_ecdhe_public).ok_or(fatal(
            AlertDescription::IllegalParameter,
            "相手の ECDHE 公開値",
        ))?;
        let mut flight = Vec::new();
        if self.cert_requested {
            let body = hs::encode_certificate(&[self.cfg.certificate.der()]);
            self.push_message(&mut flight, 0, hs::HT_CERTIFICATE, body);
        }
        self.push_message(
            &mut flight,
            0,
            hs::HT_CLIENT_KEY_EXCHANGE,
            hs::encode_client_key_exchange(&public),
        );
        // extended master secret の session_hash は ClientKeyExchange までを含む（RFC 7627 3）。
        self.derive_master(&pms)?;
        if self.cert_requested {
            let sig: Signature = self.cfg.certificate.signing_key().sign(&self.transcript);
            let body = hs::encode_signed(sig.to_der().as_bytes());
            self.push_message(&mut flight, 0, hs::HT_CERTIFICATE_VERIFY, body);
        }
        flight.push(FlightItem::ChangeCipherSpec);
        self.install_keys()?;
        let verify = self.finished_data(b"client finished")?;
        self.push_message(&mut flight, 1, hs::HT_FINISHED, verify.to_vec());
        self.write_epoch = 1;
        self.hs = Hs::ClientWaitFinished;
        self.send_new_flight(flight, false, now)
    }

    fn on_server_finished(&mut self, m: &Message) -> Result<(), DtlsError> {
        let expect = self.finished_data(b"server finished")?;
        if m.body.len() != 12 || !bool::from(m.body.ct_eq(&expect[..])) {
            return Err(fatal(
                AlertDescription::DecryptError,
                "Finished が一致しない",
            ));
        }
        self.add_transcript(m);
        // client の最後の flight は不要になった（server が F6 を送ったのは F5 を受けた証拠）。
        self.flight.clear();
        self.connected();
        Ok(())
    }

    // ---- server ----

    fn on_initial_client_hello(
        &mut self,
        rseq: u64,
        f: &Fragment<'_>,
        now: Instant,
    ) -> Result<(), DtlsError> {
        // 状態を持たない段階なので、分割された・壊れた ClientHello は黙って捨てる。
        if f.offset != 0 || f.data.len() != f.length as usize {
            return Ok(());
        }
        let Ok(ch) = hs::ClientHello::parse(f.data) else {
            return Ok(());
        };
        if let Some(key) = &self.cfg.cookie_key
            && !key.verify(&self.cfg.peer_transport_id, &ch)
        {
            let cookie = key.make(&self.cfg.peer_transport_id, &ch);
            self.out
                .push_back(hello_verify_datagram(rseq, f.seq, &cookie));
            return Ok(());
        }
        // ここから状態を持つ。
        self.reasm.set_next_seq(f.seq.wrapping_add(1));
        self.send_msg_seq = f.seq;
        self.send_seq[0] = self.send_seq[0].max(rseq);
        self.transcript = hs::message_bytes(hs::HT_CLIENT_HELLO, f.seq, f.data);

        // DTLS の版は値が小さいほど新しい。0xFEFD より大きい（古い）版だけの相手は拒否する。
        if ch.version > DTLS12 {
            return Err(fatal(
                AlertDescription::ProtocolVersion,
                "DTLS 1.2 に対応しない client",
            ));
        }
        if !ch.compression.contains(&0) {
            return Err(fatal(AlertDescription::IllegalParameter, "無圧縮が無い"));
        }
        let suite = self
            .cfg
            .cipher_suites
            .iter()
            .copied()
            .find(|s| ch.cipher_suites.contains(&s.id()))
            .ok_or(fatal(
                AlertDescription::HandshakeFailure,
                "共通の cipher suite が無い",
            ))?;
        if !ch.ext.ems {
            return Err(fatal(
                AlertDescription::HandshakeFailure,
                "extended master secret が無い",
            ));
        }
        if ch.ext.reneg.as_ref().is_some_and(|r| !r.is_empty()) {
            return Err(fatal(
                AlertDescription::HandshakeFailure,
                "renegotiation_info が空でない",
            ));
        }
        let reneg = ch.ext.reneg.is_some() || ch.cipher_suites.contains(&hs::SCSV_RENEGOTIATION);
        if !ch
            .ext
            .sig_algs
            .as_ref()
            .is_some_and(|l| l.contains(&hs::SIG_ECDSA_P256_SHA256))
        {
            return Err(fatal(
                AlertDescription::HandshakeFailure,
                "ecdsa_secp256r1_sha256 が無い",
            ));
        }
        // supported_groups は ECDSA の曲線も制約する。P-256 の証明書で署名するので secp256r1 が要る（RFC 8422 5.1）。
        if ch
            .ext
            .groups
            .as_ref()
            .is_some_and(|l| !l.contains(&NamedGroup::Secp256r1.id()))
        {
            return Err(fatal(
                AlertDescription::HandshakeFailure,
                "supported_groups に secp256r1 が無い",
            ));
        }
        let group = match &ch.ext.groups {
            Some(l) => self
                .cfg
                .groups
                .iter()
                .copied()
                .find(|g| l.contains(&g.id())),
            // 拡張が無ければ P-256 を使う（RFC 8422 4）。
            None => self
                .cfg
                .groups
                .iter()
                .copied()
                .find(|g| *g == NamedGroup::Secp256r1),
        }
        .ok_or(fatal(
            AlertDescription::HandshakeFailure,
            "共通の曲線が無い",
        ))?;
        if ch
            .ext
            .point_formats
            .as_ref()
            .is_some_and(|p| !p.contains(&0))
        {
            return Err(fatal(
                AlertDescription::IllegalParameter,
                "ec_point_formats に uncompressed が無い",
            ));
        }
        self.srtp = match &ch.ext.use_srtp {
            Some((offered, _mki)) => self
                .cfg
                .srtp_profiles
                .iter()
                .copied()
                .find(|p| offered.contains(&p.id())),
            None => None,
        };
        self.suite = Some(suite);
        self.group = Some(group);
        self.client_random = ch.random;
        getrandom::fill(&mut self.server_random).map_err(|_| DtlsError::RandomUnavailable)?;

        let mut flight = Vec::new();
        let sh = hs::encode_server_hello(&hs::ServerHelloParams {
            random: &self.server_random,
            suite: suite.id(),
            srtp: self.srtp.map(|p| p.id()),
            reneg,
            point_formats: ch.ext.point_formats.is_some(),
        });
        self.push_message(&mut flight, 0, hs::HT_SERVER_HELLO, sh);
        let cert = hs::encode_certificate(&[self.cfg.certificate.der()]);
        self.push_message(&mut flight, 0, hs::HT_CERTIFICATE, cert);
        let (eph, public) = Ephemeral::generate(group)?;
        self.ephemeral = Some(eph);
        let params = hs::encode_ecdh_params(group.id(), &public);
        let mut signed = Vec::with_capacity(64 + params.len());
        signed.extend_from_slice(&self.client_random);
        signed.extend_from_slice(&self.server_random);
        signed.extend_from_slice(&params);
        let sig: Signature = self.cfg.certificate.signing_key().sign(&signed);
        let ske = hs::encode_server_key_exchange(&params, sig.to_der().as_bytes());
        self.push_message(&mut flight, 0, hs::HT_SERVER_KEY_EXCHANGE, ske);
        if self.cfg.require_peer_certificate {
            self.push_message(
                &mut flight,
                0,
                hs::HT_CERTIFICATE_REQUEST,
                hs::encode_certificate_request(),
            );
            self.cert_requested = true;
            self.hs = Hs::ServerWaitCertificate;
        } else {
            self.hs = Hs::ServerWaitKeyExchange;
        }
        self.push_message(&mut flight, 0, hs::HT_SERVER_HELLO_DONE, Vec::new());
        self.send_new_flight(flight, false, now)
    }

    fn on_client_key_exchange(&mut self, m: &Message) -> Result<(), DtlsError> {
        let public = hs::parse_client_key_exchange(&m.body).map_err(decode_err)?;
        let eph = self
            .ephemeral
            .take()
            .ok_or(fatal(AlertDescription::InternalError, "一時鍵が無い"))?;
        let pms = eph.agree(&public).ok_or(fatal(
            AlertDescription::IllegalParameter,
            "相手の ECDHE 公開値",
        ))?;
        self.add_transcript(m);
        self.derive_master(&pms)?;
        self.install_keys()?;
        self.hs = if self.peer_key.is_some() {
            Hs::ServerWaitCertificateVerify
        } else {
            Hs::ServerWaitFinished
        };
        Ok(())
    }

    fn on_certificate_verify(&mut self, m: &Message) -> Result<(), DtlsError> {
        let (alg, sig) = hs::parse_signed(&m.body).map_err(decode_err)?;
        if alg != hs::SIG_ECDSA_P256_SHA256 {
            return Err(fatal(AlertDescription::IllegalParameter, "署名方式"));
        }
        let transcript = std::mem::take(&mut self.transcript);
        let r = self.verify_peer_signature(&transcript, &sig);
        self.transcript = transcript;
        r?;
        self.add_transcript(m);
        self.hs = Hs::ServerWaitFinished;
        Ok(())
    }

    fn on_client_finished(&mut self, m: &Message, now: Instant) -> Result<(), DtlsError> {
        let expect = self.finished_data(b"client finished")?;
        if m.body.len() != 12 || !bool::from(m.body.ct_eq(&expect[..])) {
            return Err(fatal(
                AlertDescription::DecryptError,
                "Finished が一致しない",
            ));
        }
        self.add_transcript(m);
        let verify = self.finished_data(b"server finished")?;
        let mut flight = vec![FlightItem::ChangeCipherSpec];
        self.push_message(&mut flight, 1, hs::HT_FINISHED, verify.to_vec());
        self.write_epoch = 1;
        // 最後の flight には timer を付けない。相手が F5 を再送したら送り直す。
        self.send_new_flight(flight, true, now)?;
        self.connected();
        Ok(())
    }

    // ---- 共通 ----

    fn on_certificate(&mut self, m: &Message) -> Result<(), DtlsError> {
        let chain = hs::parse_certificate(&m.body).map_err(decode_err)?;
        let Some(leaf) = chain.first() else {
            if self.cfg.role == Role::Server && !self.cfg.require_peer_certificate {
                self.add_transcript(m);
                return Ok(());
            }
            return Err(fatal(AlertDescription::HandshakeFailure, "証明書が無い"));
        };
        let key = certificate_public_key(leaf).map_err(|_| {
            fatal(
                AlertDescription::UnsupportedCertificate,
                "ECDSA P-256 ではない",
            )
        })?;
        let fp: [u8; 32] = Sha256::digest(leaf).into();
        if let Some(expected) = &self.cfg.expected_peer_fingerprint
            && !fingerprint_matches(&fp, expected)
        {
            return Err(fatal(
                AlertDescription::BadCertificate,
                "fingerprint が一致しない",
            ));
        }
        self.peer_cert = Some(leaf.clone());
        self.peer_key = Some(key);
        self.peer_fp = Some(fp);
        self.add_transcript(m);
        Ok(())
    }

    fn verify_peer_signature(&self, data: &[u8], sig: &[u8]) -> Result<(), DtlsError> {
        let key = self
            .peer_key
            .as_ref()
            .ok_or(fatal(AlertDescription::InternalError, "相手の鍵が無い"))?;
        let sig = Signature::from_der(sig)
            .map_err(|_| fatal(AlertDescription::DecryptError, "署名を解析できない"))?;
        key.verify(data, &sig)
            .map_err(|_| fatal(AlertDescription::DecryptError, "署名が正しくない"))
    }

    fn derive_master(&mut self, pms: &[u8]) -> Result<(), DtlsError> {
        let suite = self
            .suite
            .ok_or(fatal(AlertDescription::InternalError, "suite が無い"))?;
        let h = suite.prf_hash();
        let session_hash = h.digest(&self.transcript);
        let mut ms = Zeroizing::new([0u8; 48]);
        prf(
            h,
            pms,
            b"extended master secret",
            &session_hash,
            &mut ms[..],
        );
        self.master = Some(ms);
        Ok(())
    }

    fn install_keys(&mut self) -> Result<(), DtlsError> {
        let suite = self
            .suite
            .ok_or(fatal(AlertDescription::InternalError, "suite が無い"))?;
        let master = self.master.as_ref().ok_or(fatal(
            AlertDescription::InternalError,
            "master secret が無い",
        ))?;
        let k = suite.key_len();
        let iv = suite.fixed_iv_len();
        let mut kb = Zeroizing::new(vec![0u8; 2 * k + 2 * iv]);
        let mut seed = [0u8; 64];
        seed[..32].copy_from_slice(&self.server_random);
        seed[32..].copy_from_slice(&self.client_random);
        prf(
            suite.prf_hash(),
            &master[..],
            b"key expansion",
            &seed,
            &mut kb,
        );
        let client = CipherState::new(suite, &kb[..k], &kb[2 * k..2 * k + iv]);
        let server = CipherState::new(suite, &kb[k..2 * k], &kb[2 * k + iv..]);
        let (write, read) = match self.cfg.role {
            Role::Client => (client, server),
            Role::Server => (server, client),
        };
        self.write_cipher = Some(write);
        self.read_cipher = Some(read);
        Ok(())
    }

    fn finished_data(&self, label: &[u8]) -> Result<[u8; 12], DtlsError> {
        let suite = self
            .suite
            .ok_or(fatal(AlertDescription::InternalError, "suite が無い"))?;
        let master = self.master.as_ref().ok_or(fatal(
            AlertDescription::InternalError,
            "master secret が無い",
        ))?;
        let h = suite.prf_hash();
        let mut out = [0u8; 12];
        prf(h, &master[..], label, &h.digest(&self.transcript), &mut out);
        Ok(out)
    }

    fn connected(&mut self) {
        self.hs = Hs::Done;
        self.state = State::Connected;
        self.retransmit_at = None;
        self.ephemeral = None;
        self.events.push_back(DtlsEvent::Connected);
        for d in std::mem::take(&mut self.early_app) {
            self.events.push_back(DtlsEvent::ApplicationData(d));
        }
    }

    /// handshake message を flight に足し、transcript に入れる。
    fn push_message(
        &mut self,
        flight: &mut Vec<FlightItem>,
        epoch: u16,
        msg_type: u8,
        body: Vec<u8>,
    ) {
        let seq = self.send_msg_seq;
        self.send_msg_seq = self.send_msg_seq.wrapping_add(1);
        self.transcript
            .extend_from_slice(&hs::message_bytes(msg_type, seq, &body));
        flight.push(FlightItem::Handshake {
            epoch,
            msg_type,
            seq,
            body,
        });
    }

    fn send_new_flight(
        &mut self,
        flight: Vec<FlightItem>,
        last: bool,
        now: Instant,
    ) -> Result<(), DtlsError> {
        self.flight = flight;
        self.retransmits = 0;
        self.rto = self.cfg.initial_retransmit_timeout;
        self.peer_flight_start = self.reasm.next_seq();
        self.transmit_flight()?;
        self.retransmit_at = if last { None } else { Some(now + self.rto) };
        Ok(())
    }

    /// 現在の flight を MTU に合わせて分割し、datagram に詰めて送る。
    fn transmit_flight(&mut self) -> Result<(), DtlsError> {
        let mtu = self.cfg.mtu;
        let items = std::mem::take(&mut self.flight);
        let mut dgram = Vec::with_capacity(mtu);
        let mut result = Ok(());
        'outer: for item in &items {
            match item {
                FlightItem::Handshake {
                    epoch,
                    msg_type,
                    seq,
                    body,
                } => {
                    let overhead = RECORD_HEADER_LEN
                        + hs::HANDSHAKE_HEADER_LEN
                        + if *epoch == 1 {
                            self.suite.map_or(0, |s| s.overhead())
                        } else {
                            0
                        };
                    let max = mtu - overhead;
                    let mut off = 0;
                    loop {
                        let n = (body.len() - off).min(max);
                        let frag = hs::fragment_bytes(
                            *msg_type,
                            *seq,
                            body.len(),
                            off,
                            &body[off..off + n],
                        );
                        match self.seal_record(*epoch, CT_HANDSHAKE, &frag) {
                            Ok(rec) => self.push_record(&mut dgram, rec),
                            Err(e) => {
                                result = Err(e);
                                break 'outer;
                            }
                        }
                        off += n;
                        if off >= body.len() {
                            break;
                        }
                    }
                }
                FlightItem::ChangeCipherSpec => {
                    match self.seal_record(0, CT_CHANGE_CIPHER_SPEC, &[1]) {
                        Ok(rec) => self.push_record(&mut dgram, rec),
                        Err(e) => {
                            result = Err(e);
                            break 'outer;
                        }
                    }
                }
            }
        }
        if !dgram.is_empty() {
            self.out.push_back(dgram);
        }
        self.flight = items;
        result
    }

    fn push_record(&mut self, dgram: &mut Vec<u8>, rec: Vec<u8>) {
        if !dgram.is_empty() && dgram.len() + rec.len() > self.cfg.mtu {
            self.out.push_back(std::mem::take(dgram));
        }
        dgram.extend_from_slice(&rec);
    }

    fn seal_record(
        &mut self,
        epoch: u16,
        content_type: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>, DtlsError> {
        let idx = epoch as usize;
        let seq = self.send_seq[idx];
        if seq > MAX_SEQ {
            return Err(DtlsError::SequenceExhausted);
        }
        self.send_seq[idx] = seq + 1;
        let body = if epoch == 0 {
            payload.to_vec()
        } else {
            let c = self
                .write_cipher
                .as_ref()
                .ok_or(fatal(AlertDescription::InternalError, "鍵が無い"))?;
            c.seal(epoch, seq, content_type, payload)
        };
        let mut out = Vec::with_capacity(RECORD_HEADER_LEN + body.len());
        record::encode_header(&mut out, content_type, DTLS12, epoch, seq, body.len());
        out.extend_from_slice(&body);
        Ok(out)
    }

    fn send_alert(&mut self, level: u8, desc: AlertDescription) -> Result<(), DtlsError> {
        let rec = self.seal_record(self.write_epoch, CT_ALERT, &[level, desc.code()])?;
        self.out.push_back(rec);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn set_send_seq_for_test(&mut self, epoch: u16, seq: u64) {
        self.send_seq[epoch as usize] = seq;
    }
}

/// 組み立て中の分割された ClientHello。
#[derive(Debug)]
struct SplitHello {
    seq: u16,
    data: Vec<u8>,
    have: Vec<bool>,
    missing: usize,
    rseq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DtlsCertificate;

    /// 最初の ClientHello が 2 つ（逆順・重なりあり）の record に分かれていても handshake できる（Chrome 156 の形）。
    #[test]
    fn fragmented_initial_client_hello() {
        let now = Instant::now();
        let cc = DtlsCertificate::generate_self_signed("c").unwrap();
        let mut c = DtlsEndpoint::new(DtlsConfig::client(cc), now).unwrap();
        let mut s = server();
        let ch = c.poll_transmit().unwrap();
        // record を外し、handshake message を 2 つの断片に分けて別の record にする
        let recs = record::split_records(&ch);
        let rec = &recs[0];
        let frags = hs::parse_fragments(rec.body).unwrap();
        let m = &frags[0];
        let cut = m.data.len() * 2 / 3;
        let mk = |rseq: u64, off: usize, end: usize| {
            let mut frag = Vec::new();
            frag.push(m.msg_type);
            frag.extend_from_slice(&m.length.to_be_bytes()[1..]);
            frag.extend_from_slice(&m.seq.to_be_bytes());
            frag.extend_from_slice(&(off as u32).to_be_bytes()[1..]);
            frag.extend_from_slice(&((end - off) as u32).to_be_bytes()[1..]);
            frag.extend_from_slice(&m.data[off..end]);
            let mut out = Vec::new();
            record::encode_header(&mut out, CT_HANDSHAKE, DTLS12, 0, rseq, frag.len());
            out.extend_from_slice(&frag);
            out
        };
        let second = mk(1, cut - 10, m.data.len());
        let first = mk(0, 0, cut);
        s.handle_datagram(&second, now).unwrap();
        assert!(s.poll_transmit().is_none());
        // 偽の断片（長さが違う）は無視されて組み立て直す
        s.handle_datagram(&mk(5, 0, 3)[..], now).ok();
        s.handle_datagram(&second, now).unwrap();
        s.handle_datagram(&first, now).unwrap();
        for _ in 0..10 {
            while let Some(d) = s.poll_transmit() {
                let _ = c.handle_datagram(&d, now);
            }
            while let Some(d) = c.poll_transmit() {
                let _ = s.handle_datagram(&d, now);
            }
        }
        assert!(c.is_connected() && s.is_connected());
        // 上限を超える長さは受けない
        let mut s = server();
        // 長さ 0x010000（64 KiB）を名乗る断片
        let mut frag = vec![hs::HT_CLIENT_HELLO];
        frag.extend_from_slice(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 1, 2, 3, 4]);
        let mut out = Vec::new();
        record::encode_header(&mut out, CT_HANDSHAKE, DTLS12, 0, 0, frag.len());
        out.append(&mut frag);
        s.handle_datagram(&out, now).unwrap();
        assert!(s.split_hello.is_none());
    }

    #[test]
    fn sequence_exhaustion_refused() {
        let now = Instant::now();
        let cc = DtlsCertificate::generate_self_signed("c").unwrap();
        let sc = DtlsCertificate::generate_self_signed("s").unwrap();
        let mut c = DtlsEndpoint::new(DtlsConfig::client(cc), now).unwrap();
        let mut s = DtlsEndpoint::new(DtlsConfig::server(sc), now).unwrap();
        for _ in 0..10 {
            while let Some(d) = c.poll_transmit() {
                let _ = s.handle_datagram(&d, now);
            }
            while let Some(d) = s.poll_transmit() {
                let _ = c.handle_datagram(&d, now);
            }
        }
        assert!(c.is_connected() && s.is_connected());
        c.set_send_seq_for_test(1, MAX_SEQ);
        c.send(b"last").unwrap();
        assert_eq!(c.send(b"over"), Err(DtlsError::SequenceExhausted));
    }

    use crate::codec::{put_u16, put_vec8, put_vec16, u16s};

    fn hs_datagram(epoch0_seq: u64, msg_type: u8, msg_seq: u16, body: &[u8]) -> Vec<u8> {
        let frag = hs::message_bytes(msg_type, msg_seq, body);
        let mut out = Vec::new();
        record::encode_header(&mut out, CT_HANDSHAKE, DTLS12, 0, epoch0_seq, frag.len());
        out.extend_from_slice(&frag);
        out
    }

    fn ext(ty: u16, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        put_u16(&mut v, ty);
        put_vec16(&mut v, data);
        v
    }

    fn std_exts() -> Vec<(u16, Vec<u8>)> {
        let mut groups = Vec::new();
        put_vec16(&mut groups, &u16s(&[29, 23]));
        let mut sig = Vec::new();
        put_vec16(&mut sig, &u16s(&[hs::SIG_ECDSA_P256_SHA256]));
        vec![
            (hs::EXT_EXTENDED_MASTER_SECRET, vec![]),
            (hs::EXT_RENEGOTIATION_INFO, vec![0]),
            (hs::EXT_SUPPORTED_GROUPS, groups),
            (hs::EXT_SIGNATURE_ALGORITHMS, sig),
        ]
    }

    fn client_hello(version: u16, suites: &[u16], comp: &[u8], exts: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut v = Vec::new();
        put_u16(&mut v, version);
        v.extend_from_slice(&[5u8; 32]);
        put_vec8(&mut v, &[]);
        put_vec8(&mut v, &[]);
        put_vec16(&mut v, &u16s(suites));
        put_vec8(&mut v, comp);
        let e: Vec<u8> = exts.iter().flat_map(|(t, d)| ext(*t, d)).collect();
        put_vec16(&mut v, &e);
        v
    }

    fn server() -> DtlsEndpoint {
        let sc = DtlsCertificate::generate_self_signed("s").unwrap();
        DtlsEndpoint::new(DtlsConfig::server(sc), Instant::now()).unwrap()
    }

    fn expect_alert(r: Result<(), DtlsError>, alert: AlertDescription) {
        match r {
            Err(DtlsError::Fatal { alert: a, .. }) => assert_eq!(a, alert),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn server_rejects_bad_client_hellos() {
        let now = Instant::now();
        let without = |t: u16| {
            std_exts()
                .into_iter()
                .filter(|(x, _)| *x != t)
                .collect::<Vec<_>>()
        };
        let cases: Vec<(Vec<u8>, AlertDescription)> = vec![
            // extended master secret が無い
            (
                client_hello(
                    DTLS12,
                    &[0xC02B],
                    &[0],
                    &without(hs::EXT_EXTENDED_MASTER_SECRET),
                ),
                AlertDescription::HandshakeFailure,
            ),
            // 圧縮だけ
            (
                client_hello(DTLS12, &[0xC02B], &[1], &std_exts()),
                AlertDescription::IllegalParameter,
            ),
            // 弱い・未対応の suite だけ（RSA・CBC・NULL）
            (
                client_hello(DTLS12, &[0x002F, 0xC013, 0x0000, 0xC02F], &[0], &std_exts()),
                AlertDescription::HandshakeFailure,
            ),
            // signature_algorithms が無い（SHA-1 既定になる）
            (
                client_hello(
                    DTLS12,
                    &[0xC02B],
                    &[0],
                    &without(hs::EXT_SIGNATURE_ALGORITHMS),
                ),
                AlertDescription::HandshakeFailure,
            ),
            // 空でない renegotiation_info
            (
                client_hello(DTLS12, &[0xC02B], &[0], &{
                    let mut e = without(hs::EXT_RENEGOTIATION_INFO);
                    e.push((
                        hs::EXT_RENEGOTIATION_INFO,
                        vec![12, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                    ));
                    e
                }),
                AlertDescription::HandshakeFailure,
            ),
            // DTLS 1.0 だけ
            (
                client_hello(DTLS10, &[0xC02B], &[0], &std_exts()),
                AlertDescription::ProtocolVersion,
            ),
            // 共通の曲線が無い（x448 だけ）
            (
                client_hello(DTLS12, &[0xC02B], &[0], &{
                    let mut e = without(hs::EXT_SUPPORTED_GROUPS);
                    let mut g = Vec::new();
                    put_vec16(&mut g, &u16s(&[30]));
                    e.push((hs::EXT_SUPPORTED_GROUPS, g));
                    e
                }),
                AlertDescription::HandshakeFailure,
            ),
        ];
        for (i, (ch, alert)) in cases.into_iter().enumerate() {
            let mut s = server();
            let r = s.handle_datagram(&hs_datagram(0, hs::HT_CLIENT_HELLO, 0, &ch), now);
            expect_alert(r, alert);
            let a = s.poll_transmit().expect("fatal alert を送る");
            assert_eq!(a[0], CT_ALERT, "case {i}");
            assert_eq!(&a[13..], &[ALERT_FATAL, alert.code()]);
            assert!(s.is_closed());
        }
        // 正しいものは通る（対照）
        let mut s = server();
        s.handle_datagram(
            &hs_datagram(
                0,
                hs::HT_CLIENT_HELLO,
                0,
                &client_hello(DTLS12, &[0xC02B], &[0], &std_exts()),
            ),
            now,
        )
        .unwrap();
        assert!(s.poll_transmit().is_some());
    }

    fn server_hello(exts: &[(u16, Vec<u8>)], comp: u8, suite: u16) -> Vec<u8> {
        let mut v = Vec::new();
        put_u16(&mut v, DTLS12);
        v.extend_from_slice(&[6u8; 32]);
        put_vec8(&mut v, &[]);
        put_u16(&mut v, suite);
        v.push(comp);
        let e: Vec<u8> = exts.iter().flat_map(|(t, d)| ext(*t, d)).collect();
        put_vec16(&mut v, &e);
        v
    }

    #[test]
    fn client_rejects_bad_server_hellos() {
        let now = Instant::now();
        let ems = (hs::EXT_EXTENDED_MASTER_SECRET, vec![]);
        let cases: Vec<(Vec<u8>, AlertDescription)> = vec![
            (
                server_hello(&[], 0, 0xC02B),
                AlertDescription::HandshakeFailure,
            ),
            (
                server_hello(std::slice::from_ref(&ems), 1, 0xC02B),
                AlertDescription::IllegalParameter,
            ),
            (
                server_hello(std::slice::from_ref(&ems), 0, 0x002F),
                AlertDescription::IllegalParameter,
            ),
            (
                server_hello(&[ems.clone(), (35, vec![])], 0, 0xC02B),
                AlertDescription::UnsupportedExtension,
            ),
            (
                server_hello(
                    &[ems.clone(), (hs::EXT_RENEGOTIATION_INFO, vec![1, 9])],
                    0,
                    0xC02B,
                ),
                AlertDescription::HandshakeFailure,
            ),
            (
                server_hello(
                    &[ems.clone(), (hs::EXT_USE_SRTP, vec![0, 2, 0, 8, 0])],
                    0,
                    0xC02B,
                ),
                AlertDescription::IllegalParameter,
            ),
            (
                server_hello(
                    &[ems.clone(), (hs::EXT_USE_SRTP, vec![0, 2, 0, 7, 1, 9])],
                    0,
                    0xC02B,
                ),
                AlertDescription::IllegalParameter,
            ),
        ];
        for (sh, alert) in cases {
            let cc = DtlsCertificate::generate_self_signed("c").unwrap();
            let mut c = DtlsEndpoint::new(DtlsConfig::client(cc), now).unwrap();
            c.poll_transmit().unwrap();
            expect_alert(
                c.handle_datagram(&hs_datagram(0, hs::HT_SERVER_HELLO, 0, &sh), now),
                alert,
            );
        }
        // 対照: EMS と use_srtp（申し出たもの）なら通る
        let cc = DtlsCertificate::generate_self_signed("c").unwrap();
        let mut c = DtlsEndpoint::new(DtlsConfig::client(cc), now).unwrap();
        let sh = server_hello(&[ems, (hs::EXT_USE_SRTP, vec![0, 2, 0, 7, 0])], 0, 0xC02B);
        c.handle_datagram(&hs_datagram(0, hs::HT_SERVER_HELLO, 0, &sh), now)
            .unwrap();
        assert_eq!(c.srtp_profile(), Some(SrtpProtectionProfile::AeadAes128Gcm));
    }

    fn connected_pair() -> (DtlsEndpoint, DtlsEndpoint) {
        let now = Instant::now();
        let cc = DtlsCertificate::generate_self_signed("c").unwrap();
        let sc = DtlsCertificate::generate_self_signed("s").unwrap();
        let mut c = DtlsEndpoint::new(DtlsConfig::client(cc), now).unwrap();
        let mut s = DtlsEndpoint::new(DtlsConfig::server(sc), now).unwrap();
        for _ in 0..10 {
            while let Some(d) = c.poll_transmit() {
                let _ = s.handle_datagram(&d, now);
            }
            while let Some(d) = s.poll_transmit() {
                let _ = c.handle_datagram(&d, now);
            }
        }
        assert!(c.is_connected() && s.is_connected());
        while c.poll_event().is_some() {}
        while s.poll_event().is_some() {}
        (c, s)
    }

    #[test]
    fn renegotiation_refused() {
        let now = Instant::now();
        let (mut c, mut s) = connected_pair();
        // client が epoch 1 で新しい ClientHello を送る
        let ch = client_hello(DTLS12, &[0xC02B], &[0], &std_exts());
        let seq = c.send_msg_seq;
        let rec = c
            .seal_record(
                1,
                CT_HANDSHAKE,
                &hs::message_bytes(hs::HT_CLIENT_HELLO, seq, &ch),
            )
            .unwrap();
        s.handle_datagram(&rec, now).unwrap();
        assert!(s.is_connected());
        let alert = s.poll_transmit().unwrap();
        c.handle_datagram(&alert, now).unwrap();
        assert!(c.is_connected(), "warning の no_renegotiation では切れない");
        // server が HelloRequest を送る
        let seq = s.send_msg_seq;
        let rec = s
            .seal_record(
                1,
                CT_HANDSHAKE,
                &hs::message_bytes(hs::HT_HELLO_REQUEST, seq, &[]),
            )
            .unwrap();
        c.handle_datagram(&rec, now).unwrap();
        assert!(c.is_connected());
        let a = c.poll_transmit().unwrap();
        assert_eq!(a[0], CT_ALERT);
        assert_eq!(a[3..5], [0, 1], "epoch 1 で暗号化した alert");
        // 平文（epoch 0）の新しい ClientHello は無視する
        s.handle_datagram(&hs_datagram(50, hs::HT_CLIENT_HELLO, 5, &ch), now)
            .unwrap();
        assert!(s.is_connected());
        assert!(s.poll_transmit().is_none());
        // その後も通信できる
        c.send(b"still").unwrap();
        s.handle_datagram(&c.poll_transmit().unwrap(), now).unwrap();
        assert_eq!(
            s.poll_event(),
            Some(DtlsEvent::ApplicationData(b"still"[..].into()))
        );
    }

    #[test]
    fn ephemeral_rejects_bad_points() {
        let (x, _) = Ephemeral::generate(NamedGroup::X25519).unwrap();
        assert!(x.agree(&[0u8; 32]).is_none());
        assert!(x.agree(&[1u8; 31]).is_none());
        let (p, public) = Ephemeral::generate(NamedGroup::Secp256r1).unwrap();
        assert!(p.agree(&public).is_some());
        let mut bad = public.clone();
        bad[40] ^= 1;
        assert!(p.agree(&bad).is_none());
        assert!(p.agree(&public[..33]).is_none());
    }
}
