//! STUN のメッセージの解析（RFC 8489）。属性の取り出しと MESSAGE-INTEGRITY の検証まで。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_stun::Message;

fuzz_target!(|data: &[u8]| {
    let _ = senca_stun::message::is_stun(data);
    if let Ok(m) = Message::parse(Bytes::copy_from_slice(data)) {
        let _ = (m.username(), m.realm(), m.nonce(), m.software(), m.error_code());
        let _ = (m.mapped_address(), m.priority(), m.use_candidate(), m.lifetime());
        let _ = (m.ice_controlling(), m.ice_controlled(), m.unknown_comprehension_required());
        if m.has_integrity() {
            let _ = m.verify_integrity(&senca_stun::short_term_key("password"));
        }
    }
});
