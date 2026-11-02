//! server のハンドシェイク状態機械（RFC 8446 2 章・4 章）。

use std::sync::Arc;
use std::time::SystemTime;

use pki_types::CertificateDer;
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::cert::{self, CertifiedKey, ClientAuth, VERIFY_SCHEMES};
use crate::codec::{Reader, len_prefixed, put_u16, put_vec8, put_vec16};
use crate::config::ServerConfig;
use crate::crypto::{CipherSuite, KeyExchange, NamedGroup, Secret, random_array};
use crate::engine::{Action, Common, Handshaker, Level, Transcript, expect_level};
use crate::error::{AlertDescription, Error};
use crate::key_schedule::{KeySchedule, check_finished, finished_mac, resumption_psk};
use crate::msgs::{self, ext, hs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    WaitClientHello,
    WaitSecondClientHello,
    WaitCert,
    WaitCertVerify,
    WaitFinished,
    Connected,
}

/// HRR を送ったときの最初の ClientHello の要点。
struct FirstHello {
    random: [u8; 32],
    session_id: Vec<u8>,
    suite: CipherSuite,
    group: NamedGroup,
}

/// チケットの中身（暗号化して client に渡す）。
struct TicketState {
    suite: CipherSuite,
    issued_ms: u64,
    lifetime_secs: u32,
    psk: Secret,
    sni: Option<String>,
    quic: bool,
}

const TICKET_VERSION: u8 = 1;

impl TicketState {
    fn encode(&self) -> Result<Zeroizing<Vec<u8>>, Error> {
        let mut v = Zeroizing::new(Vec::with_capacity(128));
        v.push(TICKET_VERSION);
        put_u16(&mut v, self.suite.to_u16());
        v.extend_from_slice(&self.issued_ms.to_be_bytes());
        v.extend_from_slice(&self.lifetime_secs.to_be_bytes());
        put_vec8(&mut v, self.psk.as_bytes())?;
        put_vec8(&mut v, self.sni.as_deref().unwrap_or("").as_bytes())?;
        v.push(u8::from(self.quic));
        Ok(v)
    }

    fn decode(b: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(b);
        if r.u8()? != TICKET_VERSION {
            return Err(Error::decode("チケットの版が違う"));
        }
        let suite = CipherSuite::from_u16(r.u16()?).ok_or(Error::decode("チケットのスイート"))?;
        let issued_ms = r.u64()?;
        let lifetime_secs = r.u32()?;
        let psk = Secret::new(r.vec8()?.to_vec());
        let sni = r.vec8()?;
        let sni = if sni.is_empty() {
            None
        } else {
            Some(String::from_utf8(sni.to_vec()).map_err(|_| Error::decode("チケットの SNI"))?)
        };
        let quic = r.u8()? == 1;
        r.finish()?;
        Ok(Self {
            suite,
            issued_ms,
            lifetime_secs,
            psk,
            sni,
            quic,
        })
    }
}

pub(crate) struct ServerHs {
    config: Arc<ServerConfig>,
    c: Common,
    state: State,
    transcript: Transcript,
    first: Option<FirstHello>,
    ks: Option<KeySchedule>,
    c_hs: Option<Secret>,
    c_ap: Option<Secret>,
    sent_ccs: bool,
    rejected_early_data: bool,
    client_psk_dhe: bool,
    cert_requested: bool,
}

impl ServerHs {
    pub(crate) fn new(config: Arc<ServerConfig>, quic: Option<Vec<u8>>, now: SystemTime) -> Result<Self, Error> {
        if config.cipher_suites.is_empty() || config.groups.is_empty() {
            return Err(Error::Config("暗号スイートか group が空".into()));
        }
        if quic.is_some() && config.alpn_protocols.is_empty() {
            return Err(Error::Config("QUIC では ALPN が必須".into()));
        }
        if config.ticket_lifetime_secs > 604_800 {
            return Err(Error::Config("チケットの寿命は 7 日以下".into()));
        }
        Ok(Self {
            config,
            c: Common::new(false, quic, now),
            state: State::WaitClientHello,
            transcript: Transcript::new(),
            first: None,
            ks: None,
            c_hs: None,
            c_ap: None,
            sent_ccs: false,
            rejected_early_data: false,
            client_psk_dhe: false,
            cert_requested: false,
        })
    }

    fn handle_client_hello(&mut self, msg: &[u8], out: &mut Vec<Action>) -> Result<(), Error> {
        let ch = msgs::parse_client_hello(msg)?;
        let second = self.state == State::WaitSecondClientHello;
        if !ch.supported_versions.as_ref().is_some_and(|v| v.contains(&msgs::TLS13)) {
            return Err(Error::fatal(AlertDescription::PROTOCOL_VERSION, "client が TLS 1.3 に対応していない"));
        }
        if self.c.quic.is_some() {
            if ch.quic_transport_parameters.is_none() {
                return Err(Error::fatal(
                    AlertDescription::MISSING_EXTENSION,
                    "quic_transport_parameters がない",
                ));
            }
            if !ch.session_id.is_empty() {
                return Err(Error::illegal("QUIC で legacy_session_id が空でない"));
            }
        }
        // スイート: server の優先順。
        let suite = self
            .config
            .cipher_suites
            .iter()
            .copied()
            .find(|s| ch.cipher_suites.contains(&s.to_u16()))
            .ok_or(Error::fatal(AlertDescription::HANDSHAKE_FAILURE, "共通の暗号スイートがない"))?;
        // group と key_share。
        let groups = ch.supported_groups.as_ref().ok_or(Error::fatal(
            AlertDescription::MISSING_EXTENSION,
            "supported_groups がない",
        ))?;
        let shares = ch
            .key_shares
            .as_ref()
            .ok_or(Error::fatal(AlertDescription::MISSING_EXTENSION, "key_share がない"))?;
        if shares.iter().any(|(g, _)| !groups.contains(g)) {
            return Err(Error::illegal("supported_groups にない group の key_share"));
        }
        let chosen = self
            .config
            .groups
            .iter()
            .copied()
            .find_map(|g| shares.iter().find(|(x, _)| *x == g.to_u16()).map(|(_, k)| (g, k.clone())));

        if second {
            let first = self.first.as_ref().ok_or(Error::internal("最初の ClientHello がない"))?;
            if ch.random != first.random || ch.session_id != first.session_id {
                return Err(Error::illegal("二つ目の ClientHello が一つ目と食い違う"));
            }
            if suite != first.suite {
                return Err(Error::illegal("二つ目の ClientHello でスイートが変わった"));
            }
            if shares.len() != 1 || shares[0].0 != first.group.to_u16() {
                return Err(Error::illegal("二つ目の ClientHello の key_share が HRR と違う"));
            }
            if ch.early_data {
                return Err(Error::illegal("二つ目の ClientHello に early_data"));
            }
            if ch.cookie.is_some() {
                return Err(Error::illegal("送っていない cookie"));
            }
        } else if ch.cookie.is_some() {
            return Err(Error::illegal("送っていない cookie"));
        }

        let Some((group, client_share)) = chosen else {
            if second {
                return Err(Error::illegal("二つ目の ClientHello に使える key_share がない"));
            }
            // HelloRetryRequest（RFC 8446 4.1.4）。
            let group = self
                .config
                .groups
                .iter()
                .copied()
                .find(|g| groups.contains(&g.to_u16()))
                .ok_or(Error::fatal(AlertDescription::HANDSHAKE_FAILURE, "共通の group がない"))?;
            self.transcript.set_hash(suite.hash())?;
            self.transcript.add(msg);
            self.transcript.restart_with_message_hash(suite.hash())?;
            let hrr = self.build_server_hello(&msgs::HRR_RANDOM, &ch.session_id, suite, |o| {
                msgs::put_ext(o, ext::KEY_SHARE, |o| {
                    put_u16(o, group.to_u16());
                    Ok(())
                })
            })?;
            self.transcript.add(&hrr);
            out.push(Action::Message {
                level: Level::Initial,
                data: hrr,
            });
            if !ch.session_id.is_empty() && self.c.quic.is_none() {
                out.push(Action::Ccs);
                self.sent_ccs = true;
            }
            self.rejected_early_data = ch.early_data && self.c.quic.is_none();
            self.c.hrr = true;
            self.first = Some(FirstHello {
                random: ch.random,
                session_id: ch.session_id.clone(),
                suite,
                group,
            });
            self.state = State::WaitSecondClientHello;
            return Ok(());
        };

        self.c.suite = Some(suite);
        self.c.group = Some(group);
        self.c.sni = ch.server_name.clone();
        let h = suite.hash();
        self.transcript.set_hash(h)?;

        // ALPN（RFC 7301）。
        if let Some(offered) = &ch.alpn
            && !self.config.alpn_protocols.is_empty()
        {
            let p = self
                .config
                .alpn_protocols
                .iter()
                .find(|p| offered.contains(p))
                .ok_or(Error::fatal(AlertDescription::NO_APPLICATION_PROTOCOL, "共通の ALPN がない"))?;
            self.c.alpn = Some(p.clone());
        }
        if self.c.quic.is_some() {
            if self.c.alpn.is_none() {
                return Err(Error::fatal(AlertDescription::NO_APPLICATION_PROTOCOL, "QUIC で ALPN が決まらない"));
            }
            self.c.peer_transport_parameters = ch.quic_transport_parameters.clone();
        }
        let send_rsl = match (ch.record_size_limit, &self.c.quic) {
            (Some(v), None) => {
                self.c.peer_record_limit = Some(v);
                let own = self.config.record_size_limit.unwrap_or(16385).clamp(64, 16385);
                self.c.own_record_limit = Some(own);
                Some(own)
            }
            _ => None,
        };

        // PSK（psk_dhe_ke だけ）。
        let mut psk_choice: Option<(u16, Secret)> = None;
        if let Some(offered) = &ch.psk {
            let modes = ch.psk_modes.as_ref().ok_or(Error::fatal(
                AlertDescription::MISSING_EXTENSION,
                "pre_shared_key に psk_key_exchange_modes が伴わない",
            ))?;
            if modes.contains(&msgs::PSK_DHE_KE)
                && !matches!(self.config.client_auth, ClientAuth::Required(_))
                && let Some(keys) = &self.config.ticket_keys
            {
                for (i, (id, _age)) in offered.identities.iter().enumerate().take(8) {
                    let Some(plain) = keys.open(self.c.now_ms, id) else { continue };
                    let Ok(t) = TicketState::decode(&plain) else { continue };
                    let expired = self.c.now_ms < t.issued_ms
                        || self.c.now_ms - t.issued_ms > u64::from(t.lifetime_secs) * 1000;
                    if t.suite.hash() != h || expired || t.sni != self.c.sni || t.quic != self.c.quic.is_some() {
                        continue;
                    }
                    // binder を検証する（RFC 8446 4.2.11.2）。失敗は接続を終える。
                    let th = self.transcript.hash_with(&msg[..offered.truncated_len])?;
                    let ks = KeySchedule::early(h, Some(&t.psk));
                    let expected = finished_mac(h, &ks.resumption_binder_key()?, &th)?;
                    let got = &offered.binders[i];
                    if expected.len() != got.len() || !bool::from(expected.ct_eq(got)) {
                        return Err(Error::fatal(AlertDescription::DECRYPT_ERROR, "PSK binder が一致しない"));
                    }
                    psk_choice = Some((i as u16, t.psk));
                    break;
                }
            }
        }
        if let Some(modes) = &ch.psk_modes {
            self.client_psk_dhe = modes.contains(&msgs::PSK_DHE_KE);
        }
        self.transcript.add(msg);

        // 証明書（PSK で再開しないとき）。
        let cert_and_scheme: Option<(Arc<CertifiedKey>, u16)> = if psk_choice.is_none() {
            let sa = ch.signature_algorithms.as_ref().ok_or(Error::fatal(
                AlertDescription::MISSING_EXTENSION,
                "signature_algorithms がない",
            ))?;
            let ck = self.config.resolve_cert(self.c.sni.as_deref()).ok_or(Error::fatal(
                AlertDescription::UNRECOGNIZED_NAME,
                "SNI に合う証明書がない",
            ))?;
            let scheme = ck.key.choose_scheme(sa).ok_or(Error::fatal(
                AlertDescription::HANDSHAKE_FAILURE,
                "共通の署名方式がない",
            ))?;
            Some((ck, scheme))
        } else {
            None
        };

        // 鍵交換と ServerHello。
        let (kx, public) = KeyExchange::generate(group)?;
        let shared = kx.agree(&client_share)?;
        drop(kx);
        let random: [u8; 32] = random_array()?;
        let psk_idx = psk_choice.as_ref().map(|(i, _)| *i);
        let sh = self.build_server_hello(&random, &ch.session_id, suite, |o| {
            msgs::put_ext(o, ext::KEY_SHARE, |o| {
                put_u16(o, group.to_u16());
                put_vec16(o, &public)
            })?;
            if let Some(i) = psk_idx {
                msgs::put_ext(o, ext::PRE_SHARED_KEY, |o| {
                    put_u16(o, i);
                    Ok(())
                })?;
            }
            Ok(())
        })?;
        self.transcript.add(&sh);
        out.push(Action::Message {
            level: Level::Initial,
            data: sh,
        });
        if !ch.session_id.is_empty() && self.c.quic.is_none() && !self.sent_ccs {
            out.push(Action::Ccs);
            self.sent_ccs = true;
        }
        self.c.resumed = psk_choice.is_some();
        // 0-RTT は使わない（仕様 6.4）。TCP では client の early data を読み飛ばす。
        if ch.early_data && self.c.quic.is_none() {
            self.rejected_early_data = true;
        }

        let mut ks = KeySchedule::early(h, psk_choice.as_ref().map(|(_, p)| p));
        ks.advance(Some(&shared))?;
        let th = self.transcript.hash()?;
        let c_hs = ks.derive(b"c hs traffic", &th)?;
        let s_hs = ks.derive(b"s hs traffic", &th)?;
        out.push(Action::WriteSecret {
            level: Level::Handshake,
            secret: s_hs.clone(),
        });
        out.push(Action::ReadSecret {
            level: Level::Handshake,
            secret: c_hs.clone(),
        });

        // EncryptedExtensions。
        let mut ee = Vec::new();
        len_prefixed(&mut ee, 2, |o| {
            if ch.server_name.is_some() && psk_choice.is_none() {
                msgs::put_ext(o, ext::SERVER_NAME, |_| Ok(()))?;
            }
            if let Some(p) = &self.c.alpn {
                msgs::put_ext(o, ext::ALPN, |o| msgs::put_alpn(o, std::slice::from_ref(p)))?;
            }
            if let Some(l) = send_rsl {
                msgs::put_ext(o, ext::RECORD_SIZE_LIMIT, |o| {
                    put_u16(o, l);
                    Ok(())
                })?;
            }
            if let Some(tp) = &self.c.quic {
                msgs::put_ext(o, ext::QUIC_TRANSPORT_PARAMETERS, |o| {
                    o.extend_from_slice(tp);
                    Ok(())
                })?;
            }
            Ok(())
        })?;
        self.send(out, Level::Handshake, msgs::handshake(hs::ENCRYPTED_EXTENSIONS, &ee)?);

        if let Some((ck, scheme)) = cert_and_scheme {
            if !matches!(self.config.client_auth, ClientAuth::None) {
                self.send(out, Level::Handshake, msgs::encode_certificate_request(&VERIFY_SCHEMES)?);
                self.cert_requested = true;
            }
            let certs: Vec<&[u8]> = ck.chain.iter().map(|c| c.as_ref()).collect();
            self.send(out, Level::Handshake, msgs::encode_certificate(&[], &certs)?);
            let sig = ck.key.sign(scheme, &msgs::verify_message(true, &self.transcript.hash()?))?;
            self.send(out, Level::Handshake, msgs::encode_certificate_verify(scheme, &sig)?);
        }
        let fin = msgs::handshake(hs::FINISHED, &finished_mac(h, &s_hs, &self.transcript.hash()?)?)?;
        self.send(out, Level::Handshake, fin);

        ks.advance(None)?;
        let th = self.transcript.hash()?;
        let c_ap = ks.derive(b"c ap traffic", &th)?;
        let s_ap = ks.derive(b"s ap traffic", &th)?;
        self.c.exporter = Some(ks.derive(b"exp master", &th)?);
        out.push(Action::WriteSecret {
            level: Level::OneRtt,
            secret: s_ap.clone(),
        });
        self.c.server_app = Some(s_ap);
        self.c_ap = Some(c_ap);
        self.c_hs = Some(c_hs);
        self.ks = Some(ks);
        self.state = if self.cert_requested {
            State::WaitCert
        } else {
            State::WaitFinished
        };
        Ok(())
    }

    fn send(&mut self, out: &mut Vec<Action>, level: Level, m: Vec<u8>) {
        self.transcript.add(&m);
        out.push(Action::Message { level, data: m });
    }

    fn build_server_hello(
        &self,
        random: &[u8; 32],
        session_id: &[u8],
        suite: CipherSuite,
        exts: impl FnOnce(&mut Vec<u8>) -> Result<(), Error>,
    ) -> Result<Vec<u8>, Error> {
        let mut b = Vec::with_capacity(128);
        put_u16(&mut b, msgs::LEGACY_VERSION);
        b.extend_from_slice(random);
        put_vec8(&mut b, session_id)?;
        put_u16(&mut b, suite.to_u16());
        b.push(0);
        len_prefixed(&mut b, 2, |o| {
            msgs::put_ext(o, ext::SUPPORTED_VERSIONS, |o| {
                put_u16(o, msgs::TLS13);
                Ok(())
            })?;
            exts(o)
        })?;
        msgs::handshake(hs::SERVER_HELLO, &b)
    }

    fn handle_certificate(&mut self, msg: &[u8]) -> Result<(), Error> {
        let cm = msgs::parse_certificate(msg)?;
        if !cm.context.is_empty() {
            return Err(Error::illegal("certificate_request_context が一致しない"));
        }
        if cm.certs.is_empty() {
            if matches!(self.config.client_auth, ClientAuth::Required(_)) {
                return Err(Error::fatal(
                    AlertDescription::CERTIFICATE_REQUIRED,
                    "クライアント証明書が必要",
                ));
            }
            self.transcript.add(msg);
            self.state = State::WaitFinished;
            return Ok(());
        }
        let roots = match &self.config.client_auth {
            ClientAuth::Optional(r) | ClientAuth::Required(r) => r.clone(),
            ClientAuth::None => return Err(Error::unexpected("要求していないクライアント証明書")),
        };
        let chain: Vec<CertificateDer<'static>> = cm.certs.into_iter().map(CertificateDer::from).collect();
        cert::verify_client(&roots, &chain, self.c.now)?;
        self.c.peer_certs = chain;
        self.transcript.add(msg);
        self.state = State::WaitCertVerify;
        Ok(())
    }

    fn handle_certificate_verify(&mut self, msg: &[u8]) -> Result<(), Error> {
        let (scheme, sig) = msgs::parse_certificate_verify(msg)?;
        if !VERIFY_SCHEMES.contains(&scheme) {
            return Err(Error::illegal("CertificateVerify の署名方式が提示外"));
        }
        let th = self.transcript.hash()?;
        let leaf = self.c.peer_certs.first().ok_or(Error::internal("証明書がない"))?;
        cert::verify_signature(leaf, scheme, &msgs::verify_message(false, &th), &sig)?;
        self.transcript.add(msg);
        self.state = State::WaitFinished;
        Ok(())
    }

    fn handle_finished(&mut self, msg: &[u8], out: &mut Vec<Action>) -> Result<(), Error> {
        let h = self.c.hash()?;
        let c_hs = self.c_hs.take().ok_or(Error::internal("鍵がない"))?;
        check_finished(h, &c_hs, &self.transcript.hash()?, &msg[4..])?;
        self.transcript.add(msg);
        let ks = self.ks.take().ok_or(Error::internal("鍵スケジュールがない"))?;
        let res_master = ks.derive(b"res master", &self.transcript.hash()?)?;
        let c_ap = self.c_ap.take().ok_or(Error::internal("鍵がない"))?;
        out.push(Action::ReadSecret {
            level: Level::OneRtt,
            secret: c_ap.clone(),
        });
        self.c.client_app = Some(c_ap);
        self.c.complete = true;
        self.state = State::Connected;
        out.push(Action::Complete);
        self.issue_tickets(&res_master, out)
    }

    /// NewSessionTicket を送る。クライアント証明書で認証した接続には出さない
    /// （チケットに相手の身元を入れていないため）。
    fn issue_tickets(&mut self, res_master: &Secret, out: &mut Vec<Action>) -> Result<(), Error> {
        let Some(keys) = &self.config.ticket_keys else { return Ok(()) };
        if !self.client_psk_dhe
            || !self.c.peer_certs.is_empty()
            || matches!(self.config.client_auth, ClientAuth::Required(_))
            || self.config.ticket_lifetime_secs == 0
        {
            return Ok(());
        }
        let suite = self.c.suite.ok_or(Error::internal("スイートが未定"))?;
        let lifetime = self
            .config
            .ticket_lifetime_secs
            .min((keys.rotation_ms() / 1000).min(u64::from(u32::MAX)) as u32);
        for i in 0..self.config.tickets_per_handshake.min(8) {
            let nonce = vec![i];
            let st = TicketState {
                suite,
                issued_ms: self.c.now_ms,
                lifetime_secs: lifetime,
                psk: resumption_psk(suite.hash(), res_master, &nonce)?,
                sni: self.c.sni.clone(),
                quic: self.c.quic.is_some(),
            };
            let blob = keys.seal(self.c.now_ms, &st.encode()?)?;
            let m = msgs::encode_new_session_ticket(&msgs::NewSessionTicket {
                lifetime,
                age_add: u32::from_be_bytes(random_array()?),
                nonce,
                ticket: blob,
            })?;
            out.push(Action::Message {
                level: Level::OneRtt,
                data: m,
            });
        }
        Ok(())
    }
}

impl Handshaker for ServerHs {
    fn start(&mut self, _out: &mut Vec<Action>) -> Result<(), Error> {
        Ok(())
    }

    fn handle(&mut self, level: Level, msg: &[u8], out: &mut Vec<Action>) -> Result<(), Error> {
        let typ = msg[0];
        match (self.state, typ) {
            (State::WaitClientHello | State::WaitSecondClientHello, hs::CLIENT_HELLO) => {
                expect_level(level, Level::Initial)?;
                self.handle_client_hello(msg, out)
            }
            (State::WaitCert, hs::CERTIFICATE) => {
                expect_level(level, Level::Handshake)?;
                self.handle_certificate(msg)
            }
            (State::WaitCertVerify, hs::CERTIFICATE_VERIFY) => {
                expect_level(level, Level::Handshake)?;
                self.handle_certificate_verify(msg)
            }
            (State::WaitFinished, hs::FINISHED) => {
                expect_level(level, Level::Handshake)?;
                self.rejected_early_data = false;
                self.handle_finished(msg, out)
            }
            (State::Connected, hs::KEY_UPDATE) => {
                expect_level(level, Level::OneRtt)?;
                self.c.recv_key_update(msg, out)
            }
            _ => Err(Error::unexpected("想定外のハンドシェイクメッセージ")),
        }
    }

    fn common(&self) -> &Common {
        &self.c
    }

    fn common_mut(&mut self) -> &mut Common {
        &mut self.c
    }

    fn accepts_ccs(&self) -> bool {
        // 最初の ClientHello を受け取った後、client の Finished を受け取るまで。
        !matches!(self.state, State::WaitClientHello | State::Connected)
    }

    fn rejected_early_data(&self) -> bool {
        self.rejected_early_data
    }
}
