//! 試験用の STUN クライアント（std の UDP）。Binding を送り、`MAPPED <addr>` を出す。
//!
//! `stun_client <server addr> [bind addr]`。再送は RFC 8489 の既定（RTO 500 ms・7 回）。

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_stun::{
    Class, Message, MessageBuilder, Method, Retransmission, RetransmitAction, RtoConfig,
    TransactionId,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let server: SocketAddr = args
        .next()
        .ok_or("usage: stun_client <server> [bind]")?
        .parse()?;
    let bind = args.next().unwrap_or_else(|| "0.0.0.0:0".into());
    let sock = UdpSocket::bind(bind)?;
    let tid = TransactionId::random()?;
    let req = MessageBuilder::new(Class::Request, Method::BINDING, tid)
        .fingerprint()
        .build()?;
    sock.send_to(&req, server)?;
    let mut r = Retransmission::new(Instant::now(), RtoConfig::default(), None);
    let mut buf = vec![0u8; 2048];
    loop {
        let wait = r
            .poll_timeout()
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        sock.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == server => {
                let Ok(m) = Message::parse(Bytes::copy_from_slice(&buf[..n])) else {
                    continue;
                };
                if m.transaction_id != tid {
                    continue;
                }
                match (m.class, m.mapped_address()) {
                    (Class::Success, Some(a)) => {
                        println!("MAPPED {a}");
                        if let Some(s) = m.software() {
                            println!("SOFTWARE {s}");
                        }
                        return Ok(());
                    }
                    _ => return Err(format!("error response {:?}", m.error_code()).into()),
                }
            }
            Ok(_) => {}
            Err(_) => match r.handle_timeout(Instant::now()) {
                RetransmitAction::Retransmit => {
                    sock.send_to(&req, server)?;
                }
                RetransmitAction::Wait => {}
                RetransmitAction::TimedOut => return Err("timeout".into()),
            },
        }
    }
}
