//! relay 先（peer）のアドレスの許可・拒否（仕様 9.4: TURN の open relay を防ぐ）。
//!
//! 既定では loopback・私設・link-local・CGNAT・文書用・multicast・未指定などへの relay を拒否する。
//! 管理者が許可したネットワーク（`allow`）だけ解放できる。`deny` は常に優先する。

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// IP の範囲（CIDR）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl fmt::Debug for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// CIDR の誤り。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseIpNetError;

impl IpNet {
    pub fn new(addr: IpAddr, prefix: u8) -> Option<IpNet> {
        let max = if addr.is_ipv4() { 32 } else { 128 };
        (prefix <= max).then_some(IpNet { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(n), IpAddr::V4(a)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix as u32)
                };
                u32::from(n) & mask == u32::from(a) & mask
            }
            (IpAddr::V6(n), IpAddr::V6(a)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix as u32)
                };
                u128::from(n) & mask == u128::from(a) & mask
            }
            _ => false,
        }
    }
}

impl FromStr for IpNet {
    type Err = ParseIpNetError;

    /// `10.0.0.0/8`・`fd00::/8`・`192.168.1.10`（単一アドレス）。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (a, p) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = a.parse().map_err(|_| ParseIpNetError)?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match p {
            Some(p) => p.parse::<u8>().map_err(|_| ParseIpNetError)?,
            None => max,
        };
        IpNet::new(addr, prefix).ok_or(ParseIpNetError)
    }
}

/// relay 先の方針。
#[derive(Debug, Clone, Default)]
pub struct PeerPolicy {
    /// 既定で拒否する範囲のうち、許可するもの（管理者の許可ネットワーク）。
    pub allow: Vec<IpNet>,
    /// 追加で拒否する範囲（`allow` より優先）。
    pub deny: Vec<IpNet>,
}

impl PeerPolicy {
    /// 既定（特別な範囲をすべて拒否）。
    pub fn strict() -> Self {
        PeerPolicy::default()
    }

    /// relay してよいか。
    pub fn is_allowed(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        if self.deny.iter().any(|n| n.contains(ip)) {
            return false;
        }
        if self.allow.iter().any(|n| n.contains(ip)) {
            return true;
        }
        !is_special(ip)
    }
}

/// IPv4-mapped・NAT64（64:ff9b::/96）は中の IPv4 で判定する。
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let s = v6.segments();
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                let o = v6.octets();
                return IpAddr::V4(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
            }
            ip
        }
        _ => ip,
    }
}

/// 既定で relay を拒否する範囲か。
pub fn is_special(ip: IpAddr) -> bool {
    match canonical(ip) {
        IpAddr::V4(a) => is_special_v4(a),
        IpAddr::V6(a) => is_special_v6(a),
    }
}

fn is_special_v4(a: Ipv4Addr) -> bool {
    const NETS: &[([u8; 4], u8)] = &[
        ([0, 0, 0, 0], 8),       // this network
        ([10, 0, 0, 0], 8),      // 私設
        ([100, 64, 0, 0], 10),   // CGNAT
        ([127, 0, 0, 0], 8),     // loopback
        ([169, 254, 0, 0], 16),  // link-local（クラウドのメタデータを含む）
        ([172, 16, 0, 0], 12),   // 私設
        ([192, 0, 0, 0], 24),    // IETF プロトコル割当
        ([192, 0, 2, 0], 24),    // 文書用
        ([192, 88, 99, 0], 24),  // 6to4 relay anycast
        ([192, 168, 0, 0], 16),  // 私設
        ([198, 18, 0, 0], 15),   // ベンチマーク
        ([198, 51, 100, 0], 24), // 文書用
        ([203, 0, 113, 0], 24),  // 文書用
        ([224, 0, 0, 0], 4),     // multicast
        ([240, 0, 0, 0], 4),     // 予約・broadcast
    ];
    let x = u32::from(a);
    NETS.iter().any(|(n, p)| {
        let mask = u32::MAX << (32 - *p as u32);
        x & mask == u32::from_be_bytes(*n) & mask
    })
}

fn is_special_v6(a: Ipv6Addr) -> bool {
    const NETS: &[(u128, u8)] = &[
        (0, 96),                                         // 未指定・loopback・IPv4 互換（::/96）
        (0x0100_0000_0000_0000_0000_0000_0000_0000, 64), // discard
        (0x2001_0db8_0000_0000_0000_0000_0000_0000, 32), // 文書用
        (0x2001_0000_0000_0000_0000_0000_0000_0000, 23), // IETF プロトコル割当（Teredo を含む）
        (0x2002_0000_0000_0000_0000_0000_0000_0000, 16), // 6to4
        (0xfc00_0000_0000_0000_0000_0000_0000_0000, 7),  // ULA
        (0xfe80_0000_0000_0000_0000_0000_0000_0000, 10), // link-local
        (0xfec0_0000_0000_0000_0000_0000_0000_0000, 10), // site-local（廃止）
        (0xff00_0000_0000_0000_0000_0000_0000_0000, 8),  // multicast
    ];
    let x = u128::from(a);
    NETS.iter().any(|(n, p)| {
        let mask = u128::MAX << (128 - *p as u32);
        x & mask == n & mask
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn default_denies_special() {
        let p = PeerPolicy::strict();
        for s in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2001:db8::1",
        ] {
            assert!(!p.is_allowed(ip(s)), "{s} should be denied");
        }
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "172.32.0.1",
            "2a00:1450::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(p.is_allowed(ip(s)), "{s} should be allowed");
        }
    }

    #[test]
    fn allow_and_deny_lists() {
        let p = PeerPolicy {
            allow: vec!["10.0.0.0/8".parse().unwrap()],
            deny: vec!["10.9.0.0/16".parse().unwrap(), "8.8.8.8".parse().unwrap()],
        };
        assert!(p.is_allowed(ip("10.1.1.1")));
        assert!(!p.is_allowed(ip("10.9.1.1")));
        assert!(!p.is_allowed(ip("8.8.8.8")));
        assert!(!p.is_allowed(ip("192.168.0.1")));
        assert!("1.2.3.4/33".parse::<IpNet>().is_err());
        assert!("x/3".parse::<IpNet>().is_err());
    }
}
