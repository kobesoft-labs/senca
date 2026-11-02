//! ICE 候補（RFC 8445 5.1）と SDP の candidate 属性（RFC 8839 5.1）。

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

/// candidate 行の長さの上限（壊れた入力で大きく確保しない）。
pub const MAX_CANDIDATE_LEN: usize = 1024;
/// 拡張属性の数の上限。
const MAX_EXTENSIONS: usize = 16;

/// 候補の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CandidateKind {
    Host,
    ServerReflexive,
    PeerReflexive,
    Relayed,
}

impl CandidateKind {
    /// type preference（RFC 8445 5.1.2.2 の推奨値）。
    pub fn type_preference(self) -> u32 {
        match self {
            CandidateKind::Host => 126,
            CandidateKind::PeerReflexive => 110,
            CandidateKind::ServerReflexive => 100,
            CandidateKind::Relayed => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            CandidateKind::Host => "host",
            CandidateKind::ServerReflexive => "srflx",
            CandidateKind::PeerReflexive => "prflx",
            CandidateKind::Relayed => "relay",
        }
    }
}

/// 輸送。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Transport {
    Udp,
    /// TCP（RFC 6544、`tcptype` を伴う）。対にしない。
    Tcp,
    Other(String),
}

/// 候補のアドレス。ブラウザーは私設アドレスを mDNS の `.local` 名で隠すことがある。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CandidateAddress {
    Ip(SocketAddr),
    /// mDNS の名前（`xxxxxxxx-....local`）と port。
    Mdns {
        name: String,
        port: u16,
    },
}

/// ICE 候補。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub foundation: String,
    pub component: u16,
    pub transport: Transport,
    pub priority: u32,
    pub address: CandidateAddress,
    pub kind: CandidateKind,
    /// raddr/rport。
    pub related: Option<SocketAddr>,
    /// TCP 候補の tcptype。
    pub tcptype: Option<String>,
    /// それ以外の拡張（generation・ufrag・network-id・network-cost など）。
    pub extensions: Vec<(String, String)>,
}

/// 候補の解析の誤り。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateParseError {
    TooLong,
    MissingField(&'static str),
    BadFoundation,
    BadComponent,
    BadPriority,
    BadAddress,
    BadPort,
    BadType,
    BadRelated,
    TooManyExtensions,
}

impl fmt::Display for CandidateParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "candidate を解析できない: {self:?}")
    }
}

impl std::error::Error for CandidateParseError {}

/// 候補の priority（RFC 8445 5.1.2.1）。
pub fn compute_priority(kind: CandidateKind, local_preference: u16, component: u16) -> u32 {
    (kind.type_preference() << 24)
        | ((local_preference as u32) << 8)
        | (256 - component.clamp(1, 256) as u32)
}

/// foundation（RFC 8445 5.1.1.3）: 種類・base の IP・STUN/TURN サーバー・輸送が同じなら同じ値。
pub fn compute_foundation(
    kind: CandidateKind,
    base_ip: IpAddr,
    server: Option<IpAddr>,
    transport: &Transport,
) -> String {
    // FNV-1a（識別用で秘密ではない）
    let mut h: u32 = 0x811c_9dc5;
    let mut feed = |b: &[u8]| {
        for x in b {
            h ^= *x as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
    };
    feed(kind.as_str().as_bytes());
    feed(base_ip.to_string().as_bytes());
    if let Some(s) = server {
        feed(s.to_string().as_bytes());
    }
    feed(format!("{transport:?}").as_bytes());
    h.to_string()
}

fn is_ice_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '/'
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic())
}

impl Candidate {
    /// IP アドレス（mDNS 名なら `None`）。
    pub fn addr(&self) -> Option<SocketAddr> {
        match &self.address {
            CandidateAddress::Ip(a) => Some(*a),
            CandidateAddress::Mdns { .. } => None,
        }
    }

    /// 拡張属性の値。
    pub fn extension(&self, name: &str) -> Option<&str> {
        self.extensions
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// SDP の `a=` を除いた `candidate:...` の形。
    pub fn to_sdp_attribute(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let transport = match &self.transport {
            Transport::Udp => "udp",
            Transport::Tcp => "tcp",
            Transport::Other(s) => s.as_str(),
        };
        let (addr, port) = match &self.address {
            CandidateAddress::Ip(a) => (a.ip().to_string(), a.port()),
            CandidateAddress::Mdns { name, port } => (name.clone(), *port),
        };
        write!(
            f,
            "candidate:{} {} {} {} {} {} typ {}",
            self.foundation,
            self.component,
            transport,
            self.priority,
            addr,
            port,
            self.kind.as_str()
        )?;
        if let Some(r) = self.related {
            write!(f, " raddr {} rport {}", r.ip(), r.port())?;
        }
        if let Some(t) = &self.tcptype {
            write!(f, " tcptype {t}")?;
        }
        for (k, v) in &self.extensions {
            write!(f, " {k} {v}")?;
        }
        Ok(())
    }
}

impl FromStr for Candidate {
    type Err = CandidateParseError;

    /// `a=candidate:...`・`candidate:...`・先頭なし のどれでも受け付ける。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        use CandidateParseError as E;
        if s.len() > MAX_CANDIDATE_LEN {
            return Err(E::TooLong);
        }
        let s = s.trim();
        let s = s.strip_prefix("a=").unwrap_or(s);
        let s = s.strip_prefix("candidate:").unwrap_or(s);
        let mut it = s.split(' ').filter(|x| !x.is_empty());
        let foundation = it.next().ok_or(E::MissingField("foundation"))?;
        if foundation.is_empty() || foundation.len() > 32 || !foundation.chars().all(is_ice_char) {
            return Err(E::BadFoundation);
        }
        let component = it.next().ok_or(E::MissingField("component"))?;
        if component.len() > 3 || !component.bytes().all(|b| b.is_ascii_digit()) {
            return Err(E::BadComponent);
        }
        let component: u16 = component.parse().map_err(|_| E::BadComponent)?;
        if !(1..=256).contains(&component) {
            return Err(E::BadComponent);
        }
        let transport = it.next().ok_or(E::MissingField("transport"))?;
        let transport = if transport.eq_ignore_ascii_case("udp") {
            Transport::Udp
        } else if transport.eq_ignore_ascii_case("tcp") {
            Transport::Tcp
        } else if is_token(transport) {
            Transport::Other(transport.to_string())
        } else {
            return Err(E::MissingField("transport"));
        };
        let priority = it.next().ok_or(E::MissingField("priority"))?;
        if priority.len() > 10 || !priority.bytes().all(|b| b.is_ascii_digit()) {
            return Err(E::BadPriority);
        }
        let priority: u32 = priority.parse().map_err(|_| E::BadPriority)?;
        if priority == 0 || priority > 0x7FFF_FFFF {
            return Err(E::BadPriority);
        }
        let addr = it.next().ok_or(E::MissingField("connection-address"))?;
        let port: u16 = it
            .next()
            .ok_or(E::MissingField("port"))?
            .parse()
            .map_err(|_| E::BadPort)?;
        let address = if let Ok(ip) = addr.parse::<IpAddr>() {
            CandidateAddress::Ip(SocketAddr::new(ip, port))
        } else if addr.len() <= 255
            && addr.to_ascii_lowercase().ends_with(".local")
            && addr
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        {
            CandidateAddress::Mdns {
                name: addr.to_ascii_lowercase(),
                port,
            }
        } else {
            return Err(E::BadAddress);
        };
        if it.next() != Some("typ") {
            return Err(E::MissingField("typ"));
        }
        let kind = match it.next().ok_or(E::MissingField("cand-type"))? {
            "host" => CandidateKind::Host,
            "srflx" => CandidateKind::ServerReflexive,
            "prflx" => CandidateKind::PeerReflexive,
            "relay" => CandidateKind::Relayed,
            _ => return Err(E::BadType),
        };
        let mut related_ip: Option<IpAddr> = None;
        let mut related_port: Option<u16> = None;
        let mut tcptype = None;
        let mut extensions = Vec::new();
        while let Some(name) = it.next() {
            let value = it.next().ok_or(E::MissingField("extension value"))?;
            match name {
                // mDNS の raddr（隠された related）は 0.0.0.0 と同じに扱う。
                "raddr" => related_ip = Some(value.parse().unwrap_or(IpAddr::from([0, 0, 0, 0]))),
                "rport" => related_port = Some(value.parse().map_err(|_| E::BadRelated)?),
                "tcptype" => tcptype = Some(value.to_string()),
                _ => {
                    if !is_token(name) || !value.bytes().all(|b| b.is_ascii_graphic()) {
                        return Err(E::BadType);
                    }
                    if extensions.len() >= MAX_EXTENSIONS {
                        return Err(E::TooManyExtensions);
                    }
                    extensions.push((name.to_string(), value.to_string()));
                }
            }
        }
        let related = match (related_ip, related_port) {
            (Some(ip), Some(p)) => Some(SocketAddr::new(ip, p)),
            (None, None) => None,
            _ => return Err(E::BadRelated),
        };
        Ok(Candidate {
            foundation: foundation.to_string(),
            component,
            transport,
            priority,
            address,
            kind,
            related,
            tcptype,
            extensions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_host_and_srflx() {
        let c: Candidate =
            "candidate:842163049 1 udp 1677729535 203.0.113.7 61665 typ srflx raddr 192.168.1.5 rport 61665 generation 0 ufrag sKh8 network-id 1 network-cost 10"
                .parse()
                .unwrap();
        assert_eq!(c.kind, CandidateKind::ServerReflexive);
        assert_eq!(c.addr(), Some("203.0.113.7:61665".parse().unwrap()));
        assert_eq!(c.related, Some("192.168.1.5:61665".parse().unwrap()));
        assert_eq!(c.extension("ufrag"), Some("sKh8"));
        assert_eq!(c.priority, 1677729535);
        let back: Candidate = c.to_string().parse().unwrap();
        assert_eq!(back, c);

        let c: Candidate = "a=candidate:1 1 UDP 2122260223 2001:db8::5 50000 typ host"
            .parse()
            .unwrap();
        assert_eq!(c.addr(), Some("[2001:db8::5]:50000".parse().unwrap()));
        assert_eq!(
            c.to_string(),
            "candidate:1 1 udp 2122260223 2001:db8::5 50000 typ host"
        );
    }

    #[test]
    fn mdns_and_tcp() {
        let c: Candidate =
            "candidate:3442447574 1 udp 2122260223 1f7a0d2c-29b9-4d5c-a1c3-2bd2bdd1e1b6.local 54400 typ host generation 0"
                .parse()
                .unwrap();
        assert!(matches!(
            c.address,
            CandidateAddress::Mdns { port: 54400, .. }
        ));
        assert_eq!(c.addr(), None);
        // Firefox: TCP の active は port 9
        let c: Candidate = "candidate:2 1 TCP 2105458943 192.0.2.3 9 typ host tcptype active"
            .parse()
            .unwrap();
        assert_eq!(c.transport, Transport::Tcp);
        assert_eq!(c.tcptype.as_deref(), Some("active"));
        // Firefox の relay
        let c: Candidate =
            "candidate:5 1 UDP 8331263 198.51.100.4 52931 typ relay raddr 198.51.100.4 rport 52931"
                .parse()
                .unwrap();
        assert_eq!(c.kind, CandidateKind::Relayed);
        // mDNS で隠された raddr
        let c: Candidate =
            "candidate:6 1 udp 1685987071 203.0.113.9 3000 typ srflx raddr 0.0.0.0 rport 0"
                .parse()
                .unwrap();
        assert_eq!(c.related, Some("0.0.0.0:0".parse().unwrap()));
    }

    #[test]
    fn rejects() {
        for s in [
            "",
            "candidate:",
            "candidate:1 1 udp 1 1.2.3.4 5 typ",
            "candidate:1 0 udp 1 1.2.3.4 5 typ host",
            "candidate:1 1 udp 0 1.2.3.4 5 typ host",
            "candidate:1 1 udp 4294967295 1.2.3.4 5 typ host",
            "candidate:1 1 udp 1 1.2.3.4 70000 typ host",
            "candidate:1 1 udp 1 example.com 5 typ host",
            "candidate:1 1 udp 1 1.2.3.4 5 type host",
            "candidate:1 1 udp 1 1.2.3.4 5 typ hosty",
            "candidate:1 1 udp 1 1.2.3.4 5 typ srflx raddr 1.2.3.4",
            "candidate:1 1 udp 1 1.2.3.4 5 typ host generation",
            "candidate:a_b 1 udp 1 1.2.3.4 5 typ host",
            "candidate:123456789012345678901234567890123 1 udp 1 1.2.3.4 5 typ host",
        ] {
            assert!(s.parse::<Candidate>().is_err(), "{s}");
        }
        assert_eq!(
            "x".repeat(2000).parse::<Candidate>(),
            Err(CandidateParseError::TooLong)
        );
    }

    #[test]
    fn priority_formula() {
        assert_eq!(compute_priority(CandidateKind::Host, 65535, 1), 2130706431);
        assert_eq!(compute_priority(CandidateKind::Relayed, 65535, 1), 16777215);
        let a = compute_foundation(
            CandidateKind::Host,
            "10.0.0.1".parse().unwrap(),
            None,
            &Transport::Udp,
        );
        let b = compute_foundation(
            CandidateKind::Host,
            "10.0.0.2".parse().unwrap(),
            None,
            &Transport::Udp,
        );
        assert_ne!(a, b);
        assert!(a.chars().all(is_ice_char) && a.len() <= 32);
    }
}
