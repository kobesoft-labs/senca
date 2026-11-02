//! 試験用の STUN Binding 応答器（std の UDP）。
//!
//! `stun_server <bind addr>` — 準備ができたら `LISTENING <addr>` を出す。
//! 理解必須の未知属性を含む要求には 420、要求でないものには応えない。

use std::net::UdpSocket;

use bytes::Bytes;
use senca_stun::{Attribute, Class, Message, binding_success, error_response, is_stun};

fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3478".into());
    let sock = UdpSocket::bind(&addr)?;
    println!("LISTENING {}", sock.local_addr()?);
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, from) = sock.recv_from(&mut buf)?;
        if !is_stun(&buf[..n]) {
            continue;
        }
        let Ok(msg) = Message::parse(Bytes::copy_from_slice(&buf[..n])) else {
            continue;
        };
        if msg.class != Class::Request {
            continue;
        }
        let b = if !msg.unknown_comprehension_required().is_empty() {
            error_response(&msg, 420, "Unknown Attribute")
        } else if msg.method != senca_stun::Method::BINDING {
            error_response(&msg, 400, "Bad Request")
        } else {
            // RFC 3489 の古いクライアント向けに MAPPED-ADDRESS も付ける。
            binding_success(&msg, from).attr(Attribute::MappedAddress(from))
        };
        if let Ok(resp) = b
            .attr(Attribute::Software("senca-stun".into()))
            .fingerprint()
            .build()
        {
            let _ = sock.send_to(&resp, from);
        }
    }
}
