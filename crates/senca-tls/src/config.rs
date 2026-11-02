//! client / server の設定、セッションチケットの鍵と保管。

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead as _, KeyInit, Payload};
use zeroize::Zeroizing;

use crate::cert::{CertifiedKey, ClientAuth, ServerVerifier};
use crate::crypto::{CipherSuite, NamedGroup, Secret, random, random_array};
use crate::error::Error;

/// 既定の ALPN（HTTP/1.1）。
pub const ALPN_HTTP11: &[u8] = b"http/1.1";
/// HTTP/3。
pub const ALPN_H3: &[u8] = b"h3";
/// MoQ（draft の版は設定で変える）。
pub const ALPN_MOQ00: &[u8] = b"moq-00";

/// client の設定。複数の接続で共有する（`Arc`）。
#[derive(Clone)]
pub struct ClientConfig {
    /// 提示する暗号スイート（優先順）。
    pub cipher_suites: Vec<CipherSuite>,
    /// 提示する group（優先順）。
    pub groups: Vec<NamedGroup>,
    /// 最初の ClientHello で key_share を送る group の数（先頭から）。足りなければ HRR になる。
    pub key_share_count: usize,
    /// 提示する ALPN（優先順）。空なら送らない。
    pub alpn_protocols: Vec<Vec<u8>>,
    /// サーバー証明書の検証方針。
    pub verifier: ServerVerifier,
    /// クライアント証明書（mTLS）。要求されたときだけ送る。
    pub client_cert: Option<Arc<CertifiedKey>>,
    /// セッションチケットの保管先。`None` なら再開しない。
    pub session_store: Option<Arc<dyn ClientSessionStore>>,
    /// SNI を送るか（DNS 名のときだけ送る）。
    pub enable_sni: bool,
    /// record_size_limit（RFC 8449）として広告する値。`None` なら送らない。
    pub record_size_limit: Option<u16>,
}

impl fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientConfig")
            .field("cipher_suites", &self.cipher_suites)
            .field("groups", &self.groups)
            .field("alpn_protocols", &self.alpn_protocols)
            .field("verifier", &self.verifier)
            .field("client_cert", &self.client_cert.is_some())
            .finish()
    }
}

impl ClientConfig {
    /// 既定値（全スイート・x25519 優先・SNI あり・メモリ上のチケット保管）。
    pub fn new(verifier: ServerVerifier) -> Self {
        Self {
            cipher_suites: CipherSuite::ALL.to_vec(),
            groups: NamedGroup::ALL.to_vec(),
            key_share_count: 1,
            alpn_protocols: Vec::new(),
            verifier,
            client_cert: None,
            session_store: Some(Arc::new(MemorySessionStore::new(64))),
            enable_sni: true,
            record_size_limit: None,
        }
    }
}

/// server の設定。複数の接続で共有する（`Arc`）。
#[derive(Clone)]
pub struct ServerConfig {
    /// 受け付ける暗号スイート（server の優先順）。
    pub cipher_suites: Vec<CipherSuite>,
    /// 受け付ける group（server の優先順）。
    pub groups: Vec<NamedGroup>,
    /// SNI に一致しないとき（または SNI がないとき）の証明書。`None` なら unrecognized_name。
    pub default_cert: Option<Arc<CertifiedKey>>,
    /// SNI ごとの証明書。キーは小文字の DNS 名、または `*.example.com` の形の wildcard。
    pub sni_certs: HashMap<String, Arc<CertifiedKey>>,
    /// 受け付ける ALPN（server の優先順）。空なら ALPN を無視する。
    /// client が ALPN を出し、一つも一致しなければ no_application_protocol で終える。
    pub alpn_protocols: Vec<Vec<u8>>,
    /// クライアント証明書（mTLS）。
    pub client_auth: ClientAuth,
    /// セッションチケットの鍵。`None` ならチケットを出さない。
    pub ticket_keys: Option<Arc<TicketKeys>>,
    /// 一回のハンドシェイクで送るチケットの数。
    pub tickets_per_handshake: u8,
    /// チケットの寿命（秒、7 日以下）。
    pub ticket_lifetime_secs: u32,
    /// record_size_limit（RFC 8449）として広告する値。client が送ってきたときだけ応える。
    pub record_size_limit: Option<u16>,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("cipher_suites", &self.cipher_suites)
            .field("groups", &self.groups)
            .field("sni_certs", &self.sni_certs.keys().collect::<Vec<_>>())
            .field("alpn_protocols", &self.alpn_protocols)
            .field("client_auth", &self.client_auth)
            .field("tickets", &self.ticket_keys.is_some())
            .finish()
    }
}

impl ServerConfig {
    /// 既定値（全スイート・x25519 優先・チケットあり（12 時間で鍵を回す））。
    pub fn new(default_cert: Arc<CertifiedKey>) -> Result<Self, Error> {
        Ok(Self {
            cipher_suites: CipherSuite::ALL.to_vec(),
            groups: NamedGroup::ALL.to_vec(),
            default_cert: Some(default_cert),
            sni_certs: HashMap::new(),
            alpn_protocols: Vec::new(),
            client_auth: ClientAuth::None,
            ticket_keys: Some(Arc::new(TicketKeys::new(Duration::from_secs(12 * 3600))?)),
            tickets_per_handshake: 1,
            ticket_lifetime_secs: 7200,
            record_size_limit: None,
        })
    }

    /// SNI に一致する証明書を足す。
    pub fn add_sni_cert(&mut self, name: &str, cert: Arc<CertifiedKey>) {
        self.sni_certs.insert(name.trim_end_matches('.').to_ascii_lowercase(), cert);
    }

    pub(crate) fn resolve_cert(&self, sni: Option<&str>) -> Option<Arc<CertifiedKey>> {
        if let Some(n) = sni {
            if let Some(c) = self.sni_certs.get(n) {
                return Some(c.clone());
            }
            if let Some((_, rest)) = n.split_once('.')
                && let Some(c) = self.sni_certs.get(&format!("*.{rest}"))
            {
                return Some(c.clone());
            }
        }
        self.default_cert.clone()
    }
}

// ---------------------------------------------------------------------------
// チケットの鍵（server）
// ---------------------------------------------------------------------------

/// 一つの鍵が暗号化してよいチケットの数（ランダム nonce の衝突確率を抑える）。
const TICKETS_PER_KEY: u64 = 1 << 24;

struct TicketKey {
    id: [u8; 4],
    aead: Aes256Gcm,
    created_ms: u64,
    used: u64,
}

impl TicketKey {
    fn generate(now_ms: u64) -> Result<Self, Error> {
        let k: Zeroizing<[u8; 32]> = Zeroizing::new(random_array()?);
        Ok(Self {
            id: random_array()?,
            aead: Aes256Gcm::new_from_slice(&k[..]).map_err(|_| Error::internal("チケット鍵"))?,
            created_ms: now_ms,
            used: 0,
        })
    }
}

/// セッションチケットを暗号化する鍵（stateless）。一定時間ごとに新しい鍵へ回し、
/// 一つ前の鍵でも復号できるようにする。チケットの寿命は回す周期以下にする。
pub struct TicketKeys {
    rotation_ms: u64,
    state: Mutex<(Option<TicketKey>, Option<TicketKey>)>,
}

impl fmt::Debug for TicketKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TicketKeys(rotation={}ms)", self.rotation_ms)
    }
}

impl TicketKeys {
    /// `rotation` ごとに鍵を回す。最初の鍵は最初の利用時に作る。
    pub fn new(rotation: Duration) -> Result<Self, Error> {
        // 乱数源が使えることをここで確かめる。
        let mut probe = [0u8; 1];
        random(&mut probe)?;
        Ok(Self {
            rotation_ms: rotation.as_millis().max(1000) as u64,
            state: Mutex::new((None, None)),
        })
    }

    /// 回す周期（ミリ秒）。
    pub fn rotation_ms(&self) -> u64 {
        self.rotation_ms
    }

    /// 次の利用で鍵を必ず回す（漏えいが疑われるときなど）。
    pub fn rotate_now(&self, now_ms: u64) -> Result<(), Error> {
        let mut st = self.state.lock().map_err(|_| Error::internal("ロックが壊れた"))?;
        let next = TicketKey::generate(now_ms)?;
        st.1 = st.0.take();
        st.0 = Some(next);
        Ok(())
    }

    fn rotate_if_needed(&self, st: &mut (Option<TicketKey>, Option<TicketKey>), now_ms: u64) -> Result<(), Error> {
        let need = match &st.0 {
            None => true,
            Some(k) => now_ms.saturating_sub(k.created_ms) >= self.rotation_ms || k.used >= TICKETS_PER_KEY,
        };
        if need {
            let next = TicketKey::generate(now_ms)?;
            st.1 = st.0.take();
            st.0 = Some(next);
        }
        // 一つ前の鍵は 2 周期を過ぎたら捨てる。
        if let Some(p) = &st.1
            && now_ms.saturating_sub(p.created_ms) >= self.rotation_ms.saturating_mul(2)
        {
            st.1 = None;
        }
        Ok(())
    }

    /// チケットを暗号化する。形式: key_id(4) || nonce(12) || AES-256-GCM(平文) 。
    pub(crate) fn seal(&self, now_ms: u64, plain: &[u8]) -> Result<Vec<u8>, Error> {
        let mut st = self.state.lock().map_err(|_| Error::internal("ロックが壊れた"))?;
        self.rotate_if_needed(&mut st, now_ms)?;
        let k = st.0.as_mut().ok_or(Error::internal("チケット鍵がない"))?;
        k.used += 1;
        let nonce: [u8; 12] = random_array()?;
        let ct = k
            .aead
            .encrypt(&nonce.into(), Payload { msg: plain, aad: &k.id })
            .map_err(|_| Error::internal("チケットの暗号化"))?;
        let mut out = Vec::with_capacity(16 + ct.len());
        out.extend_from_slice(&k.id);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// チケットを復号する。知らない鍵・改変は `None`（完全なハンドシェイクに戻る）。
    pub(crate) fn open(&self, now_ms: u64, blob: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        if blob.len() < 4 + 12 + 16 || blob.len() > 1024 {
            return None;
        }
        let mut st = self.state.lock().ok()?;
        self.rotate_if_needed(&mut st, now_ms).ok()?;
        let (id, rest) = blob.split_at(4);
        let (nonce, ct) = rest.split_at(12);
        let nonce: [u8; 12] = nonce.try_into().ok()?;
        let k = [st.0.as_ref(), st.1.as_ref()].into_iter().flatten().find(|k| k.id == id)?;
        k.aead
            .decrypt(&nonce.into(), Payload { msg: ct, aad: id })
            .ok()
            .map(Zeroizing::new)
    }
}

// ---------------------------------------------------------------------------
// チケットの保管（client）
// ---------------------------------------------------------------------------

/// client が受け取ったチケット。
#[derive(Clone)]
pub struct StoredTicket {
    pub(crate) suite: CipherSuite,
    pub(crate) ticket: Vec<u8>,
    pub(crate) psk: Secret,
    pub(crate) age_add: u32,
    pub(crate) lifetime_secs: u32,
    pub(crate) received_ms: u64,
    pub(crate) alpn: Option<Vec<u8>>,
}

impl fmt::Debug for StoredTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredTicket")
            .field("suite", &self.suite)
            .field("lifetime_secs", &self.lifetime_secs)
            .finish_non_exhaustive()
    }
}

impl StoredTicket {
    pub fn suite(&self) -> CipherSuite {
        self.suite
    }

    /// 発行時の接続で決まった ALPN。
    pub fn alpn(&self) -> Option<&[u8]> {
        self.alpn.as_deref()
    }

    pub(crate) fn valid_at(&self, now_ms: u64) -> bool {
        now_ms >= self.received_ms && now_ms - self.received_ms < u64::from(self.lifetime_secs) * 1000
    }
}

/// チケットの保管先。チケットは一回だけ使う（`take` で取り出して消す、RFC 8446 C.4）。
pub trait ClientSessionStore: Send + Sync {
    fn put(&self, server_name: &str, ticket: StoredTicket);
    fn take(&self, server_name: &str) -> Option<StoredTicket>;
}

/// メモリ上の保管先。サーバー名ごとに最新の数枚を持つ。
pub struct MemorySessionStore {
    max_names: usize,
    inner: Mutex<HashMap<String, VecDeque<StoredTicket>>>,
}

impl MemorySessionStore {
    pub fn new(max_names: usize) -> Self {
        Self {
            max_names: max_names.max(1),
            inner: Mutex::new(HashMap::new()),
        }
    }
}

impl ClientSessionStore for MemorySessionStore {
    fn put(&self, server_name: &str, ticket: StoredTicket) {
        let Ok(mut m) = self.inner.lock() else { return };
        if !m.contains_key(server_name) && m.len() >= self.max_names {
            // 満杯なら適当な一つを捨てる。
            if let Some(k) = m.keys().next().cloned() {
                m.remove(&k);
            }
        }
        let q = m.entry(server_name.to_string()).or_default();
        if q.len() >= 4 {
            q.pop_front();
        }
        q.push_back(ticket);
    }

    fn take(&self, server_name: &str) -> Option<StoredTicket> {
        let mut m = self.inner.lock().ok()?;
        let q = m.get_mut(server_name)?;
        let t = q.pop_back();
        if q.is_empty() {
            m.remove(server_name);
        }
        t
    }
}
