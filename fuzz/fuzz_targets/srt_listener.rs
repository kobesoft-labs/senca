//! SRT の listener（Listener::handle_datagram）。
//!
//! 入力の形: 先頭 1 バイトが設定（bit0: パスフレーズ付きで受け入れる、bit1: 拒否する）、残りは
//! 「長さ 2 バイト・データグラム」の並び。listener の cookie は乱数なので、ハンドシェイクの
//! データグラムには直前に listener が返した cookie を書き込み、接続後のデータグラムの宛先は
//! 受け入れた接続の socket ID に書き換える（ハンドシェイクの先の接続の処理まで届かせるため）。
#![no_main]
use std::net::SocketAddr;

use libfuzzer_sys::fuzz_target;
use senca_srt::{AcceptDecision, AcceptParams, Listener, ListenerConfig, Passphrase, RejectReason};

const HS_COOKIE_OFF: usize = 16 + 28;

fuzz_target!(|data: &[u8]| {
    let Some((&sel, mut rest)) = data.split_first() else { return };
    let with_pass = sel & 1 != 0;
    let reject = sel & 2 != 0;
    let Ok(mut l) = Listener::new(ListenerConfig::default(), move |_req| {
        if reject {
            return AcceptDecision::Reject(RejectReason::PEER);
        }
        let mut p = AcceptParams::default();
        if with_pass {
            p.passphrase = Passphrase::new("fuzz-passphrase-0123").ok();
        }
        AcceptDecision::Accept(p)
    }) else {
        return;
    };
    let from: SocketAddr = "192.0.2.10:40000".parse().unwrap();
    let mut now: u64 = 1_000_000_000;
    let mut cookie: Option<[u8; 4]> = None;
    while rest.len() >= 2 {
        let len = u16::from_be_bytes([rest[0], rest[1]]) as usize;
        rest = &rest[2..];
        let len = len.min(rest.len());
        let mut dg = rest[..len].to_vec();
        rest = &rest[len..];
        let is_hs = dg.len() >= HS_COOKIE_OFF + 4 && dg[0] == 0x80 && dg[1] == 0x00;
        if is_hs {
            if let Some(c) = cookie {
                dg[HS_COOKIE_OFF..HS_COOKIE_OFF + 4].copy_from_slice(&c);
            }
        } else if dg.len() >= 16 && dg[12..16] != [0, 0, 0, 0] {
            if let Some((id, _)) = l.connections().next() {
                dg[12..16].copy_from_slice(&id.0.to_be_bytes());
            }
        }
        now += 10_000;
        l.handle_datagram(now, from, &dg);
        while let Some((_, out)) = l.poll_transmit() {
            if out.len() >= HS_COOKIE_OFF + 4 && out[0] == 0x80 && out[1] == 0x00 {
                let mut c = [0u8; 4];
                c.copy_from_slice(&out[HS_COOKIE_OFF..HS_COOKIE_OFF + 4]);
                if c != [0, 0, 0, 0] {
                    cookie = Some(c);
                }
            }
        }
        while l.poll_event().is_some() {}
        while l.poll_rejection().is_some() {}
        if let Some(t) = l.poll_timeout() {
            if t <= now {
                l.handle_timeout(now);
            }
        }
    }
    l.handle_timeout(now + 10_000_000);
    while l.poll_transmit().is_some() {}
});
