//! RFC 8448 のトレースとの突き合わせ。

use super::hex;
use super::rfc8448_data::*;
use crate::crypto::{CipherSuite, HashAlg, KeyExchange, RecordKey, Secret, hkdf_expand_label};
use crate::engine::Transcript;
use crate::key_schedule::{KeySchedule, finished_mac, resumption_psk};
use crate::msgs;

const H: HashAlg = HashAlg::Sha256;

fn sec(s: &str) -> Secret {
    Secret::new(hex(s))
}

fn x25519(private: &str) -> KeyExchange {
    let k: [u8; 32] = hex(private).try_into().unwrap();
    KeyExchange::X25519(x25519_dalek::StaticSecret::from(k))
}

fn transcript(msgs: &[&str]) -> Transcript {
    let mut t = Transcript::new();
    t.set_hash(H).unwrap();
    for m in msgs {
        t.add(&hex(m));
    }
    t
}

/// 平文を記録として暗号化し、トレースの完全な記録と比べる。逆に復号もする。
fn check_record(suite: CipherSuite, secret: &Secret, seq: u64, inner_type: u8, plain: &str, record: &str) {
    let mut k = RecordKey::from_secret(suite, secret).unwrap();
    for _ in 0..seq {
        let mut dummy = Vec::new();
        k.seal(&[], &mut dummy).unwrap();
    }
    let mut inner = hex(plain);
    inner.push(inner_type);
    let len = (inner.len() + 16) as u16;
    let hdr = [23, 3, 3, (len >> 8) as u8, len as u8];
    k.seal(&hdr, &mut inner).unwrap();
    let mut got = hdr.to_vec();
    got.extend_from_slice(&inner);
    assert_eq!(got, hex(record), "暗号化した記録が一致しない");

    let mut k = RecordKey::from_secret(suite, secret).unwrap();
    for _ in 0..seq {
        let mut dummy = Vec::new();
        k.seal(&[], &mut dummy).unwrap();
    }
    let rec = hex(record);
    let mut body = rec[5..].to_vec();
    k.open(&rec[..5], &mut body).unwrap();
    assert_eq!(body.pop(), Some(inner_type));
    assert_eq!(body, hex(plain));
}

#[test]
fn simple_1rtt_key_schedule() {
    // ECDHE（x25519）
    let shared = x25519(S3_CLIENT_PRIV).agree(&hex(S3_SERVER_PUB)).unwrap();
    assert_eq!(shared.as_bytes(), hex(S3_ECDHE));
    let shared2 = x25519(S3_SERVER_PRIV)
        .agree(&x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(
            <[u8; 32]>::try_from(hex(S3_CLIENT_PRIV)).unwrap(),
        )).to_bytes())
        .unwrap();
    assert_eq!(shared, shared2);

    // メッセージの解析
    let ch = msgs::parse_client_hello(&hex(S3_CH)).unwrap();
    assert_eq!(ch.server_name.as_deref(), Some("server"));
    assert_eq!(ch.cipher_suites, vec![0x1301, 0x1303, 0x1302]);
    assert_eq!(ch.record_size_limit, Some(0x4001));
    assert_eq!(ch.key_shares.as_ref().unwrap()[0].0, 0x001d);
    let sh = msgs::parse_server_hello(&hex(S3_SH)).unwrap();
    assert!(!sh.is_hrr);
    assert_eq!(sh.cipher_suite, 0x1301);
    assert_eq!(sh.key_share.unwrap().1, hex(S3_SERVER_PUB));

    // 鍵スケジュール
    let mut ks = KeySchedule::early(H, None);
    assert_eq!(ks.current().as_bytes(), hex(S3_EARLY));
    ks.advance(Some(&shared)).unwrap();
    assert_eq!(ks.current().as_bytes(), hex(S3_HS_SECRET));
    let mut t = transcript(&[S3_CH, S3_SH]);
    let th = t.hash().unwrap();
    let c_hs = ks.derive(b"c hs traffic", &th).unwrap();
    let s_hs = ks.derive(b"s hs traffic", &th).unwrap();
    assert_eq!(c_hs.as_bytes(), hex(S3_C_HS));
    assert_eq!(s_hs.as_bytes(), hex(S3_S_HS));
    assert_eq!(
        hkdf_expand_label(H, s_hs.as_bytes(), b"key", &[], 16).unwrap().as_bytes(),
        hex(S3_S_HS_KEY)
    );
    assert_eq!(
        hkdf_expand_label(H, s_hs.as_bytes(), b"iv", &[], 12).unwrap().as_bytes(),
        hex(S3_S_HS_IV)
    );

    // server の Finished
    for m in [S3_EE, S3_CERT, S3_CV] {
        t.add(&hex(m));
    }
    let fin = finished_mac(H, &s_hs, &t.hash().unwrap()).unwrap();
    assert_eq!(fin, hex(S3_S_FINISHED));
    t.add(&msgs::handshake(msgs::hs::FINISHED, &fin).unwrap());

    // server の暗号化したハンドシェイク（EE..Finished を一つの記録に）
    check_record(CipherSuite::Aes128GcmSha256, &s_hs, 0, 22, S3_S_FLIGHT, S3_S_FLIGHT_RECORD);

    // application secret
    ks.advance(None).unwrap();
    assert_eq!(ks.current().as_bytes(), hex(S3_MASTER));
    let th = t.hash().unwrap();
    let c_ap = ks.derive(b"c ap traffic", &th).unwrap();
    let s_ap = ks.derive(b"s ap traffic", &th).unwrap();
    assert_eq!(c_ap.as_bytes(), hex(S3_C_AP));
    assert_eq!(s_ap.as_bytes(), hex(S3_S_AP));
    assert_eq!(ks.derive(b"exp master", &th).unwrap().as_bytes(), hex(S3_EXP));
    assert_eq!(
        hkdf_expand_label(H, s_ap.as_bytes(), b"key", &[], 16).unwrap().as_bytes(),
        hex(S3_S_AP_KEY)
    );
    assert_eq!(
        hkdf_expand_label(H, s_ap.as_bytes(), b"iv", &[], 12).unwrap().as_bytes(),
        hex(S3_S_AP_IV)
    );

    // client の Finished
    let cfin = finished_mac(H, &c_hs, &th).unwrap();
    assert_eq!(cfin, hex(S3_C_FINISHED));
    let cfin_msg = msgs::handshake(msgs::hs::FINISHED, &cfin).unwrap();
    assert_eq!(cfin_msg, hex(S3_C_FIN_MSG));
    check_record(CipherSuite::Aes128GcmSha256, &c_hs, 0, 22, S3_C_FIN_MSG, S3_C_FIN_RECORD);
    t.add(&cfin_msg);
    let res = ks.derive(b"res master", &t.hash().unwrap()).unwrap();
    assert_eq!(res.as_bytes(), hex(S3_RES_MASTER));

    // NewSessionTicket と再開用 PSK
    let nst = msgs::parse_new_session_ticket(&hex(S3_NST)).unwrap();
    assert_eq!(nst.lifetime, 30);
    assert_eq!(
        resumption_psk(H, &res, &nst.nonce).unwrap().as_bytes(),
        hex(S3_RESUMPTION_PSK)
    );
    check_record(CipherSuite::Aes128GcmSha256, &s_ap, 0, 22, S3_NST, S3_NST_RECORD);

    // app data と close_notify
    check_record(CipherSuite::Aes128GcmSha256, &c_ap, 0, 23, S3_C_APPDATA, S3_C_APPDATA_RECORD);
    check_record(CipherSuite::Aes128GcmSha256, &s_ap, 1, 23, S3_S_APPDATA, S3_S_APPDATA_RECORD);
    check_record(CipherSuite::Aes128GcmSha256, &c_ap, 1, 21, "0100", S3_C_ALERT_RECORD);
    check_record(CipherSuite::Aes128GcmSha256, &s_ap, 2, 21, "0100", S3_S_ALERT_RECORD);
}

#[test]
fn simple_1rtt_certificate_verify() {
    // CertificateVerify の署名対象の組み立てを、トレースの RSA-PSS 署名で確かめる。
    // （トレースの鍵は 1024 ビットで、本番の下限 2048 を満たさないので rsa crate を直接使う）
    use rsa::pkcs8::DecodePublicKey;
    use rsa::signature::Verifier;
    let cert = msgs::parse_certificate(&hex(S3_CERT)).unwrap();
    assert!(cert.context.is_empty());
    let der = pki_types::CertificateDer::from(cert.certs[0].clone());
    let ee = webpki::EndEntityCert::try_from(&der).unwrap();
    let pk = rsa::RsaPublicKey::from_public_key_der(ee.subject_public_key_info().as_ref()).unwrap();
    let (scheme, sig) = msgs::parse_certificate_verify(&hex(S3_CV)).unwrap();
    assert_eq!(scheme, crate::cert::scheme::RSA_PSS_RSAE_SHA256);
    let t = transcript(&[S3_CH, S3_SH, S3_EE, S3_CERT]);
    let m = msgs::verify_message(true, &t.hash().unwrap());
    let vk = rsa::pss::VerifyingKey::<sha2::Sha256>::new(pk);
    vk.verify(&m, &rsa::pss::Signature::try_from(&sig[..]).unwrap()).unwrap();
    // 一ビット変えると通らない
    let mut m2 = m.clone();
    m2[70] ^= 1;
    assert!(vk.verify(&m2, &rsa::pss::Signature::try_from(&sig[..]).unwrap()).is_err());
}

#[test]
fn resumed_handshake() {
    let psk = sec(S3_RESUMPTION_PSK);
    let ks = KeySchedule::early(H, Some(&psk));
    assert_eq!(ks.current().as_bytes(), hex(S4_EARLY));

    // binder（RFC 8446 4.2.11.2）
    let ch = hex(S4_CH);
    let parsed = msgs::parse_client_hello(&ch).unwrap();
    let offered = parsed.psk.as_ref().unwrap();
    assert_eq!(&ch[..offered.truncated_len], &hex(S4_CH_PREFIX)[..]);
    let bh = H.hash(&ch[..offered.truncated_len]);
    assert_eq!(bh, hex(S4_BINDER_HASH));
    let binder = finished_mac(H, &ks.resumption_binder_key().unwrap(), &bh).unwrap();
    assert_eq!(binder, hex(S4_BINDER));
    assert_eq!(offered.binders[0], binder);
    assert_eq!(parsed.psk_modes.as_deref(), Some(&[1u8][..]));
    assert!(parsed.early_data);

    let sh = msgs::parse_server_hello(&hex(S4_SH)).unwrap();
    assert_eq!(sh.psk_identity, Some(0));

    let shared = x25519(S4_CLIENT_PRIV).agree(&hex(S4_SERVER_PUB)).unwrap();
    assert_eq!(shared.as_bytes(), hex(S4_ECDHE));
    let mut ks = ks;
    ks.advance(Some(&shared)).unwrap();
    assert_eq!(ks.current().as_bytes(), hex(S4_HS_SECRET));
    let mut t = transcript(&[S4_CH, S4_SH]);
    let th = t.hash().unwrap();
    let c_hs = ks.derive(b"c hs traffic", &th).unwrap();
    let s_hs = ks.derive(b"s hs traffic", &th).unwrap();
    assert_eq!(c_hs.as_bytes(), hex(S4_C_HS));
    assert_eq!(s_hs.as_bytes(), hex(S4_S_HS));
    t.add(&hex(S4_EE));
    assert_eq!(finished_mac(H, &s_hs, &t.hash().unwrap()).unwrap(), hex(S4_S_FINISHED));
    t.add(&hex(S4_S_FIN_MSG));
    ks.advance(None).unwrap();
    assert_eq!(ks.current().as_bytes(), hex(S4_MASTER));
    let th = t.hash().unwrap();
    assert_eq!(ks.derive(b"c ap traffic", &th).unwrap().as_bytes(), hex(S4_C_AP));
    assert_eq!(ks.derive(b"s ap traffic", &th).unwrap().as_bytes(), hex(S4_S_AP));
    // トレースは 0-RTT を使うので EndOfEarlyData が transcript に入る。
    t.add(&hex(S4_EOED));
    assert_eq!(finished_mac(H, &c_hs, &t.hash().unwrap()).unwrap(), hex(S4_C_FINISHED));
    t.add(&hex(S4_C_FIN_MSG));
    assert_eq!(ks.derive(b"res master", &t.hash().unwrap()).unwrap().as_bytes(), hex(S4_RES_MASTER));
}

#[test]
fn hello_retry_request() {
    let hrr = msgs::parse_server_hello(&hex(S5_HRR)).unwrap();
    assert!(hrr.is_hrr);
    assert_eq!(hrr.key_share.as_ref().unwrap().0, 0x0017);
    assert!(hrr.cookie.is_some());
    let ch2 = msgs::parse_client_hello(&hex(S5_CH2)).unwrap();
    assert_eq!(ch2.cookie, hrr.cookie);
    assert_eq!(ch2.key_shares.as_ref().unwrap()[0].0, 0x0017);

    // secp256r1 の ECDHE
    let k = p256::SecretKey::from_slice(&hex(S5_CLIENT_PRIV)).unwrap();
    let shared = KeyExchange::P256(k).agree(&hex(S5_SERVER_PUB)).unwrap();
    assert_eq!(shared.as_bytes(), hex(S5_ECDHE));

    // transcript: message_hash(CH1) || HRR || CH2 || SH
    let mut t = Transcript::new();
    t.add(&hex(S5_CH1));
    t.restart_with_message_hash(H).unwrap();
    for m in [S5_HRR, S5_CH2, S5_SH] {
        t.add(&hex(m));
    }
    let mut ks = KeySchedule::early(H, None);
    ks.advance(Some(&shared)).unwrap();
    assert_eq!(ks.current().as_bytes(), hex(S5_HS_SECRET));
    let th = t.hash().unwrap();
    let c_hs = ks.derive(b"c hs traffic", &th).unwrap();
    let s_hs = ks.derive(b"s hs traffic", &th).unwrap();
    assert_eq!(c_hs.as_bytes(), hex(S5_C_HS));
    assert_eq!(s_hs.as_bytes(), hex(S5_S_HS));
    for m in [S5_EE, S5_CERT, S5_CV] {
        t.add(&hex(m));
    }
    assert_eq!(finished_mac(H, &s_hs, &t.hash().unwrap()).unwrap(), hex(S5_S_FINISHED));
    t.add(&hex(S5_S_FIN_MSG));
    check_record(CipherSuite::Aes128GcmSha256, &s_hs, 0, 22, S5_S_FLIGHT, S5_S_FLIGHT_RECORD);
    ks.advance(None).unwrap();
    assert_eq!(ks.current().as_bytes(), hex(S5_MASTER));
    let th = t.hash().unwrap();
    assert_eq!(ks.derive(b"c ap traffic", &th).unwrap().as_bytes(), hex(S5_C_AP));
    assert_eq!(ks.derive(b"s ap traffic", &th).unwrap().as_bytes(), hex(S5_S_AP));
    assert_eq!(finished_mac(H, &c_hs, &th).unwrap(), hex(S5_C_FINISHED));
    t.add(&hex(S5_C_FIN_MSG));
    assert_eq!(ks.derive(b"res master", &t.hash().unwrap()).unwrap().as_bytes(), hex(S5_RES_MASTER));
}

#[test]
fn client_auth_messages_parse() {
    let cr = msgs::parse_certificate_request(&hex(S6_CR)).unwrap();
    assert!(cr.context.is_empty());
    assert!(cr.signature_algorithms.contains(&crate::cert::scheme::RSA_PSS_RSAE_SHA256));
    let c = msgs::parse_certificate(&hex(S6_C_CERT)).unwrap();
    assert_eq!(c.certs.len(), 1);
    let (scheme, _) = msgs::parse_certificate_verify(&hex(S6_C_CV)).unwrap();
    assert_eq!(scheme, crate::cert::scheme::RSA_PSS_RSAE_SHA256);
}

#[test]
fn compatibility_mode() {
    let ch = msgs::parse_client_hello(&hex(S7_CH)).unwrap();
    assert_eq!(ch.session_id.len(), 32);
    let sh = msgs::parse_server_hello(&hex(S7_SH)).unwrap();
    // 中継機器互換: session_id をそのまま返し、CCS を平文で送る。
    assert_eq!(sh.session_id, ch.session_id);
    assert_eq!(hex(S7_CCS_RECORD), vec![20, 3, 3, 0, 1, 1]);
    check_record(CipherSuite::Aes128GcmSha256, &sec(S7_S_HS), 0, 22, S7_S_FLIGHT, S7_S_FLIGHT_RECORD);
}

#[test]
fn chacha20_poly1305_aead_rfc9001() {
    // RFC 9001 付録 A.5: ChaCha20-Poly1305 の nonce（iv XOR 連番）と暗号文。
    let secret = sec("9ac312a7f877468ebe69422748ad00a15443f18203a07d6060f688f30f21632b");
    let k = crate::quic::packet_keys(CipherSuite::Chacha20Poly1305Sha256, &secret).unwrap();
    let aead = crate::crypto::Aead::new(CipherSuite::Chacha20Poly1305Sha256, k.key.as_bytes()).unwrap();
    let mut nonce: [u8; 12] = k.iv.as_bytes().try_into().unwrap();
    for (a, b) in nonce[4..].iter_mut().zip(654360564u64.to_be_bytes()) {
        *a ^= b;
    }
    assert_eq!(nonce.to_vec(), hex("e0459b3474bdd0e46d417eb0"));
    let mut buf = vec![1u8];
    aead.seal(&nonce, &hex("4200bff4"), &mut buf).unwrap();
    assert_eq!(buf, hex("655e5cd55c41f69080575d7999c25a5bfb"));
    aead.open(&nonce, &hex("4200bff4"), &mut buf).unwrap();
    assert_eq!(buf, vec![1u8]);
    let mut bad = hex("655e5cd55c41f69080575d7999c25a5bfa");
    assert!(aead.open(&nonce, &hex("4200bff4"), &mut bad).is_err());
}

#[test]
fn quic_initial_secrets_rfc9001() {
    // RFC 9001 付録 A.1
    let (c, s) = crate::quic::initial_secrets(&hex("8394c8f03e515708")).unwrap();
    assert_eq!(
        c.as_bytes(),
        hex("c00cf151ca5be075ed0ebfb5c80323c42d6b7db67881289af4008f1f6c357aea")
    );
    assert_eq!(
        s.as_bytes(),
        hex("3c199828fd139efd216c155ad844cc81fb82fa8d7446fa7d78be803acdda951b")
    );
    let k = crate::quic::packet_keys(CipherSuite::Aes128GcmSha256, &c).unwrap();
    assert_eq!(k.key.as_bytes(), hex("1f369613dd76d5467730efcbe3b1a22d"));
    assert_eq!(k.iv.as_bytes(), hex("fa044b2f42a3fd3b46fb255c"));
    assert_eq!(k.hp.as_bytes(), hex("9f50449e04a0e810283a1e9933adedd2"));
    // RFC 9001 付録 A.5（ChaCha20-Poly1305 と鍵更新）
    let secret = sec("9ac312a7f877468ebe69422748ad00a15443f18203a07d6060f688f30f21632b");
    let k = crate::quic::packet_keys(CipherSuite::Chacha20Poly1305Sha256, &secret).unwrap();
    assert_eq!(
        k.key.as_bytes(),
        hex("c6d98ff3441c3fe1b2182094f69caa2ed4b716b65488960a7a984979fb23e1c8")
    );
    assert_eq!(k.iv.as_bytes(), hex("e0459b3474bdd0e44a41c144"));
    assert_eq!(
        k.hp.as_bytes(),
        hex("25a282b9e82f06f21f488917a4fc8f1b73573685608597d0efcb076b0ab7a7a4")
    );
    let ku = crate::quic::next_secret(CipherSuite::Chacha20Poly1305Sha256, &secret).unwrap();
    assert_eq!(
        ku.as_bytes(),
        hex("1223504755036d556342ee9361d253421a826c9ecdf3c7148684b36b714881f9")
    );
}
