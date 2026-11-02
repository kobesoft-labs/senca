//! QUIC モード（RFC 9001）。記録層を使わず、暗号化段階ごとにハンドシェイクのバイト列を
//! やりとりし、段階ごとの traffic secret を QUIC 層へ渡す。
//!
//! - 0-RTT は使わない（仕様 6.4）。early data の鍵は出さない。
//! - KeyUpdate メッセージは使わない（QUIC の鍵更新は [`next_secret`] で QUIC 層が行う）。
//! - transport parameters（拡張 0x39）は不透明なバイト列として QUIC 層から受け取り、相手の値を返す。
//! - TLS の alert は QUIC の CRYPTO_ERROR（0x0100 + alert）として QUIC 層が送る（[`Error::alert`]）。

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::SystemTime;

use pki_types::CertificateDer;

use crate::client::ClientHs;
use crate::config::{ClientConfig, ServerConfig};
use crate::crypto::{CipherSuite, HashAlg, Secret, hkdf_expand_label};
use crate::engine::{Action, Handshaker, HsJoiner, Level};
use crate::error::Error;
use crate::server::ServerHs;

/// QUIC 層への出力。順番どおりに処理する。
#[derive(Debug)]
pub enum QuicOutput {
    /// この段階の CRYPTO フレームで送るハンドシェイクのバイト列。
    Data { level: Level, data: Vec<u8> },
    /// この段階の受信用 secret（以後この段階のパケットを開ける）。
    ReadSecret {
        level: Level,
        suite: CipherSuite,
        secret: Secret,
    },
    /// この段階の送信用 secret。
    WriteSecret {
        level: Level,
        suite: CipherSuite,
        secret: Secret,
    },
    /// ハンドシェイク完了（client: 自分の Finished を出した、server: client の Finished を検証した）。
    HandshakeComplete,
}

/// QUIC 用の TLS 1.3 接続。
pub struct QuicConnection {
    engine: Box<dyn Handshaker>,
    joiners: [HsJoiner; 3],
    read_level: Level,
    output: VecDeque<QuicOutput>,
    error: Option<Error>,
}

impl std::fmt::Debug for QuicConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuicConnection")
            .field("read_level", &self.read_level)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

fn idx(l: Level) -> usize {
    match l {
        Level::Initial => 0,
        Level::Handshake => 1,
        Level::OneRtt => 2,
    }
}

impl QuicConnection {
    /// client を作る。`transport_parameters` は QUIC 層が符号化した自分の値。ALPN は必須。
    pub fn client(
        config: Arc<ClientConfig>,
        server_name: &str,
        transport_parameters: Vec<u8>,
        now: SystemTime,
    ) -> Result<Self, Error> {
        let engine = ClientHs::new(config, server_name, Some(transport_parameters), now)?;
        let mut q = Self::with_engine(Box::new(engine));
        let mut actions = Vec::new();
        q.engine.start(&mut actions)?;
        q.apply(actions)?;
        Ok(q)
    }

    /// server を作る。ALPN は必須。
    pub fn server(config: Arc<ServerConfig>, transport_parameters: Vec<u8>, now: SystemTime) -> Result<Self, Error> {
        let engine = ServerHs::new(config, Some(transport_parameters), now)?;
        Ok(Self::with_engine(Box::new(engine)))
    }

    fn with_engine(engine: Box<dyn Handshaker>) -> Self {
        Self {
            engine,
            joiners: Default::default(),
            read_level: Level::Initial,
            output: VecDeque::new(),
            error: None,
        }
    }

    /// `level` の CRYPTO フレームで受け取ったバイト列（順序どおりに並べ直したもの）を渡す。
    ///
    /// 期待していない段階のデータは unexpected_message。エラーの alert は
    /// QUIC の CRYPTO_ERROR として送ること。
    pub fn read_handshake(&mut self, level: Level, data: &[u8]) -> Result<(), Error> {
        if let Some(e) = &self.error {
            return Err(e.clone());
        }
        let r = self.read_inner(level, data);
        if let Err(e) = &r {
            self.error = Some(e.clone());
        }
        r
    }

    fn read_inner(&mut self, level: Level, data: &[u8]) -> Result<(), Error> {
        if level != self.read_level {
            return Err(Error::unexpected("想定外の暗号化段階の CRYPTO データ"));
        }
        self.joiners[idx(level)].push(data)?;
        while let Some(msg) = self.joiners[idx(self.read_level)].pop()? {
            let mut actions = Vec::new();
            self.engine.handle(self.read_level, &msg, &mut actions)?;
            self.apply(actions)?;
        }
        Ok(())
    }

    fn apply(&mut self, actions: Vec<Action>) -> Result<(), Error> {
        for a in actions {
            let suite = self.engine.common().suite;
            match a {
                Action::Message { level, data } => self.output.push_back(QuicOutput::Data { level, data }),
                Action::Ccs => {}
                Action::ReadSecret { level, secret } => {
                    if !self.joiners[idx(self.read_level)].is_empty() {
                        return Err(Error::unexpected("段階の切り替えをまたぐハンドシェイクメッセージ"));
                    }
                    self.read_level = level;
                    let suite = suite.ok_or(Error::internal("スイートが未定"))?;
                    self.output.push_back(QuicOutput::ReadSecret { level, suite, secret });
                }
                Action::WriteSecret { level, secret } => {
                    let suite = suite.ok_or(Error::internal("スイートが未定"))?;
                    self.output.push_back(QuicOutput::WriteSecret { level, suite, secret });
                }
                Action::Complete => self.output.push_back(QuicOutput::HandshakeComplete),
            }
        }
        Ok(())
    }

    /// 次の出力。`None` になるまで呼ぶ。
    pub fn poll_output(&mut self) -> Option<QuicOutput> {
        self.output.pop_front()
    }

    pub fn is_handshaking(&self) -> bool {
        !self.engine.common().complete
    }

    /// 今受け取れる段階。
    pub fn read_level(&self) -> Level {
        self.read_level
    }

    /// 相手の transport parameters（拡張 0x39 の中身）。
    pub fn peer_transport_parameters(&self) -> Option<&[u8]> {
        self.engine.common().peer_transport_parameters.as_deref()
    }

    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        self.engine.common().alpn.as_deref()
    }

    pub fn cipher_suite(&self) -> Option<CipherSuite> {
        self.engine.common().suite
    }

    pub fn resumed(&self) -> bool {
        self.engine.common().resumed
    }

    pub fn server_name(&self) -> Option<&str> {
        self.engine.common().sni.as_deref()
    }

    pub fn peer_certificates(&self) -> &[CertificateDer<'static>] {
        &self.engine.common().peer_certs
    }

    /// TLS-Exporter（RFC 8446 7.5）。
    pub fn export_keying_material(&self, label: &[u8], context: &[u8], len: usize) -> Result<Vec<u8>, Error> {
        let c = self.engine.common();
        let e = c.exporter.as_ref().ok_or(Error::InvalidState("ハンドシェイク未完了"))?;
        crate::key_schedule::export(c.hash()?, e, label, context, len)
    }
}

/// QUIC のパケット保護の鍵（RFC 9001 5.1）。
#[derive(Clone, PartialEq, Eq)]
pub struct PacketKeyMaterial {
    pub key: Secret,
    pub iv: Secret,
    pub hp: Secret,
}

impl std::fmt::Debug for PacketKeyMaterial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PacketKeyMaterial(<秘密>)")
    }
}

/// secret から "quic key"・"quic iv"・"quic hp" を導く。
pub fn packet_keys(suite: CipherSuite, secret: &Secret) -> Result<PacketKeyMaterial, Error> {
    let h = suite.hash();
    Ok(PacketKeyMaterial {
        key: hkdf_expand_label(h, secret.as_bytes(), b"quic key", &[], suite.key_len())?,
        iv: hkdf_expand_label(h, secret.as_bytes(), b"quic iv", &[], 12)?,
        hp: hkdf_expand_label(h, secret.as_bytes(), b"quic hp", &[], suite.key_len())?,
    })
}

/// QUIC の鍵更新（RFC 9001 6.1）: "quic ku"。
pub fn next_secret(suite: CipherSuite, secret: &Secret) -> Result<Secret, Error> {
    let h = suite.hash();
    hkdf_expand_label(h, secret.as_bytes(), b"quic ku", &[], h.output_len())
}

/// QUIC v1 の Initial secret（RFC 9001 5.2）。(client, server) を返す。
pub fn initial_secrets(client_dcid: &[u8]) -> Result<(Secret, Secret), Error> {
    const SALT_V1: [u8; 20] = [
        0x38, 0x76, 0x2c, 0xf7, 0xf5, 0x59, 0x34, 0xb3, 0x4d, 0x17, 0x9a, 0xe6, 0xa4, 0xc8, 0x0c, 0xad, 0xcc, 0xbb,
        0x7f, 0x0a,
    ];
    let h = HashAlg::Sha256;
    let initial = h.extract(&SALT_V1, client_dcid);
    Ok((
        hkdf_expand_label(h, initial.as_bytes(), b"client in", &[], 32)?,
        hkdf_expand_label(h, initial.as_bytes(), b"server in", &[], 32)?,
    ))
}
