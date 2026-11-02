//! ハンドシェイクの状態機械が共有する部品。
//!
//! 状態機械（`client`・`server`）はハンドシェイクメッセージを受け取り、`Action` の列を返す。
//! TCP（記録層あり）と QUIC（記録層なし）の両方がこれを使う。

use std::time::SystemTime;

use pki_types::{CertificateDer, UnixTime};

use crate::crypto::{CipherSuite, HashAlg, Hasher, NamedGroup, Secret};
use crate::error::Error;
use crate::key_schedule::next_traffic_secret;
use crate::msgs;

/// 暗号化の段階（RFC 9001 の encryption level）。0-RTT は使わない。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Level {
    /// 平文（QUIC では Initial 鍵で守られる）。ClientHello・ServerHello。
    Initial,
    /// handshake traffic secret。
    Handshake,
    /// application traffic secret。
    OneRtt,
}

/// 状態機械が出す指示。順番どおりに処理する。
#[derive(Debug)]
pub(crate) enum Action {
    /// ハンドシェイクメッセージ（ヘッダー込み）を `level` で送る。
    Message { level: Level, data: Vec<u8> },
    /// 中継機器互換の change_cipher_spec を送る（TCP だけ）。
    Ccs,
    /// 受信鍵を切り替える。
    ReadSecret { level: Level, secret: Secret },
    /// 送信鍵を切り替える。
    WriteSecret { level: Level, secret: Secret },
    /// ハンドシェイク完了。
    Complete,
}

/// transcript hash。hash が決まるまでは生のバイト列をためる。
pub(crate) struct Transcript {
    buf: Vec<u8>,
    hasher: Option<(HashAlg, Hasher)>,
}

impl Transcript {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            hasher: None,
        }
    }

    pub(crate) fn add(&mut self, m: &[u8]) {
        match &mut self.hasher {
            Some((_, h)) => h.update(m),
            None => self.buf.extend_from_slice(m),
        }
    }

    pub(crate) fn has_hash(&self) -> bool {
        self.hasher.is_some()
    }

    /// hash を決め、ためたバイト列を流し込む。
    pub(crate) fn set_hash(&mut self, alg: HashAlg) -> Result<(), Error> {
        match &self.hasher {
            Some((a, _)) if *a == alg => Ok(()),
            Some(_) => Err(Error::illegal("transcript の hash が途中で変わった")),
            None => {
                let mut h = alg.hasher();
                h.update(&self.buf);
                self.buf.clear();
                self.hasher = Some((alg, h));
                Ok(())
            }
        }
    }

    pub(crate) fn hash(&self) -> Result<Vec<u8>, Error> {
        self.hasher
            .as_ref()
            .map(|(_, h)| h.snapshot())
            .ok_or(Error::internal("transcript の hash が未定"))
    }

    /// 今の transcript に `extra` を足した hash（足しはしない）。
    pub(crate) fn hash_with(&self, extra: &[u8]) -> Result<Vec<u8>, Error> {
        let (_, h) = self.hasher.as_ref().ok_or(Error::internal("transcript の hash が未定"))?;
        let mut h = h.clone();
        h.update(extra);
        Ok(h.snapshot())
    }

    /// HRR 用: これまでの内容（ClientHello1）を message_hash に置き換える（RFC 8446 4.4.1）。
    pub(crate) fn restart_with_message_hash(&mut self, alg: HashAlg) -> Result<(), Error> {
        self.set_hash(alg)?;
        let ch1 = self.hash()?;
        let mut h = alg.hasher();
        h.update(&msgs::message_hash(&ch1));
        self.hasher = Some((alg, h));
        Ok(())
    }
}

/// ハンドシェイクメッセージの組み立て（記録やフレームをまたぐ分割を繋ぐ）。
#[derive(Default)]
pub(crate) struct HsJoiner {
    buf: Vec<u8>,
}

/// 未処理のハンドシェイクデータの上限。
const JOINER_MAX: usize = 128 * 1024 + 4 + 16 * 1024 + 256;

impl HsJoiner {
    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<(), Error> {
        if self.buf.len() + data.len() > JOINER_MAX {
            return Err(Error::illegal("ハンドシェイクデータが多すぎる"));
        }
        self.buf.extend_from_slice(data);
        Ok(())
    }

    /// 完成したメッセージを一つ取り出す（ヘッダー込み）。上限を超える長さは拒否する。
    pub(crate) fn pop(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let typ = self.buf[0];
        let len = u32::from_be_bytes([0, self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > msgs::max_message_len(typ) {
            return Err(Error::illegal("ハンドシェイクメッセージが大きすぎる"));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let rest = self.buf.split_off(4 + len);
        Ok(Some(std::mem::replace(&mut self.buf, rest)))
    }
}

/// client / server が共有する接続の情報。
pub(crate) struct Common {
    pub is_client: bool,
    /// QUIC モードなら自分の transport parameters。
    pub quic: Option<Vec<u8>>,
    pub now: UnixTime,
    pub now_ms: u64,
    pub suite: Option<CipherSuite>,
    pub group: Option<NamedGroup>,
    pub alpn: Option<Vec<u8>>,
    pub sni: Option<String>,
    pub peer_certs: Vec<CertificateDer<'static>>,
    pub peer_transport_parameters: Option<Vec<u8>>,
    pub resumed: bool,
    pub hrr: bool,
    /// 相手が広告した record_size_limit。
    pub peer_record_limit: Option<u16>,
    /// 自分が広告し相手も対応した record_size_limit。
    pub own_record_limit: Option<u16>,
    pub client_app: Option<Secret>,
    pub server_app: Option<Secret>,
    pub exporter: Option<Secret>,
    pub complete: bool,
    /// 相手の KeyUpdate が app data を挟まずに続いた回数。
    pub key_updates_in_row: u32,
}

impl Common {
    pub(crate) fn new(is_client: bool, quic: Option<Vec<u8>>, now: SystemTime) -> Self {
        let d = now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        Self {
            is_client,
            quic,
            now: UnixTime::since_unix_epoch(d),
            now_ms: d.as_millis() as u64,
            suite: None,
            group: None,
            alpn: None,
            sni: None,
            peer_certs: Vec::new(),
            peer_transport_parameters: None,
            resumed: false,
            hrr: false,
            peer_record_limit: None,
            own_record_limit: None,
            client_app: None,
            server_app: None,
            exporter: None,
            complete: false,
            key_updates_in_row: 0,
        }
    }

    pub(crate) fn hash(&self) -> Result<HashAlg, Error> {
        self.suite.map(|s| s.hash()).ok_or(Error::internal("スイートが未定"))
    }

    /// 自分の送信鍵を更新する（RFC 8446 4.6.3）。TCP だけ。
    pub(crate) fn send_key_update(&mut self, request: bool, out: &mut Vec<Action>) -> Result<(), Error> {
        if self.quic.is_some() {
            return Err(Error::InvalidState("QUIC では KeyUpdate を使わない"));
        }
        if !self.complete {
            return Err(Error::InvalidState("ハンドシェイク完了前の KeyUpdate"));
        }
        let h = self.hash()?;
        let own = if self.is_client { &mut self.client_app } else { &mut self.server_app };
        let cur = own.as_ref().ok_or(Error::internal("app secret がない"))?;
        let next = next_traffic_secret(h, cur)?;
        *own = Some(next.clone());
        out.push(Action::Message {
            level: Level::OneRtt,
            data: msgs::encode_key_update(request)?,
        });
        out.push(Action::WriteSecret {
            level: Level::OneRtt,
            secret: next,
        });
        Ok(())
    }

    /// 相手の KeyUpdate を処理する。
    pub(crate) fn recv_key_update(&mut self, msg: &[u8], out: &mut Vec<Action>) -> Result<(), Error> {
        if self.quic.is_some() {
            // RFC 9001 6: QUIC では KeyUpdate メッセージを送ってはならない。
            return Err(Error::unexpected("QUIC で KeyUpdate を受信"));
        }
        let request = msgs::parse_key_update(msg)?;
        self.key_updates_in_row += 1;
        if self.key_updates_in_row > 64 {
            return Err(Error::unexpected("KeyUpdate が多すぎる"));
        }
        let h = self.hash()?;
        let peer = if self.is_client { &mut self.server_app } else { &mut self.client_app };
        let cur = peer.as_ref().ok_or(Error::internal("app secret がない"))?;
        let next = next_traffic_secret(h, cur)?;
        *peer = Some(next.clone());
        out.push(Action::ReadSecret {
            level: Level::OneRtt,
            secret: next,
        });
        if request {
            self.send_key_update(false, out)?;
        }
        Ok(())
    }
}

/// client / server の状態機械。
pub(crate) trait Handshaker: Send {
    /// 最初の送信（client は ClientHello）。
    fn start(&mut self, out: &mut Vec<Action>) -> Result<(), Error>;
    /// 完成したハンドシェイクメッセージ（ヘッダー込み）を一つ処理する。
    fn handle(&mut self, level: Level, msg: &[u8], out: &mut Vec<Action>) -> Result<(), Error>;
    fn common(&self) -> &Common;
    fn common_mut(&mut self) -> &mut Common;
    /// 平文の change_cipher_spec を黙って捨ててよい時期か（RFC 8446 5）。
    fn accepts_ccs(&self) -> bool;
    /// client が early data を出したが断った（server、TCP）。読み飛ばしが要る。
    fn rejected_early_data(&self) -> bool {
        false
    }
}

pub(crate) fn expect_level(got: Level, want: Level) -> Result<(), Error> {
    if got == want {
        Ok(())
    } else {
        Err(Error::unexpected("想定外の暗号化段階でハンドシェイクメッセージを受信"))
    }
}
