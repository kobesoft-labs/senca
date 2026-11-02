//! SRTP/SRTCP の往復・改変・replay・ROC・寿命の試験。

use senca_srtp::{Direction, MasterKey, SrtpContext, SrtpError, SrtpProfile, dtls_srtp_contexts};

const ALL: [SrtpProfile; 4] = [
    SrtpProfile::Aes128CmHmacSha1_80,
    SrtpProfile::Aes128CmHmacSha1_32,
    SrtpProfile::AeadAes128Gcm,
    SrtpProfile::AeadAes256Gcm,
];

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn pair(profile: SrtpProfile) -> (SrtpContext, SrtpContext) {
    let km: Vec<u8> = (0..profile.keying_material_len())
        .map(|i| (i * 7 + 3) as u8)
        .collect();
    let (ctx, _) = dtls_srtp_contexts(profile, &km, true).unwrap();
    let (_, srx) = dtls_srtp_contexts(profile, &km, false).unwrap();
    (ctx, srx)
}

fn rtp(seq: u16, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, 96];
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&(seq as u32 * 3000).to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

fn rtcp(ssrc: u32) -> Vec<u8> {
    // SR（送信者情報だけ）
    let mut p = vec![0x80, 200, 0, 6];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&[0x11; 20]);
    p
}

/// libsrtp の srtp_driver にある AES_CM_128_HMAC_SHA1_80 の参照値。
#[test]
fn libsrtp_reference_aes_cm_sha1_80() {
    let key = hex("e1f97a0d3e018be0d64fa32c06de4139 0ec675ad498afeebb6960b3aabe6");
    let mk = MasterKey::from_concatenated(SrtpProfile::Aes128CmHmacSha1_80, &key).unwrap();
    let plain = hex("800f1234decafbadcafebabe abababababababababababababababab");
    let cipher =
        hex("800f1234decafbadcafebabe 4e55dc4ce79978d88ca4d215949d2402 b78d6acc99ea179b8dbb");
    let mut tx =
        SrtpContext::new(SrtpProfile::Aes128CmHmacSha1_80, &mk, Direction::Outbound).unwrap();
    assert_eq!(tx.protect_rtp(&plain).unwrap(), cipher);
    let mut rx =
        SrtpContext::new(SrtpProfile::Aes128CmHmacSha1_80, &mk, Direction::Inbound).unwrap();
    assert_eq!(rx.unprotect_rtp(&cipher).unwrap(), plain);
}

#[test]
fn roundtrip_all_profiles() {
    for profile in ALL {
        let (mut tx, mut rx) = pair(profile);
        for seq in 0..50u16 {
            let p = rtp(seq, 0x1234, &vec![seq as u8; 100 + seq as usize]);
            let s = tx.protect_rtp(&p).unwrap();
            assert_eq!(s.len(), p.len() + profile.rtp_tag_len());
            assert_ne!(s[12..p.len()], p[12..]);
            assert_eq!(rx.unprotect_rtp(&s).unwrap(), p, "{profile:?}");
            let c = rtcp(0x1234);
            let sc = tx.protect_rtcp(&c).unwrap();
            assert_eq!(sc.len(), c.len() + 4 + profile.rtcp_tag_len());
            assert_eq!(rx.unprotect_rtcp(&sc).unwrap(), c, "{profile:?}");
        }
    }
}

#[test]
fn csrc_and_extension_header_left_clear() {
    for profile in ALL {
        let (mut tx, mut rx) = pair(profile);
        // CC = 1、X = 1（one-byte 拡張 1 word）
        let mut p = vec![
            0x91, 96, 0, 1, 0, 0, 0, 1, 0, 0, 0, 9, 0, 0, 0, 7, 0xbe, 0xde, 0, 1, 0x10, 0xaa, 0, 0,
        ];
        p.extend_from_slice(b"payload bytes");
        let s = tx.protect_rtp(&p).unwrap();
        assert_eq!(s[..24], p[..24]);
        assert_eq!(rx.unprotect_rtp(&s).unwrap(), p);
    }
}

#[test]
fn tampering_rejected() {
    for profile in ALL {
        let (mut tx, mut rx) = pair(profile);
        let p = rtp(10, 5, b"hello srtp payload");
        let s = tx.protect_rtp(&p).unwrap();
        for byte in 0..s.len() {
            for bit in [0u8, 3, 7] {
                let mut bad = s.clone();
                bad[byte] ^= 1 << bit;
                let r = rx.unprotect_rtp(&bad);
                assert!(r.is_err(), "{profile:?} byte {byte} bit {bit}");
            }
        }
        // 改変の後も正しい packet は受かる（状態を汚さない）
        assert_eq!(rx.unprotect_rtp(&s).unwrap(), p);
        assert!(rx.unprotect_rtp(&s[..s.len() - 1]).is_err());

        let c = rtcp(5);
        let sc = tx.protect_rtcp(&c).unwrap();
        for byte in 0..sc.len() {
            let mut bad = sc.clone();
            bad[byte] ^= 0x01;
            assert!(
                rx.unprotect_rtcp(&bad).is_err(),
                "{profile:?} rtcp byte {byte}"
            );
        }
        assert_eq!(rx.unprotect_rtcp(&sc).unwrap(), c);
    }
}

#[test]
fn replay_rejected_and_reorder_accepted() {
    for profile in ALL {
        let (mut tx, mut rx) = pair(profile);
        let pkts: Vec<_> = (0..200u16)
            .map(|s| tx.protect_rtp(&rtp(s, 1, b"x")).unwrap())
            .collect();
        // 逆順の一部（window 内）は受ける
        for i in (100..200).rev() {
            rx.unprotect_rtp(&pkts[i]).unwrap();
        }
        assert_eq!(rx.unprotect_rtp(&pkts[150]), Err(SrtpError::Replayed));
        // 既定 window 128: 199 - 50 = 149 > 127 で古すぎる
        assert_eq!(rx.unprotect_rtp(&pkts[50]), Err(SrtpError::TooOld));
        assert_eq!(rx.unprotect_rtp(&pkts[72]).map(|_| ()), Ok(()));
        assert_eq!(rx.unprotect_rtp(&pkts[72]), Err(SrtpError::Replayed));
    }
}

#[test]
fn replay_window_configurable() {
    let profile = SrtpProfile::AeadAes128Gcm;
    let km = vec![9u8; profile.keying_material_len()];
    let (mut tx, _) = dtls_srtp_contexts(profile, &km, true).unwrap();
    let (_, rx) = dtls_srtp_contexts(profile, &km, false).unwrap();
    let mut rx = rx.with_replay_window(1024);
    let pkts: Vec<_> = (0..1100u16)
        .map(|s| tx.protect_rtp(&rtp(s, 1, b"x")).unwrap())
        .collect();
    rx.unprotect_rtp(&pkts[1099]).unwrap();
    rx.unprotect_rtp(&pkts[1099 - 1023]).unwrap();
    assert_eq!(rx.unprotect_rtp(&pkts[1099 - 1024]), Err(SrtpError::TooOld));
}

#[test]
fn roc_wrap() {
    for profile in ALL {
        let (mut tx, mut rx) = pair(profile);
        let mut seq = 65500u16;
        let mut sent = Vec::new();
        for _ in 0..200 {
            let p = rtp(seq, 77, &seq.to_be_bytes());
            let s = tx.protect_rtp(&p).unwrap();
            sent.push((p, s));
            seq = seq.wrapping_add(1);
        }
        assert_eq!(tx.roc(77), Some(1));
        // 折り返しの前後を入れ替えて届けても正しい ROC で復号できる
        sent.swap(30, 40);
        for (p, s) in &sent {
            assert_eq!(&rx.unprotect_rtp(s).unwrap(), p, "{profile:?}");
        }
        assert_eq!(rx.roc(77), Some(1));
        // 再送は replay、window より前は古すぎる
        assert_eq!(rx.unprotect_rtp(&sent[150].1), Err(SrtpError::Replayed));
        assert_eq!(rx.unprotect_rtp(&sent[10].1), Err(SrtpError::TooOld));
    }
}

#[test]
fn roc_mismatch_breaks_auth() {
    // ROC は暗号文に入らないが認証に入る。受信側の ROC が違えば認証に失敗する。
    let (mut tx, mut rx) = pair(SrtpProfile::Aes128CmHmacSha1_80);
    tx.set_roc(5, 3, 100);
    let s = tx.protect_rtp(&rtp(101, 5, b"abc")).unwrap();
    assert_eq!(rx.unprotect_rtp(&s), Err(SrtpError::AuthenticationFailed));
    // 認証に失敗した SSRC の状態は作らない
    assert_eq!(rx.roc(5), None);
}

#[test]
fn sender_refuses_index_reuse() {
    for profile in ALL {
        let (mut tx, _) = pair(profile);
        tx.protect_rtp(&rtp(10, 1, b"a")).unwrap();
        tx.protect_rtp(&rtp(12, 1, b"b")).unwrap();
        assert_eq!(
            tx.protect_rtp(&rtp(10, 1, b"c")),
            Err(SrtpError::IndexReuse)
        );
        // 未使用の 11 は使える（順不同の送信）
        tx.protect_rtp(&rtp(11, 1, b"d")).unwrap();
        assert_eq!(
            tx.protect_rtp(&rtp(11, 1, b"d")),
            Err(SrtpError::IndexReuse)
        );
        // 別 SSRC は独立
        tx.protect_rtp(&rtp(10, 2, b"a")).unwrap();
    }
}

#[test]
fn roc_exhaustion_refused() {
    let (mut tx, _) = pair(SrtpProfile::AeadAes128Gcm);
    tx.set_roc(1, u32::MAX, 65534);
    tx.protect_rtp(&rtp(65535, 1, b"last")).unwrap();
    assert_eq!(
        tx.protect_rtp(&rtp(0, 1, b"wrap")),
        Err(SrtpError::KeyLifetimeExceeded)
    );
}

#[test]
fn direction_enforced() {
    let (mut tx, mut rx) = pair(SrtpProfile::Aes128CmHmacSha1_80);
    let s = tx.protect_rtp(&rtp(1, 1, b"a")).unwrap();
    assert_eq!(tx.unprotect_rtp(&s), Err(SrtpError::WrongDirection));
    assert_eq!(
        rx.protect_rtp(&rtp(1, 1, b"a")),
        Err(SrtpError::WrongDirection)
    );
}

#[test]
fn rtcp_replay_and_index() {
    let (mut tx, mut rx) = pair(SrtpProfile::Aes128CmHmacSha1_32);
    let a = tx.protect_rtcp(&rtcp(9)).unwrap();
    let b = tx.protect_rtcp(&rtcp(9)).unwrap();
    // SRTCP は _32 でも 80 bit tag
    assert_eq!(a.len(), 28 + 4 + 10);
    assert_eq!(&a[a.len() - 14..a.len() - 10], &[0x80, 0, 0, 0]);
    assert_eq!(&b[b.len() - 14..b.len() - 10], &[0x80, 0, 0, 1]);
    rx.unprotect_rtcp(&b).unwrap();
    rx.unprotect_rtcp(&a).unwrap();
    assert_eq!(rx.unprotect_rtcp(&a), Err(SrtpError::Replayed));
}

#[test]
fn stream_limit() {
    let profile = SrtpProfile::AeadAes128Gcm;
    let km = vec![1u8; profile.keying_material_len()];
    let (tx, rx) = dtls_srtp_contexts(profile, &km, true).unwrap();
    let mut tx = tx.with_max_streams(2);
    let _ = rx;
    tx.protect_rtp(&rtp(1, 1, b"a")).unwrap();
    tx.protect_rtp(&rtp(1, 2, b"a")).unwrap();
    assert_eq!(
        tx.protect_rtp(&rtp(1, 3, b"a")),
        Err(SrtpError::TooManyStreams)
    );
}

#[test]
fn dtls_key_split() {
    // RFC 5764 4.2: client_key | server_key | client_salt | server_salt
    let profile = SrtpProfile::Aes128CmHmacSha1_80;
    let km: Vec<u8> = (0..60u8).collect();
    let (client_tx, client_rx) = dtls_srtp_contexts(profile, &km, true).unwrap();
    let (server_tx, server_rx) = dtls_srtp_contexts(profile, &km, false).unwrap();
    let ck = MasterKey::new(profile, &km[0..16], &km[32..46]).unwrap();
    let mut ref_tx = SrtpContext::new(profile, &ck, Direction::Outbound).unwrap();
    let (mut client_tx, mut server_rx) = (client_tx, server_rx);
    let p = rtp(1, 1, b"from client");
    let s = client_tx.protect_rtp(&p).unwrap();
    assert_eq!(ref_tx.protect_rtp(&p).unwrap(), s);
    assert_eq!(server_rx.unprotect_rtp(&s).unwrap(), p);
    let (mut server_tx, mut client_rx) = (server_tx, client_rx);
    let p = rtp(1, 2, b"from server");
    let s = server_tx.protect_rtp(&p).unwrap();
    assert_eq!(client_rx.unprotect_rtp(&s).unwrap(), p);
    assert!(dtls_srtp_contexts(profile, &km[..59], true).is_err());
}

#[test]
fn malformed_inputs() {
    let (mut tx, mut rx) = pair(SrtpProfile::AeadAes128Gcm);
    assert_eq!(tx.protect_rtp(&[0x80; 11]), Err(SrtpError::TooShort));
    assert_eq!(tx.protect_rtp(&[0x40; 20]), Err(SrtpError::InvalidHeader));
    // CC = 15 で header が packet より長い
    assert_eq!(rx.unprotect_rtp(&[0x8f; 40]), Err(SrtpError::InvalidHeader));
    assert_eq!(rx.unprotect_rtp(&rtp(1, 1, b"")), Err(SrtpError::TooShort));
    assert_eq!(rx.unprotect_rtcp(&[0x80; 10]), Err(SrtpError::TooShort));
    for n in 0..64 {
        let junk: Vec<u8> = (0..n).map(|i| (i * 31) as u8 | 0x80).collect();
        let _ = rx.unprotect_rtp(&junk);
        let _ = rx.unprotect_rtcp(&junk);
    }
}
