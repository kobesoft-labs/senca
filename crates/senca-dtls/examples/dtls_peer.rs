//! 相互接続試験用の DTLS client/server（std の UDP）。tools/interop/dtls_srtp.sh から使う。
//!
//! dtls_peer client <host:port> [opts] / dtls_peer server <bind:port> [opts]
//!   --suite aes128|aes256|chacha   --group x25519|p256   --srtp aes128gcm,aes256gcm,cm80,cm32
//!   --cookie   --mtu N   --send TEXT   --expect TEXT   --timeout SECS   --no-client-cert
//!
//! 出力（標準出力）: CONNECTED・PEER_FINGERPRINT・KEYING_MATERIAL・RECV・CLOSED の行。

use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use senca_dtls::{
    CipherSuite, CookieDecision, CookieKey, DtlsCertificate, DtlsConfig, DtlsEndpoint, DtlsEvent,
    NamedGroup, SrtpProtectionProfile, check_client_hello, format_fingerprint,
};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn die(msg: &str) -> ! {
    eprintln!("dtls_peer: {msg}");
    std::process::exit(2)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        die("usage: dtls_peer client|server ADDR [options]");
    }
    let role = args[0].clone();
    let addr: SocketAddr = args[1].parse().unwrap_or_else(|_| die("ADDR"));
    let cert = DtlsCertificate::generate_self_signed("senca-interop")
        .unwrap_or_else(|e| die(&e.to_string()));
    println!("LOCAL_FINGERPRINT {}", cert.sdp_fingerprint());
    let mut cfg = if role == "client" {
        DtlsConfig::client(cert)
    } else {
        DtlsConfig::server(cert)
    };
    let mut send_text: Option<String> = None;
    let mut expect: Option<String> = None;
    let mut timeout = Duration::from_secs(20);
    let mut cookie = false;
    let mut i = 2;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--suite" => {
                cfg.cipher_suites = vec![match v.as_str() {
                    "aes128" => CipherSuite::EcdheEcdsaAes128GcmSha256,
                    "aes256" => CipherSuite::EcdheEcdsaAes256GcmSha384,
                    "chacha" => CipherSuite::EcdheEcdsaChacha20Poly1305Sha256,
                    _ => die("--suite"),
                }];
                i += 1;
            }
            "--group" => {
                // client は secp256r1 を必ず示す（RFC 8422 5.1）。x25519 を優先にする。
                cfg.groups = match v.as_str() {
                    "x25519" => vec![NamedGroup::X25519, NamedGroup::Secp256r1],
                    "p256" => vec![NamedGroup::Secp256r1],
                    _ => die("--group"),
                };
                i += 1;
            }
            "--srtp" => {
                cfg.srtp_profiles = v
                    .split(',')
                    .map(|p| match p {
                        "aes128gcm" => SrtpProtectionProfile::AeadAes128Gcm,
                        "aes256gcm" => SrtpProtectionProfile::AeadAes256Gcm,
                        "cm80" => SrtpProtectionProfile::Aes128CmHmacSha1_80,
                        "cm32" => SrtpProtectionProfile::Aes128CmHmacSha1_32,
                        _ => die("--srtp"),
                    })
                    .collect();
                i += 1;
            }
            "--mtu" => {
                cfg.mtu = v.parse().unwrap_or_else(|_| die("--mtu"));
                i += 1;
            }
            "--send" => {
                send_text = Some(v);
                i += 1;
            }
            "--expect" => {
                expect = Some(v);
                i += 1;
            }
            "--timeout" => {
                timeout = Duration::from_secs(v.parse().unwrap_or_else(|_| die("--timeout")));
                i += 1;
            }
            "--cookie" => cookie = true,
            "--no-client-cert" => cfg.require_peer_certificate = false,
            other => die(&format!("unknown option {other}")),
        }
        i += 1;
    }
    cfg.initial_retransmit_timeout = Duration::from_millis(500);

    let started = Instant::now();
    let deadline = started + timeout;
    let mut buf = vec![0u8; 65536];
    let (sock, mut ep) = if role == "client" {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap_or_else(|_| die("bind"));
        sock.connect(addr).unwrap_or_else(|_| die("connect"));
        let ep = DtlsEndpoint::new(cfg, Instant::now()).unwrap_or_else(|e| die(&e.to_string()));
        (sock, ep)
    } else {
        let sock = UdpSocket::bind(addr).unwrap_or_else(|_| die("bind"));
        let key = CookieKey::random().unwrap_or_else(|_| die("random"));
        // 最初の ClientHello を待つ。cookie を使う場合は状態を持たずに HelloVerifyRequest を返す。
        let (first, peer) = loop {
            sock.set_read_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(10)),
            ))
            .ok();
            let (n, peer) = sock
                .recv_from(&mut buf)
                .unwrap_or_else(|_| die("no client"));
            let d = buf[..n].to_vec();
            if !cookie {
                break (d, peer);
            }
            let pid = peer.to_string().into_bytes();
            match check_client_hello(&key, &pid, &d) {
                CookieDecision::Accept => break (d, peer),
                CookieDecision::HelloVerifyRequest(h) => {
                    println!("HELLO_VERIFY_SENT");
                    sock.send_to(&h, peer).ok();
                }
                CookieDecision::Drop => {}
            }
        };
        sock.connect(peer).unwrap_or_else(|_| die("connect"));
        if cookie {
            cfg.cookie_key = Some(key);
            cfg.peer_transport_id = peer.to_string().into_bytes();
        }
        let mut ep = DtlsEndpoint::new(cfg, Instant::now()).unwrap_or_else(|e| die(&e.to_string()));
        if let Err(e) = ep.handle_datagram(&first, Instant::now()) {
            die(&format!("handshake: {e}"));
        }
        (sock, ep)
    };

    let mut sent = false;
    let mut got_expected = expect.is_none();
    let mut done_at: Option<Instant> = None;
    loop {
        while let Some(d) = ep.poll_transmit() {
            let _ = sock.send(&d);
        }
        while let Some(ev) = ep.poll_event() {
            match ev {
                DtlsEvent::Connected => {
                    println!(
                        "CONNECTED suite=0x{:04X} group={} srtp={}",
                        ep.cipher_suite().map_or(0, |s| s.id()),
                        ep.named_group().map_or(0, |g| g.id()),
                        ep.srtp_profile()
                            .map_or("none".to_string(), |p| format!("0x{:04X}", p.id()))
                    );
                    if let Some(fp) = ep.peer_fingerprint_sha256() {
                        println!("PEER_FINGERPRINT sha-256 {}", format_fingerprint(&fp));
                    }
                    if let Ok((_, km)) = ep.export_srtp_keying_material() {
                        println!("KEYING_MATERIAL {}", hex(&km));
                    }
                }
                DtlsEvent::ApplicationData(d) => {
                    let text = String::from_utf8_lossy(&d);
                    println!("RECV {}", text.trim_end());
                    if expect.as_ref().is_some_and(|e| text.contains(e.as_str())) {
                        got_expected = true;
                    }
                }
                DtlsEvent::Closed => {
                    println!("CLOSED");
                    while let Some(d) = ep.poll_transmit() {
                        let _ = sock.send(&d);
                    }
                    std::process::exit(if got_expected && (sent || send_text.is_none()) {
                        0
                    } else {
                        1
                    });
                }
            }
        }
        if ep.is_connected() && !sent {
            if let Some(t) = &send_text {
                ep.send(format!("{t}\n").as_bytes())
                    .unwrap_or_else(|e| die(&e.to_string()));
                while let Some(d) = ep.poll_transmit() {
                    let _ = sock.send(&d);
                }
            }
            sent = true;
        }
        if ep.is_connected() && sent && got_expected && done_at.is_none() {
            done_at = Some(Instant::now() + Duration::from_millis(700));
        }
        let now = Instant::now();
        if done_at.is_some_and(|t| now >= t) {
            ep.close();
            while let Some(d) = ep.poll_transmit() {
                let _ = sock.send(&d);
            }
            println!("DONE");
            std::process::exit(0);
        }
        if now >= deadline {
            die("timeout");
        }
        let mut wait = deadline - now;
        if let Some(t) = ep.poll_timeout() {
            wait = wait.min(t.saturating_duration_since(now));
        }
        if let Some(t) = done_at {
            wait = wait.min(t.saturating_duration_since(now));
        }
        sock.set_read_timeout(Some(wait.max(Duration::from_millis(5))))
            .ok();
        match sock.recv(&mut buf) {
            Ok(n) => {
                if let Err(e) = ep.handle_datagram(&buf[..n], Instant::now()) {
                    die(&format!("handshake: {e}"));
                }
            }
            Err(_) => {
                if let Err(e) = ep.handle_timeout(Instant::now()) {
                    die(&format!("timeout: {e}"));
                }
            }
        }
    }
}
