//! client のハンドシェイク状態機械（RFC 8446 2 章・4 章）。

use std::sync::Arc;
use std::time::SystemTime;

use pki_types::{CertificateDer, ServerName};

use crate::cert::{self, OFFERED_SCHEMES, VERIFY_SCHEMES};
use crate::codec::{len_prefixed, put_u16, put_u32, put_vec8, put_vec16};
use crate::config::{ClientConfig, StoredTicket};
use crate::crypto::{CipherSuite, KeyExchange, NamedGroup, Secret, random_array};
use crate::engine::{Action, Common, Handshaker, Level, Transcript, expect_level};
use crate::error::{AlertDescription, Error};
use crate::key_schedule::{KeySchedule, check_finished, finished_mac, resumption_psk};
use crate::msgs::{self, ext, hs};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Start,
    WaitServerHello,
    WaitEncryptedExtensions,
    WaitCertOrCertRequest,
    WaitCert,
    WaitCertVerify,
    WaitFinished,
    Connected,
}

pub(crate) struct ClientHs {
    config: Arc<ClientConfig>,
    c: Common,
    server_name: ServerName<'static>,
    store_key: String,
    state: State,
    random: [u8; 32],
    session_id: Vec<u8>,
    shares: Vec<(NamedGroup, KeyExchange, Vec<u8>)>,
    transcript: Transcript,
    ticket: Option<StoredTicket>,
    hrr_suite: Option<CipherSuite>,
    hrr_group: Option<NamedGroup>,
    cookie: Option<Vec<u8>>,
    ks: Option<KeySchedule>,
    c_hs: Option<Secret>,
    s_hs: Option<Secret>,
    cert_request: Option<msgs::CertificateRequest>,
    sent_ccs: bool,
    res_master: Option<Secret>,
}

impl ClientHs {
    pub(crate) fn new(
        config: Arc<ClientConfig>,
        server_name: &str,
        quic: Option<Vec<u8>>,
        now: SystemTime,
    ) -> Result<Self, Error> {
        if config.cipher_suites.is_empty() || config.groups.is_empty() {
            return Err(Error::Config("暗号スイートか group が空".into()));
        }
        if quic.is_some() && config.alpn_protocols.is_empty() {
            return Err(Error::Config("QUIC では ALPN が必須".into()));
        }
        let name = server_name.trim_end_matches('.');
        let sn = ServerName::try_from(name.to_string())
            .map_err(|_| Error::Config(format!("サーバー名が不正: {server_name}")))?;
        let sni = match &sn {
            ServerName::DnsName(d) if config.enable_sni => Some(d.as_ref().to_ascii_lowercase()),
            _ => None,
        };
        let mut c = Common::new(true, quic, now);
        c.sni = sni;
        Ok(Self {
            store_key: name.to_ascii_lowercase(),
            config,
            c,
            server_name: sn,
            state: State::Start,
            random: [0; 32],
            session_id: Vec::new(),
            shares: Vec::new(),
            transcript: Transcript::new(),
            ticket: None,
            hrr_suite: None,
            hrr_group: None,
            cookie: None,
            ks: None,
            c_hs: None,
            s_hs: None,
            cert_request: None,
            sent_ccs: false,
            res_master: None,
        })
    }

    /// ClientHello を作る。PSK があれば binder を計算して埋める。
    fn build_client_hello(&self) -> Result<Vec<u8>, Error> {
        let cfg = &self.config;
        let mut b = Vec::with_capacity(512);
        put_u16(&mut b, msgs::LEGACY_VERSION);
        b.extend_from_slice(&self.random);
        put_vec8(&mut b, &self.session_id)?;
        let suites: Vec<u16> = cfg.cipher_suites.iter().map(|s| s.to_u16()).collect();
        msgs::put_u16_list(&mut b, &suites)?;
        put_vec8(&mut b, &[0])?;
        let ticket = self.ticket.as_ref();
        let hash_len = ticket.map(|t| t.suite.hash().output_len()).unwrap_or(0);
        len_prefixed(&mut b, 2, |o| {
            if let Some(n) = &self.c.sni {
                msgs::put_ext(o, ext::SERVER_NAME, |o| msgs::put_server_name(o, n))?;
            }
            msgs::put_ext(o, ext::SUPPORTED_VERSIONS, |o| put_vec8(o, &msgs::TLS13.to_be_bytes()))?;
            let groups: Vec<u16> = cfg.groups.iter().map(|g| g.to_u16()).collect();
            msgs::put_ext(o, ext::SUPPORTED_GROUPS, |o| msgs::put_u16_list(o, &groups))?;
            msgs::put_ext(o, ext::SIGNATURE_ALGORITHMS, |o| msgs::put_u16_list(o, &OFFERED_SCHEMES))?;
            msgs::put_ext(o, ext::KEY_SHARE, |o| {
                len_prefixed(o, 2, |o| {
                    for (g, _, public) in &self.shares {
                        put_u16(o, g.to_u16());
                        put_vec16(o, public)?;
                    }
                    Ok(())
                })
            })?;
            if !cfg.alpn_protocols.is_empty() {
                msgs::put_ext(o, ext::ALPN, |o| msgs::put_alpn(o, &cfg.alpn_protocols))?;
            }
            if let (Some(l), None) = (cfg.record_size_limit, &self.c.quic) {
                msgs::put_ext(o, ext::RECORD_SIZE_LIMIT, |o| {
                    put_u16(o, l.clamp(64, 16385));
                    Ok(())
                })?;
            }
            if let Some(tp) = &self.c.quic {
                msgs::put_ext(o, ext::QUIC_TRANSPORT_PARAMETERS, |o| {
                    o.extend_from_slice(tp);
                    Ok(())
                })?;
            }
            if let Some(ck) = &self.cookie {
                msgs::put_ext(o, ext::COOKIE, |o| put_vec16(o, ck))?;
            }
            if cfg.session_store.is_some() {
                msgs::put_ext(o, ext::PSK_KEY_EXCHANGE_MODES, |o| put_vec8(o, &[msgs::PSK_DHE_KE]))?;
            }
            if let Some(t) = ticket {
                // pre_shared_key は最後（RFC 8446 4.2.11）。binder は後で埋める。
                msgs::put_ext(o, ext::PRE_SHARED_KEY, |o| {
                    len_prefixed(o, 2, |o| {
                        put_vec16(o, &t.ticket)?;
                        let age = self.c.now_ms.saturating_sub(t.received_ms) as u32;
                        put_u32(o, age.wrapping_add(t.age_add));
                        Ok(())
                    })?;
                    len_prefixed(o, 2, |o| put_vec8(o, &vec![0u8; hash_len]))
                })?;
            }
            Ok(())
        })?;
        let mut msg = msgs::handshake(hs::CLIENT_HELLO, &b)?;
        if let Some(t) = ticket {
            let truncated = msg.len() - (2 + 1 + hash_len);
            let h = t.suite.hash();
            let th = if self.transcript.has_hash() {
                self.transcript.hash_with(&msg[..truncated])?
            } else {
                h.hash(&msg[..truncated])
            };
            let ks = KeySchedule::early(h, Some(&t.psk));
            let binder = finished_mac(h, &ks.resumption_binder_key()?, &th)?;
            let at = msg.len() - hash_len;
            msg[at..].copy_from_slice(&binder);
        }
        Ok(msg)
    }

    fn send_client_hello(&mut self, out: &mut Vec<Action>) -> Result<(), Error> {
        let ch = self.build_client_hello()?;
        self.transcript.add(&ch);
        out.push(Action::Message {
            level: Level::Initial,
            data: ch,
        });
        Ok(())
    }

    fn handle_server_hello(&mut self, msg: &[u8], out: &mut Vec<Action>) -> Result<(), Error> {
        let sh = msgs::parse_server_hello(msg)?;
        if sh.session_id != self.session_id {
            return Err(Error::illegal("legacy_session_id_echo が一致しない"));
        }
        let suite = CipherSuite::from_u16(sh.cipher_suite)
            .filter(|s| self.config.cipher_suites.contains(s))
            .ok_or(Error::illegal("提示していない暗号スイート"))?;
        if sh.is_hrr {
            return self.handle_hrr(sh, suite, msg, out);
        }
        if let Some(s) = self.hrr_suite
            && s != suite
        {
            return Err(Error::illegal("HRR と ServerHello のスイートが違う"));
        }
        let (g, peer_pub) = sh.key_share.ok_or(Error::fatal(
            AlertDescription::MISSING_EXTENSION,
            "ServerHello に key_share がない（psk_ke は提示していない）",
        ))?;
        let group = NamedGroup::from_u16(g).ok_or(Error::illegal("提示していない group"))?;
        let kx = self
            .shares
            .iter()
            .find(|(x, _, _)| *x == group)
            .map(|(_, k, _)| k)
            .ok_or(Error::illegal("key_share を送っていない group"))?;
        let psk = match sh.psk_identity {
            Some(i) => {
                let t = self.ticket.as_ref().ok_or(Error::illegal("PSK を提示していない"))?;
                if i != 0 {
                    return Err(Error::illegal("selected_identity が範囲外"));
                }
                if t.suite.hash() != suite.hash() {
                    return Err(Error::illegal("PSK と hash の違うスイートが選ばれた"));
                }
                self.c.resumed = true;
                Some(t.psk.clone())
            }
            None => None,
        };
        let shared = kx.agree(&peer_pub)?;
        self.shares.clear();
        self.c.suite = Some(suite);
        self.c.group = Some(group);
        let h = suite.hash();
        self.transcript.set_hash(h)?;
        self.transcript.add(msg);
        let mut ks = KeySchedule::early(h, psk.as_ref());
        ks.advance(Some(&shared))?;
        let th = self.transcript.hash()?;
        let c_hs = ks.derive(b"c hs traffic", &th)?;
        let s_hs = ks.derive(b"s hs traffic", &th)?;
        out.push(Action::ReadSecret {
            level: Level::Handshake,
            secret: s_hs.clone(),
        });
        out.push(Action::WriteSecret {
            level: Level::Handshake,
            secret: c_hs.clone(),
        });
        self.c_hs = Some(c_hs);
        self.s_hs = Some(s_hs);
        self.ks = Some(ks);
        self.state = State::WaitEncryptedExtensions;
        Ok(())
    }

    fn handle_hrr(
        &mut self,
        sh: msgs::ServerHello,
        suite: CipherSuite,
        msg: &[u8],
        out: &mut Vec<Action>,
    ) -> Result<(), Error> {
        if self.hrr_suite.is_some() {
            return Err(Error::unexpected("HelloRetryRequest が二回来た"));
        }
        let group = match sh.key_share {
            Some((g, _)) => {
                let group = NamedGroup::from_u16(g)
                    .filter(|x| self.config.groups.contains(x))
                    .ok_or(Error::illegal("HRR が提示していない group を選んだ"))?;
                if self.shares.iter().any(|(x, _, _)| *x == group) {
                    return Err(Error::illegal("HRR が key_share 済みの group を選んだ"));
                }
                Some(group)
            }
            None => None,
        };
        if group.is_none() && sh.cookie.is_none() {
            return Err(Error::illegal("HRR が何も変えない"));
        }
        self.c.hrr = true;
        self.hrr_suite = Some(suite);
        self.hrr_group = group;
        self.cookie = sh.cookie;
        self.transcript.restart_with_message_hash(suite.hash())?;
        self.transcript.add(msg);
        if self.ticket.as_ref().is_some_and(|t| t.suite.hash() != suite.hash()) {
            self.ticket = None;
        }
        if let Some(g) = group {
            let (k, p) = KeyExchange::generate(g)?;
            self.shares = vec![(g, k, p)];
        }
        // 中継機器互換: 二つ目の ClientHello の前に CCS を一度だけ送る（TCP）。
        if self.c.quic.is_none() && !self.sent_ccs {
            out.push(Action::Ccs);
            self.sent_ccs = true;
        }
        self.send_client_hello(out)
    }

    fn handle_encrypted_extensions(&mut self, msg: &[u8]) -> Result<(), Error> {
        let mut r = crate::codec::Reader::new(&msg[4..]);
        let exts = msgs::parse_extensions(r.vec16()?)?;
        r.finish()?;
        let unsupported = Error::fatal(AlertDescription::UNSUPPORTED_EXTENSION, "EncryptedExtensions に要求していない拡張");
        for (t, body) in exts {
            match t {
                ext::SERVER_NAME => {
                    if self.c.sni.is_none() {
                        return Err(unsupported);
                    }
                    if !body.is_empty() {
                        return Err(Error::decode("server_name の応答が空でない"));
                    }
                }
                ext::SUPPORTED_GROUPS => {} // 参考情報なので無視する。
                ext::ALPN => {
                    if self.config.alpn_protocols.is_empty() {
                        return Err(unsupported);
                    }
                    let v = msgs::parse_alpn(body)?;
                    if v.len() != 1 || !self.config.alpn_protocols.contains(&v[0]) {
                        return Err(Error::illegal("ALPN の応答が提示外"));
                    }
                    self.c.alpn = v.into_iter().next();
                }
                ext::RECORD_SIZE_LIMIT => {
                    if self.config.record_size_limit.is_none() || self.c.quic.is_some() {
                        return Err(unsupported);
                    }
                    let mut br = crate::codec::Reader::new(body);
                    let v = br.u16()?;
                    br.finish()?;
                    if v < 64 {
                        return Err(Error::illegal("record_size_limit が 64 未満"));
                    }
                    self.c.peer_record_limit = Some(v);
                    self.c.own_record_limit = self.config.record_size_limit.map(|l| l.clamp(64, 16385));
                }
                ext::QUIC_TRANSPORT_PARAMETERS => {
                    if self.c.quic.is_none() {
                        return Err(unsupported);
                    }
                    self.c.peer_transport_parameters = Some(body.to_vec());
                }
                ext::KEY_SHARE
                | ext::PRE_SHARED_KEY
                | ext::SUPPORTED_VERSIONS
                | ext::PSK_KEY_EXCHANGE_MODES
                | ext::COOKIE
                | ext::SIGNATURE_ALGORITHMS => {
                    return Err(Error::illegal("EncryptedExtensions に置けない拡張"));
                }
                _ => return Err(unsupported),
            }
        }
        if self.c.quic.is_some() {
            if self.c.peer_transport_parameters.is_none() {
                return Err(Error::fatal(
                    AlertDescription::MISSING_EXTENSION,
                    "quic_transport_parameters がない",
                ));
            }
            if self.c.alpn.is_none() {
                return Err(Error::fatal(AlertDescription::NO_APPLICATION_PROTOCOL, "QUIC で ALPN が決まらない"));
            }
        }
        self.transcript.add(msg);
        self.state = if self.c.resumed {
            State::WaitFinished
        } else {
            State::WaitCertOrCertRequest
        };
        Ok(())
    }

    fn handle_certificate(&mut self, msg: &[u8]) -> Result<(), Error> {
        let cm = msgs::parse_certificate(msg)?;
        if !cm.context.is_empty() {
            return Err(Error::illegal("certificate_request_context が空でない"));
        }
        if cm.certs.is_empty() {
            return Err(Error::decode("サーバーの証明書が空"));
        }
        let chain: Vec<CertificateDer<'static>> = cm.certs.into_iter().map(CertificateDer::from).collect();
        cert::verify_server(&self.config.verifier, &chain, &self.server_name, self.c.now)?;
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
        cert::verify_signature(leaf, scheme, &msgs::verify_message(true, &th), &sig)?;
        self.transcript.add(msg);
        self.state = State::WaitFinished;
        Ok(())
    }

    fn handle_finished(&mut self, msg: &[u8], out: &mut Vec<Action>) -> Result<(), Error> {
        let h = self.c.hash()?;
        let s_hs = self.s_hs.take().ok_or(Error::internal("鍵がない"))?;
        check_finished(h, &s_hs, &self.transcript.hash()?, &msg[4..])?;
        self.transcript.add(msg);
        let ks = self.ks.as_mut().ok_or(Error::internal("鍵スケジュールがない"))?;
        ks.advance(None)?;
        let th = self.transcript.hash()?;
        let c_ap = ks.derive(b"c ap traffic", &th)?;
        let s_ap = ks.derive(b"s ap traffic", &th)?;
        self.c.exporter = Some(ks.derive(b"exp master", &th)?);
        out.push(Action::ReadSecret {
            level: Level::OneRtt,
            secret: s_ap.clone(),
        });

        // client の二つ目の送信。
        if self.c.quic.is_none() && !self.sent_ccs {
            out.push(Action::Ccs);
            self.sent_ccs = true;
        }
        if let Some(cr) = self.cert_request.take() {
            let chosen = self
                .config
                .client_cert
                .as_ref()
                .and_then(|ck| ck.key.choose_scheme(&cr.signature_algorithms).map(|s| (ck.clone(), s)));
            match chosen {
                Some((ck, scheme)) => {
                    let certs: Vec<&[u8]> = ck.chain.iter().map(|c| c.as_ref()).collect();
                    let m = msgs::encode_certificate(&cr.context, &certs)?;
                    self.transcript.add(&m);
                    out.push(Action::Message {
                        level: Level::Handshake,
                        data: m,
                    });
                    let sig = ck.key.sign(scheme, &msgs::verify_message(false, &self.transcript.hash()?))?;
                    let m = msgs::encode_certificate_verify(scheme, &sig)?;
                    self.transcript.add(&m);
                    out.push(Action::Message {
                        level: Level::Handshake,
                        data: m,
                    });
                }
                None => {
                    let m = msgs::encode_certificate(&cr.context, &[])?;
                    self.transcript.add(&m);
                    out.push(Action::Message {
                        level: Level::Handshake,
                        data: m,
                    });
                }
            }
        }
        let c_hs = self.c_hs.take().ok_or(Error::internal("鍵がない"))?;
        let fin = msgs::handshake(hs::FINISHED, &finished_mac(h, &c_hs, &self.transcript.hash()?)?)?;
        self.transcript.add(&fin);
        out.push(Action::Message {
            level: Level::Handshake,
            data: fin,
        });
        let ks = self.ks.as_ref().ok_or(Error::internal("鍵スケジュールがない"))?;
        self.res_master = Some(ks.derive(b"res master", &self.transcript.hash()?)?);
        self.ks = None;
        out.push(Action::WriteSecret {
            level: Level::OneRtt,
            secret: c_ap.clone(),
        });
        self.c.client_app = Some(c_ap);
        self.c.server_app = Some(s_ap);
        self.c.complete = true;
        self.state = State::Connected;
        out.push(Action::Complete);
        Ok(())
    }

    fn handle_new_session_ticket(&mut self, msg: &[u8]) -> Result<(), Error> {
        let t = msgs::parse_new_session_ticket(msg)?;
        let (Some(store), Some(rm), Some(suite)) = (&self.config.session_store, &self.res_master, self.c.suite) else {
            return Ok(());
        };
        if t.lifetime == 0 {
            return Ok(());
        }
        let psk = resumption_psk(suite.hash(), rm, &t.nonce)?;
        store.put(
            &self.store_key,
            StoredTicket {
                suite,
                ticket: t.ticket,
                psk,
                age_add: t.age_add,
                lifetime_secs: t.lifetime,
                received_ms: self.c.now_ms,
                alpn: self.c.alpn.clone(),
            },
        );
        Ok(())
    }
}

impl Handshaker for ClientHs {
    fn start(&mut self, out: &mut Vec<Action>) -> Result<(), Error> {
        if self.state != State::Start {
            return Err(Error::InvalidState("二度目の start"));
        }
        self.random = random_array()?;
        // 中継機器互換（TCP）: 32 バイトの legacy_session_id。QUIC では空（RFC 9001 8.4）。
        self.session_id = if self.c.quic.is_none() {
            random_array::<32>()?.to_vec()
        } else {
            Vec::new()
        };
        if let Some(store) = &self.config.session_store
            && let Some(t) = store.take(&self.store_key)
            && t.valid_at(self.c.now_ms)
            && self.config.cipher_suites.iter().any(|s| s.hash() == t.suite.hash())
        {
            self.ticket = Some(t);
        }
        let n = self.config.key_share_count.clamp(1, self.config.groups.len());
        for g in self.config.groups.iter().take(n) {
            let (k, p) = KeyExchange::generate(*g)?;
            self.shares.push((*g, k, p));
        }
        self.send_client_hello(out)?;
        self.state = State::WaitServerHello;
        Ok(())
    }

    fn handle(&mut self, level: Level, msg: &[u8], out: &mut Vec<Action>) -> Result<(), Error> {
        let typ = msg[0];
        match (self.state, typ) {
            (State::WaitServerHello, hs::SERVER_HELLO) => {
                expect_level(level, Level::Initial)?;
                self.handle_server_hello(msg, out)
            }
            (State::WaitEncryptedExtensions, hs::ENCRYPTED_EXTENSIONS) => {
                expect_level(level, Level::Handshake)?;
                self.handle_encrypted_extensions(msg)
            }
            (State::WaitCertOrCertRequest, hs::CERTIFICATE_REQUEST) => {
                expect_level(level, Level::Handshake)?;
                let cr = msgs::parse_certificate_request(msg)?;
                if !cr.context.is_empty() {
                    return Err(Error::illegal("certificate_request_context が空でない"));
                }
                self.cert_request = Some(cr);
                self.transcript.add(msg);
                self.state = State::WaitCert;
                Ok(())
            }
            (State::WaitCertOrCertRequest | State::WaitCert, hs::CERTIFICATE) => {
                expect_level(level, Level::Handshake)?;
                self.handle_certificate(msg)
            }
            (State::WaitCertVerify, hs::CERTIFICATE_VERIFY) => {
                expect_level(level, Level::Handshake)?;
                self.handle_certificate_verify(msg)
            }
            (State::WaitFinished, hs::FINISHED) => {
                expect_level(level, Level::Handshake)?;
                self.handle_finished(msg, out)
            }
            (State::Connected, hs::NEW_SESSION_TICKET) => {
                expect_level(level, Level::OneRtt)?;
                self.handle_new_session_ticket(msg)
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
        // 最初の ClientHello を送った後、サーバーの Finished を受け取るまで。
        !matches!(self.state, State::Start | State::Connected)
    }
}
