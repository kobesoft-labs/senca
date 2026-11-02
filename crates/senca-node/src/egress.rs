//! 外へ出る接続の宛先の検査（仕様 9.4 の SSRF 対策）。
//!
//! 名前を解決した後の IP を見る（DNS の再解決で私設アドレスに向けられても拒否する）。既定で拒否するもの:
//! ループバック、リンクローカル（169.254.0.0/16・クラウドのメタデータを含む）、未指定、マルチキャスト、
//! 私設アドレス（10/8・172.16/12・192.168/16・100.64/10・fc00::/7・fe80::/10）。
//! 社内のカメラなどへつなぐときは、管理者が `--egress-allow CIDR` で範囲を許可する。

use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, Copy)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Cidr> {
        let (a, p) = s.split_once('/')?;
        let addr: IpAddr = a.parse().ok()?;
        let prefix: u8 = p.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        (prefix <= max).then_some(Cidr { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let m = if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix) };
                (u32::from(a) & m) == (u32::from(b) & m)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let m = if self.prefix == 0 { 0 } else { u128::MAX << (128 - self.prefix) };
                (u128::from(a) & m) == (u128::from(b) & m)
            }
            _ => false,
        }
    }
}

fn restricted(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_loopback()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_multicast()
                || v.is_broadcast()
                || v.is_private()
                || (o[0] == 100 && (o[1] & 0xC0) == 64)
                || o[0] == 0
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return restricted(IpAddr::V4(v4));
            }
            let s = v.segments();
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (s[0] & 0xFE00) == 0xFC00 || (s[0] & 0xFFC0) == 0xFE80
        }
    }
}

/// 宛先が許されるか。
pub fn allowed(ip: IpAddr, allow: &[String]) -> bool {
    if !restricted(ip) {
        return true;
    }
    allow.iter().filter_map(|c| Cidr::parse(c)).any(|c| c.contains(ip))
}

/// 名前を解決して、許される最初のアドレスを返す。許されるものが無ければ Err（理由付き）。
pub async fn resolve(host: &str, port: u16, allow: &[String]) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.map_err(|e| format!("resolve {host}: {e}"))?.collect();
    if addrs.is_empty() {
        return Err(format!("resolve {host}: no address"));
    }
    addrs
        .iter()
        .copied()
        .find(|a| allowed(a.ip(), allow))
        .ok_or_else(|| format!("{host} は許されていない宛先です（--egress-allow で許可が必要）"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy() {
        let none: Vec<String> = vec![];
        assert!(!allowed("127.0.0.1".parse().unwrap(), &none));
        assert!(!allowed("169.254.169.254".parse().unwrap(), &none));
        assert!(!allowed("10.1.2.3".parse().unwrap(), &none));
        assert!(!allowed("::1".parse().unwrap(), &none));
        assert!(!allowed("::ffff:127.0.0.1".parse().unwrap(), &none));
        assert!(!allowed("fd00::1".parse().unwrap(), &none));
        assert!(allowed("8.8.8.8".parse().unwrap(), &none));
        let lan = vec!["10.1.0.0/16".to_string()];
        assert!(allowed("10.1.2.3".parse().unwrap(), &lan));
        assert!(!allowed("10.2.0.1".parse().unwrap(), &lan));
        assert!(Cidr::parse("1.2.3.4/33").is_none());
    }
}
