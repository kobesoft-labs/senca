//! DTLS 1.2 の server（DtlsEndpoint::handle_datagram）。入力は「長さ 2 バイト・データグラム」の並び。
//! 先頭 1 バイトの bit0 で HelloVerifyRequest の cookie を求める。
#![no_main]
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use libfuzzer_sys::fuzz_target;
use senca_dtls::{CookieKey, DtlsCertificate, DtlsConfig, DtlsEndpoint};

fn cert() -> &'static DtlsCertificate {
    static C: OnceLock<DtlsCertificate> = OnceLock::new();
    C.get_or_init(|| DtlsCertificate::generate_self_signed("senca-fuzz").expect("cert"))
}

fuzz_target!(|data: &[u8]| {
    let Some((&sel, mut rest)) = data.split_first() else { return };
    let mut cfg = DtlsConfig::server(cert().clone());
    cfg.require_peer_certificate = sel & 2 != 0;
    if sel & 1 != 0 {
        cfg.cookie_key = Some(CookieKey::from_secret([7u8; 32]));
        cfg.peer_transport_id = b"192.0.2.1:5000".to_vec();
    }
    let mut now = Instant::now();
    let Ok(mut ep) = DtlsEndpoint::new(cfg, now) else { return };
    while rest.len() >= 2 {
        let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
        rest = &rest[2..];
        let len = len.min(rest.len());
        let dg = &rest[..len];
        rest = &rest[len..];
        now += Duration::from_millis(10);
        let _ = ep.handle_datagram(dg, now);
        while ep.poll_transmit().is_some() {}
        while ep.poll_event().is_some() {}
        if let Some(t) = ep.poll_timeout() {
            if t <= now {
                let _ = ep.handle_timeout(now);
            }
        }
        if ep.is_closed() {
            break;
        }
    }
});
