//! TCP 用の接続（記録層つき、RFC 8446 5 章）。sans-io: バイト列を入れてバイト列を取り出す。
//!
//! 使い方（擬似コード）:
//!
//! ```text
//! loop {
//!     while conn.wants_write() { sock.write_all(&conn.take_tls()) }
//!     let n = sock.read(&mut buf);            // 0 なら EOF
//!     let mut off = 0;
//!     while off < n { off += conn.read_tls(&buf[off..n])?; drain plaintext }
//!     while let Some(p) = conn.take_plaintext() { ... }
//! }
//! ```

use std::sync::Arc;
use std::time::SystemTime;

use pki_types::CertificateDer;

use crate::client::ClientHs;
use crate::config::{ClientConfig, ServerConfig};
use crate::crypto::{CipherSuite, NamedGroup, RecordKey, Secret};
use crate::engine::{Action, Handshaker, HsJoiner, Level};
use crate::error::{AlertDescription, Error};
use crate::server::ServerHs;

const CT_CCS: u8 = 20;
const CT_ALERT: u8 = 21;
const CT_HANDSHAKE: u8 = 22;
const CT_APPDATA: u8 = 23;

/// TLSPlaintext.fragment の上限（2^14）。
pub(crate) const MAX_PLAINTEXT: usize = 16384;
/// TLSCiphertext.fragment の上限（2^14 + 256）。
const MAX_CIPHERTEXT: usize = 16384 + 256;
/// 受信バッファの上限（記録二つ分）。
const IN_CAP: usize = 2 * (5 + MAX_CIPHERTEXT);
/// 取り出されていない平文の上限。これを超えると `read_tls` は受け取らない。
const PLAIN_CAP: usize = 256 * 1024;
/// ハンドシェイク完了前に預かる送信データの上限。
const PENDING_CAP: usize = 256 * 1024;
/// 断った early data を読み飛ばす上限（RFC 8446 4.2.10）。
const EARLY_SKIP_MAX: usize = 64 * 1024;
/// 一回のハンドシェイクで黙って捨てる CCS の数。
const MAX_CCS: u32 = 8;
/// この数の記録を送ったら自動で KeyUpdate する（AES-GCM の上限 2^24.5 より十分小さく、RFC 8446 5.5）。
const AUTO_KEY_UPDATE: u64 = 1 << 23;
/// 受信側でこの数を超えたら接続を終える（相手が鍵を更新しない）。
const RECV_RECORD_LIMIT: u64 = 1 << 24;

/// 接続の状態をまとめた情報。
#[derive(Clone, Debug)]
pub struct HandshakeInfo {
    pub cipher_suite: Option<CipherSuite>,
    pub group: Option<NamedGroup>,
    pub alpn: Option<Vec<u8>>,
    pub server_name: Option<String>,
    pub resumed: bool,
    pub hello_retry: bool,
}

/// tokio に依存しない接続の抽象。node はこれを TcpStream に繋ぐ（async の糊は node 側）。
pub trait TlsSession {
    /// 受信した TLS のバイト列を渡す。受け取った長さを返す（0 なら平文を取り出してから再度呼ぶ）。
    fn read_tls(&mut self, data: &[u8]) -> Result<usize, Error>;
    /// 送るべき TLS のバイト列があるか。
    fn wants_write(&self) -> bool;
    /// 送るべき TLS のバイト列を取り出す。
    fn take_tls(&mut self) -> Vec<u8>;
    /// 復号済みの平文を取り出す。
    fn take_plaintext(&mut self) -> Option<Vec<u8>>;
    /// 平文を送る（ハンドシェイク中は預かる）。
    fn write_plaintext(&mut self, data: &[u8]) -> Result<usize, Error>;
    /// ハンドシェイク中か。
    fn is_handshaking(&self) -> bool;
    /// close_notify を送る。
    fn send_close_notify(&mut self);
    /// 相手が close_notify を送ってきたか。
    fn peer_closed(&self) -> bool;
}

/// TCP 用の TLS 1.3 接続（client / server）。
pub struct Connection {
    engine: Box<dyn Handshaker>,
    is_client: bool,
    suite: Option<CipherSuite>,
    read_key: Option<RecordKey>,
    read_level: Level,
    write_key: Option<RecordKey>,
    write_level: Level,
    in_buf: Vec<u8>,
    joiner: HsJoiner,
    out: Vec<u8>,
    hs_out: Vec<u8>,
    plain: std::collections::VecDeque<Vec<u8>>,
    plain_len: usize,
    pending: Vec<u8>,
    first_record: bool,
    ccs_seen: u32,
    early_skipped: usize,
    early_skip_done: bool,
    peer_closed: bool,
    sent_close: bool,
    error: Option<Error>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("is_client", &self.is_client)
            .field("handshaking", &self.is_handshaking())
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// client を作り、ClientHello を送信待ちにする。`server_name` は DNS 名か IP アドレス。
    pub fn client(config: Arc<ClientConfig>, server_name: &str, now: SystemTime) -> Result<Self, Error> {
        let engine = ClientHs::new(config, server_name, None, now)?;
        let mut c = Self::with_engine(Box::new(engine), true);
        let mut actions = Vec::new();
        c.engine.start(&mut actions)?;
        c.apply(actions)?;
        Ok(c)
    }

    /// server を作る。
    pub fn server(config: Arc<ServerConfig>, now: SystemTime) -> Result<Self, Error> {
        let engine = ServerHs::new(config, None, now)?;
        Ok(Self::with_engine(Box::new(engine), false))
    }

    fn with_engine(engine: Box<dyn Handshaker>, is_client: bool) -> Self {
        Self {
            engine,
            is_client,
            suite: None,
            read_key: None,
            read_level: Level::Initial,
            write_key: None,
            write_level: Level::Initial,
            in_buf: Vec::new(),
            joiner: HsJoiner::default(),
            out: Vec::new(),
            hs_out: Vec::new(),
            plain: Default::default(),
            plain_len: 0,
            pending: Vec::new(),
            first_record: true,
            ccs_seen: 0,
            early_skipped: 0,
            early_skip_done: false,
            peer_closed: false,
            sent_close: false,
            error: None,
        }
    }

    pub fn is_client(&self) -> bool {
        self.is_client
    }

    /// 接続が致命的なエラーで終わっていればその内容。
    pub fn error(&self) -> Option<&Error> {
        self.error.as_ref()
    }

    /// 交渉の結果。
    pub fn info(&self) -> HandshakeInfo {
        let c = self.engine.common();
        HandshakeInfo {
            cipher_suite: c.suite,
            group: c.group,
            alpn: c.alpn.clone(),
            server_name: c.sni.clone(),
            resumed: c.resumed,
            hello_retry: c.hrr,
        }
    }

    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.engine.common().alpn.as_deref()
    }

    /// 相手の証明書チェーン（leaf が先頭、検証済み）。
    pub fn peer_certificates(&self) -> &[CertificateDer<'static>] {
        &self.engine.common().peer_certs
    }

    /// TLS-Exporter（RFC 8446 7.5）。ハンドシェイク完了後だけ。
    pub fn export_keying_material(&self, label: &[u8], context: &[u8], len: usize) -> Result<Vec<u8>, Error> {
        let c = self.engine.common();
        let e = c.exporter.as_ref().ok_or(Error::InvalidState("ハンドシェイク未完了"))?;
        crate::key_schedule::export(c.hash()?, e, label, context, len)
    }

    /// 自分の送信鍵を更新する。`request_peer` なら相手にも更新を求める。
    pub fn update_keys(&mut self, request_peer: bool) -> Result<(), Error> {
        self.check_ok()?;
        let mut actions = Vec::new();
        self.engine.common_mut().send_key_update(request_peer, &mut actions)?;
        let r = self.apply(actions);
        self.guard(r)
    }

    fn check_ok(&self) -> Result<(), Error> {
        match &self.error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// エラーを記録し、送れるなら致命的 alert を送る。
    fn guard<T>(&mut self, r: Result<T, Error>) -> Result<T, Error> {
        if let Err(e) = &r
            && self.error.is_none()
        {
            if let Some(a) = e.alert()
                && !self.sent_close
            {
                self.hs_out.clear();
                let _ = self.write_record(CT_ALERT, &[2, a.0]);
                self.sent_close = true;
            }
            self.error = Some(e.clone());
        }
        r
    }

    // -----------------------------------------------------------------------
    // 送信
    // -----------------------------------------------------------------------

    fn max_fragment(&self) -> usize {
        match (&self.write_key, self.engine.common().peer_record_limit) {
            (Some(_), Some(l)) => (usize::from(l).saturating_sub(1)).clamp(63, MAX_PLAINTEXT),
            _ => MAX_PLAINTEXT,
        }
    }

    fn write_record(&mut self, typ: u8, data: &[u8]) -> Result<(), Error> {
        match &mut self.write_key {
            None => {
                let ver: u16 = if self.first_record && self.is_client { 0x0301 } else { 0x0303 };
                self.out.push(typ);
                self.out.extend_from_slice(&ver.to_be_bytes());
                self.out.extend_from_slice(&(data.len() as u16).to_be_bytes());
                self.out.extend_from_slice(data);
            }
            Some(k) => {
                let mut inner = Vec::with_capacity(data.len() + 1 + 16);
                inner.extend_from_slice(data);
                inner.push(typ);
                let len = (inner.len() + 16) as u16;
                let hdr = [CT_APPDATA, 3, 3, (len >> 8) as u8, len as u8];
                k.seal(&hdr, &mut inner)?;
                self.out.extend_from_slice(&hdr);
                self.out.extend_from_slice(&inner);
            }
        }
        self.first_record = false;
        Ok(())
    }

    fn write_fragmented(&mut self, typ: u8, data: &[u8]) -> Result<(), Error> {
        let max = self.max_fragment();
        for chunk in data.chunks(max) {
            self.write_record(typ, chunk)?;
        }
        Ok(())
    }

    fn flush_handshake(&mut self) -> Result<(), Error> {
        if !self.hs_out.is_empty() {
            let d = std::mem::take(&mut self.hs_out);
            self.write_fragmented(CT_HANDSHAKE, &d)?;
        }
        Ok(())
    }

    fn apply(&mut self, actions: Vec<Action>) -> Result<(), Error> {
        let suite = |s: &Self| s.engine.common().suite.ok_or(Error::internal("スイートが未定"));
        for a in actions {
            match a {
                Action::Message { level, data } => {
                    if level != self.write_level {
                        return Err(Error::internal("送信段階が食い違う"));
                    }
                    self.hs_out.extend_from_slice(&data);
                }
                Action::Ccs => {
                    self.flush_handshake()?;
                    // CCS は常に平文（RFC 8446 D.4）。
                    self.out.extend_from_slice(&[CT_CCS, 3, 3, 0, 1, 1]);
                }
                Action::WriteSecret { level, secret } => {
                    self.flush_handshake()?;
                    self.write_key = Some(RecordKey::from_secret(suite(self)?, &secret)?);
                    self.write_level = level;
                }
                Action::ReadSecret { level, secret } => {
                    // ハンドシェイクメッセージは鍵の切り替えをまたいではならない（RFC 8446 5.1）。
                    if !self.joiner.is_empty() {
                        return Err(Error::unexpected("鍵の切り替えをまたぐハンドシェイクメッセージ"));
                    }
                    self.install_read(level, &secret)?;
                }
                Action::Complete => {
                    self.suite = self.engine.common().suite;
                }
            }
        }
        self.flush_handshake()?;
        if self.engine.common().complete && !self.pending.is_empty() {
            let p = std::mem::take(&mut self.pending);
            self.send_appdata(&p)?;
        }
        Ok(())
    }

    fn install_read(&mut self, level: Level, secret: &Secret) -> Result<(), Error> {
        let suite = self.engine.common().suite.ok_or(Error::internal("スイートが未定"))?;
        self.read_key = Some(RecordKey::from_secret(suite, secret)?);
        self.read_level = level;
        Ok(())
    }

    fn send_appdata(&mut self, data: &[u8]) -> Result<(), Error> {
        let max = self.max_fragment();
        for chunk in data.chunks(max) {
            if self.write_key.as_ref().is_some_and(|k| k.seq() >= AUTO_KEY_UPDATE) {
                let mut actions = Vec::new();
                self.engine.common_mut().send_key_update(false, &mut actions)?;
                self.apply(actions)?;
            }
            self.write_record(CT_APPDATA, chunk)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // 受信
    // -----------------------------------------------------------------------

    fn process(&mut self) -> Result<(), Error> {
        let mut off = 0;
        let r = loop {
            if self.plain_len >= PLAIN_CAP || self.error.is_some() {
                break Ok(());
            }
            let buf = &self.in_buf[off..];
            if buf.len() < 5 {
                break Ok(());
            }
            let typ = buf[0];
            let len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
            if buf[1] != 3 {
                break Err(Error::decode("記録の版が不正"));
            }
            if len > MAX_CIPHERTEXT || (self.read_key.is_none() && typ != CT_APPDATA && len > MAX_PLAINTEXT) {
                break Err(Error::fatal(AlertDescription::RECORD_OVERFLOW, "記録が長すぎる"));
            }
            if buf.len() < 5 + len {
                break Ok(());
            }
            let mut hdr = [0u8; 5];
            hdr.copy_from_slice(&buf[..5]);
            let payload = buf[5..5 + len].to_vec();
            off += 5 + len;
            if let Err(e) = self.handle_record(hdr, payload) {
                break Err(e);
            }
        };
        self.in_buf.drain(..off);
        r
    }

    fn handle_record(&mut self, hdr: [u8; 5], mut payload: Vec<u8>) -> Result<(), Error> {
        let typ = hdr[0];
        if self.peer_closed {
            // close_notify の後のデータは無視する（RFC 8446 6.1）。
            return Ok(());
        }
        // 中継機器互換の CCS（RFC 8446 5）。
        if typ == CT_CCS {
            if payload != [1] || !self.engine.accepts_ccs() || !self.joiner.is_empty() {
                return Err(Error::unexpected("change_cipher_spec が不正"));
            }
            self.ccs_seen += 1;
            if self.ccs_seen > MAX_CCS {
                return Err(Error::unexpected("change_cipher_spec が多すぎる"));
            }
            return Ok(());
        }
        let skip_early = self.engine.rejected_early_data() && !self.early_skip_done;
        let (typ, data) = match &mut self.read_key {
            None => {
                match typ {
                    CT_HANDSHAKE | CT_ALERT => {}
                    CT_APPDATA if skip_early => return self.skip_early(payload.len()),
                    _ => return Err(Error::unexpected("平文の段階で想定外の記録")),
                }
                (typ, payload)
            }
            Some(k) => {
                if typ != CT_APPDATA {
                    return Err(Error::unexpected("暗号化の段階で平文の記録"));
                }
                if payload.len() < 17 {
                    return Err(Error::fatal(AlertDescription::BAD_RECORD_MAC, "暗号化記録が短すぎる"));
                }
                if k.seq() >= RECV_RECORD_LIMIT {
                    return Err(Error::unexpected("鍵の更新なしに記録を受け取りすぎた"));
                }
                match k.open(&hdr, &mut payload) {
                    Ok(()) => {}
                    Err(_) if skip_early => return self.skip_early(payload.len()),
                    Err(e) => return Err(e),
                }
                if skip_early {
                    self.early_skip_done = true;
                }
                // 末尾のゼロ詰めを外し、本当の種類を取り出す。
                let Some(pos) = payload.iter().rposition(|b| *b != 0) else {
                    return Err(Error::unexpected("記録の種類がない"));
                };
                let inner = payload[pos];
                payload.truncate(pos);
                let limit = self
                    .engine
                    .common()
                    .own_record_limit
                    .map(|l| usize::from(l) - 1)
                    .unwrap_or(MAX_PLAINTEXT)
                    .min(MAX_PLAINTEXT);
                if pos > limit {
                    return Err(Error::fatal(AlertDescription::RECORD_OVERFLOW, "平文が長すぎる"));
                }
                (inner, payload)
            }
        };
        match typ {
            CT_HANDSHAKE => self.handle_handshake_data(&data),
            CT_ALERT => self.handle_alert(&data),
            CT_APPDATA => {
                if !self.joiner.is_empty() {
                    return Err(Error::unexpected("ハンドシェイクメッセージの途中に app data"));
                }
                if self.read_level != Level::OneRtt {
                    return Err(Error::unexpected("ハンドシェイク完了前の app data"));
                }
                self.engine.common_mut().key_updates_in_row = 0;
                if !data.is_empty() {
                    self.plain_len += data.len();
                    self.plain.push_back(data);
                }
                Ok(())
            }
            _ => Err(Error::unexpected("想定外の記録の種類")),
        }
    }

    fn skip_early(&mut self, n: usize) -> Result<(), Error> {
        self.early_skipped += n;
        if self.early_skipped > EARLY_SKIP_MAX {
            return Err(Error::unexpected("early data が多すぎる"));
        }
        Ok(())
    }

    fn handle_handshake_data(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.is_empty() {
            return Err(Error::unexpected("空のハンドシェイク記録"));
        }
        self.joiner.push(data)?;
        while let Some(msg) = self.joiner.pop()? {
            let mut actions = Vec::new();
            self.engine.handle(self.read_level, &msg, &mut actions)?;
            self.apply(actions)?;
        }
        Ok(())
    }

    fn handle_alert(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() != 2 {
            return Err(Error::decode("alert の長さが不正"));
        }
        if !self.joiner.is_empty() {
            return Err(Error::unexpected("ハンドシェイクメッセージの途中に alert"));
        }
        let desc = AlertDescription(data[1]);
        match desc {
            AlertDescription::CLOSE_NOTIFY => {
                self.peer_closed = true;
                Ok(())
            }
            AlertDescription::USER_CANCELED => Ok(()),
            // それ以外は level を問わず致命的（RFC 8446 6.2）。
            _ => {
                self.sent_close = true; // 相手の致命的 alert には応えない。
                Err(Error::PeerAlert(desc))
            }
        }
    }
}

impl TlsSession for Connection {
    fn read_tls(&mut self, data: &[u8]) -> Result<usize, Error> {
        self.check_ok()?;
        if self.plain_len >= PLAIN_CAP {
            return Ok(0);
        }
        let n = data.len().min(IN_CAP.saturating_sub(self.in_buf.len()));
        self.in_buf.extend_from_slice(&data[..n]);
        let r = self.process();
        self.guard(r)?;
        Ok(n)
    }

    fn wants_write(&self) -> bool {
        !self.out.is_empty()
    }

    fn take_tls(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    fn take_plaintext(&mut self) -> Option<Vec<u8>> {
        let p = self.plain.pop_front()?;
        self.plain_len -= p.len();
        Some(p)
    }

    fn write_plaintext(&mut self, data: &[u8]) -> Result<usize, Error> {
        self.check_ok()?;
        if self.sent_close {
            return Err(Error::InvalidState("close_notify を送った後"));
        }
        if !self.engine.common().complete {
            let n = data.len().min(PENDING_CAP.saturating_sub(self.pending.len()));
            self.pending.extend_from_slice(&data[..n]);
            return Ok(n);
        }
        let r = self.send_appdata(data);
        self.guard(r)?;
        Ok(data.len())
    }

    fn is_handshaking(&self) -> bool {
        !self.engine.common().complete
    }

    fn send_close_notify(&mut self) {
        if !self.sent_close && self.error.is_none() {
            let _ = self.write_record(CT_ALERT, &[1, 0]);
            self.sent_close = true;
        }
    }

    fn peer_closed(&self) -> bool {
        self.peer_closed
    }
}
