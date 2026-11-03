//! ノード間の接続の相互認証（ADR-0009）。合言葉そのものも、毎回同じ値（合言葉のハッシュ）も送らない。
//!
//! つないだ直後（mTLS ならその上）で、決まった長さの 3 つのメッセージを交わす:
//!
//! 1. つなぐ側 → `HELLO`: `"SNCA"` | 版（1） | 用途（1） | 自分の番号（8） | 乱数 `nc`（32）
//! 2. 受ける側 → `CHALLENGE`: 自分の番号（8） | 乱数 `ns`（32） | `HMAC(K, "server" ‖ 用途 ‖ 番号 ‖ nc ‖ ns)`（32）
//! 3. つなぐ側は、相手の番号が宛先の番号か・MAC が合うかを確かめてから → `PROOF`: `HMAC(K, "client" ‖ …)`（32）
//! 4. 受ける側が `PROOF` を確かめる。合わなければ何も返さずに閉じる。
//!
//! - `K = HKDF-SHA256(合言葉, salt = "senca-cluster/v1", info = "node-auth/" ‖ 用途)`。Raft と中継で鍵を分ける。
//! - MAC は相手が選んだ新しい乱数を含むので、見た値を送り直しても通らない（リプレイ不可）。役割の文字列が違うので、
//!   受ける側の MAC をつなぐ側の証明に使い回すこともできない。つなぐ側は相手を確かめてから自分の証明を出す。
//! - 全体で 5 秒の時間切れ。長さは固定（読みすぎない）。

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAGIC: &[u8; 4] = b"SNCA";
const VERSION: u8 = 1;
/// 認証の全体の時間切れ。
pub const TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_LEN: usize = 4 + 1 + 1 + 8 + 32;
const CHALLENGE_LEN: usize = 8 + 32 + 32;
const PROOF_LEN: usize = 32;

/// 接続の用途（鍵と MAC を分ける）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Raft = 1,
    Relay = 2,
}

impl Purpose {
    fn label(self) -> &'static [u8] {
        match self {
            Purpose::Raft => b"raft",
            Purpose::Relay => b"relay",
        }
    }
}

/// 合言葉から導いた認証の鍵。
#[derive(Clone)]
pub struct AuthKey {
    key: [u8; 32],
    purpose: Purpose,
}

impl AuthKey {
    pub fn derive(secret: &str, purpose: Purpose) -> AuthKey {
        let hk = hkdf::Hkdf::<Sha256>::new(Some(b"senca-cluster/v1"), secret.as_bytes());
        let mut key = [0u8; 32];
        let info = [b"node-auth/".as_slice(), purpose.label()].concat();
        hk.expand(&info, &mut key).expect("32 bytes is a valid HKDF-SHA256 length");
        AuthKey { key, purpose }
    }

    fn mac(&self, role: &[u8], client: u64, server: u64, nc: &[u8; 32], ns: &[u8; 32]) -> [u8; 32] {
        let mut m = <Hmac<Sha256> as Mac>::new_from_slice(&self.key).expect("hmac key");
        m.update(b"senca-cluster-auth/1 ");
        m.update(role);
        m.update(&[self.purpose as u8]);
        m.update(&client.to_be_bytes());
        m.update(&server.to_be_bytes());
        m.update(nc);
        m.update(ns);
        m.finalize().into_bytes().into()
    }
}

fn nonce() -> Result<[u8; 32], String> {
    let mut n = [0u8; 32];
    getrandom::fill(&mut n).map_err(|e| format!("random: {e}"))?;
    Ok(n)
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// つなぐ側。`peer` は宛先のノードの番号（相手がそれを名乗り、合言葉を知っていることを確かめる）。
pub async fn client<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, key: &AuthKey, me: u64, peer: u64) -> Result<(), String> {
    tokio::time::timeout(TIMEOUT, async {
        let nc = nonce()?;
        let mut hello = Vec::with_capacity(HELLO_LEN);
        hello.extend_from_slice(MAGIC);
        hello.push(VERSION);
        hello.push(key.purpose as u8);
        hello.extend_from_slice(&me.to_be_bytes());
        hello.extend_from_slice(&nc);
        s.write_all(&hello).await.map_err(|e| e.to_string())?;
        s.flush().await.map_err(|e| e.to_string())?;
        let mut ch = [0u8; CHALLENGE_LEN];
        s.read_exact(&mut ch).await.map_err(|_| "相手が認証の途中で閉じました（合言葉か版が違う可能性）".to_string())?;
        let server = u64::from_be_bytes(ch[..8].try_into().expect("8"));
        let ns: [u8; 32] = ch[8..40].try_into().expect("32");
        if server != peer {
            return Err(format!("相手が別のノードを名乗りました（{server}、宛先は {peer}）"));
        }
        if !ct_eq(&ch[40..], &key.mac(b"server", me, server, &nc, &ns)) {
            return Err("相手が合言葉を知っていません".into());
        }
        s.write_all(&key.mac(b"client", me, server, &nc, &ns)).await.map_err(|e| e.to_string())?;
        s.flush().await.map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
    .map_err(|_| "認証が時間内に終わりませんでした".to_string())?
}

/// 受ける側。認めた相手の番号を返す。`allowed` は名乗ってよい番号か。
pub async fn server<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, key: &AuthKey, me: u64, allowed: &(dyn Fn(u64) -> bool + Sync)) -> Result<u64, String> {
    tokio::time::timeout(TIMEOUT, async {
        let mut hello = [0u8; HELLO_LEN];
        s.read_exact(&mut hello).await.map_err(|_| "認証の前に閉じました".to_string())?;
        if &hello[..4] != MAGIC || hello[4] != VERSION || hello[5] != key.purpose as u8 {
            return Err("認証のメッセージが違います（版か用途が違う）".into());
        }
        let client = u64::from_be_bytes(hello[6..14].try_into().expect("8"));
        let nc: [u8; 32] = hello[14..].try_into().expect("32");
        if client == 0 || client == me || !allowed(client) {
            return Err(format!("知らないノードの番号です（{client}）"));
        }
        let ns = nonce()?;
        let mut ch = Vec::with_capacity(CHALLENGE_LEN);
        ch.extend_from_slice(&me.to_be_bytes());
        ch.extend_from_slice(&ns);
        ch.extend_from_slice(&key.mac(b"server", client, me, &nc, &ns));
        s.write_all(&ch).await.map_err(|e| e.to_string())?;
        s.flush().await.map_err(|e| e.to_string())?;
        let mut proof = [0u8; PROOF_LEN];
        s.read_exact(&mut proof).await.map_err(|_| "相手が証明を出さずに閉じました（合言葉が違う可能性）".to_string())?;
        if !ct_eq(&proof, &key.mac(b"client", client, me, &nc, &ns)) {
            return Err("合言葉が違います".into());
        }
        Ok(client)
    })
    .await
    .map_err(|_| "認証が時間内に終わりませんでした".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pair(sk: &str, ck: &str, peer: u64) -> (Result<(), String>, Result<u64, String>) {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let ka = AuthKey::derive(ck, Purpose::Raft);
        let kb = AuthKey::derive(sk, Purpose::Raft);
        let c = async move { client(&mut a, &ka, 1, peer).await };
        let s = async move { server(&mut b, &kb, 2, &|id| id == 1).await };
        tokio::join!(c, s)
    }

    #[tokio::test]
    async fn mutual_ok() {
        let (c, s) = pair("0123456789abcdef", "0123456789abcdef", 2).await;
        assert!(c.is_ok(), "{c:?}");
        assert_eq!(s.unwrap(), 1);
    }

    #[tokio::test]
    async fn wrong_secret_fails_both_sides() {
        let (c, s) = pair("0123456789abcdef", "0123456789abcdeX", 2).await;
        assert!(c.is_err());
        assert!(s.is_err());
    }

    #[tokio::test]
    async fn wrong_peer_identity_fails() {
        let (c, _s) = pair("0123456789abcdef", "0123456789abcdef", 3).await;
        assert!(c.unwrap_err().contains("別のノード"));
    }

    #[test]
    fn keys_differ_per_purpose_and_macs_per_role() {
        let a = AuthKey::derive("s3cret-s3cret-s3cret", Purpose::Raft);
        let b = AuthKey::derive("s3cret-s3cret-s3cret", Purpose::Relay);
        assert_ne!(a.key, b.key);
        let (nc, ns) = ([1u8; 32], [2u8; 32]);
        assert_ne!(a.mac(b"server", 1, 2, &nc, &ns), a.mac(b"client", 1, 2, &nc, &ns));
        // 乱数が変われば MAC も変わる（送り直しは通らない）
        assert_ne!(a.mac(b"client", 1, 2, &nc, &ns), a.mac(b"client", 1, 2, &nc, &[3u8; 32]));
    }

    /// 前の接続の証明を送り直しても通らない。
    #[tokio::test]
    async fn replayed_proof_is_rejected() {
        let key = AuthKey::derive("0123456789abcdef", Purpose::Relay);
        // 1 回目: 正しく通し、つなぐ側の送ったバイト列（HELLO と PROOF）を記録する
        let (mut a, mut b) = tokio::io::duplex(4096);
        let k1 = key.clone();
        let srv = tokio::spawn(async move { server(&mut b, &k1, 2, &|id| id == 1).await });
        client(&mut a, &key, 1, 2).await.unwrap();
        assert_eq!(srv.await.unwrap().unwrap(), 1);
        // 攻撃者: HELLO を作り、前回の PROOF に当たる任意の 32 バイトを送る（新しい ns には合わない）
        let (mut x, mut y) = tokio::io::duplex(4096);
        let k2 = key.clone();
        let srv = tokio::spawn(async move { server(&mut y, &k2, 2, &|id| id == 1).await });
        let mut hello = Vec::new();
        hello.extend_from_slice(MAGIC);
        hello.push(VERSION);
        hello.push(Purpose::Relay as u8);
        hello.extend_from_slice(&1u64.to_be_bytes());
        hello.extend_from_slice(&[7u8; 32]);
        x.write_all(&hello).await.unwrap();
        let mut ch = [0u8; CHALLENGE_LEN];
        x.read_exact(&mut ch).await.unwrap();
        let old = key.mac(b"client", 1, 2, &[7u8; 32], &[0u8; 32]);
        x.write_all(&old).await.unwrap();
        assert!(srv.await.unwrap().is_err());
    }
}
