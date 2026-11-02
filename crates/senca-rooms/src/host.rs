//! [`Action`] のうちピア接続に関わるものを senca-webrtc の [`PeerConnection`] で実行する部品（sans-io）。
//!
//! 1 つの UDP ソケットで全員を受ける形（ADR-0012）: 受けた datagram は送り元のアドレス（ICE が選んだ組）か
//! STUN の USERNAME（自分の ufrag）で接続へ振り分ける。
//!
//! ```text
//! let mut host = PeerHost::new(PeerConfig { certificate: Some(cert), ..Default::default() }, udp_addr);
//! loop {
//!     // 入力: svc.on_signaling_text(..) / host.handle_datagram(now, from, &buf)
//!     host.pump(now, &mut svc, |a| match a {               // ピア接続以外の行動はノードが実行する
//!         Action::Signal { conn, text, .. } => ws_send(conn, text),
//!         Action::CloseSignaling { conn, code, reason } => ws_close(conn, code, reason),
//!         Action::Event(e) => { sse.publish(&e); outbox.push(now, &e) }
//!         Action::AuthWebhook { .. } | Action::Relay(_) => { .. }
//!         _ => {}
//!     });
//!     while let Some(t) = host.poll_transmit() { udp.send_to(&t.data, t.remote) }
//!     sleep_until(min(host.poll_timeout(), svc.poll_timeout()));
//!     host.handle_timeout(now); svc.handle_timeout(now);
//! }
//! ```

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use senca_core::media::Codec;
use senca_webrtc::{PeerConfig, PeerConnection, Transmit, stun_username_ufrag};

use crate::service::{Action, ConnKey, PeerSpec, PeerStats, RoomService, SignalPath};
use crate::signaling::{DisconnectReason, ErrorCode, SIGNALING_LABEL};

struct Hosted {
    pc: PeerConnection,
    remote: Option<SocketAddr>,
}

/// ピア接続の集まり。
pub struct PeerHost {
    base: PeerConfig,
    addr: SocketAddr,
    peers: BTreeMap<ConnKey, Hosted>,
    by_remote: HashMap<SocketAddr, ConnKey>,
    tx: VecDeque<Transmit>,
    stats_interval: Duration,
    next_stats: Option<Instant>,
}

impl std::fmt::Debug for PeerHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerHost")
            .field("addr", &self.addr)
            .field("peers", &self.peers.len())
            .finish_non_exhaustive()
    }
}

/// コーデックを SDP の encoding 名に（能力の絞り込み用）。
fn encoding(c: Codec) -> &'static str {
    match c {
        Codec::H264 => "h264",
        Codec::H265 => "h265",
        Codec::Vp8 => "vp8",
        Codec::Vp9 => "vp9",
        Codec::Av1 => "av1",
        Codec::Opus => "opus",
        _ => "",
    }
}

impl PeerHost {
    /// `base` は全員に共通の設定（DTLS の証明書は使い回すとよい）、`addr` は UDP ソケットの公開アドレス。
    pub fn new(base: PeerConfig, addr: SocketAddr) -> PeerHost {
        PeerHost {
            base,
            addr,
            peers: BTreeMap::new(),
            by_remote: HashMap::new(),
            tx: VecDeque::new(),
            stats_interval: Duration::from_secs(1),
            next_stats: None,
        }
    }

    pub fn peer(&self, k: ConnKey) -> Option<&PeerConnection> {
        self.peers.get(&k).map(|h| &h.pc)
    }

    pub fn peer_mut(&mut self, k: ConnKey) -> Option<&mut PeerConnection> {
        self.peers.get_mut(&k).map(|h| &mut h.pc)
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    fn collect_tx(&mut self) {
        for (k, h) in self.peers.iter_mut() {
            while let Some(t) = h.pc.poll_transmit() {
                self.tx.push_back(t);
            }
            if let Some((_, r)) = h.pc.selected_path()
                && h.remote != Some(r)
            {
                if let Some(old) = h.remote {
                    self.by_remote.remove(&old);
                }
                h.remote = Some(r);
                self.by_remote.insert(r, *k);
            }
        }
    }

    fn create(&mut self, now: Instant, spec: &PeerSpec) -> Result<PeerConnection, String> {
        let mut cfg = self.base.clone();
        let want = [encoding(spec.video_codec), encoding(spec.audio_codec)];
        cfg.capabilities.codecs.retain(|c| {
            let e = c.encoding.to_ascii_lowercase();
            want.contains(&e.as_str()) || e == "rtx"
        });
        cfg.datachannel = spec.datachannel;
        let mut pc = PeerConnection::new(cfg, now).map_err(|e| format!("{e:?}"))?;
        pc.add_host_address(now, self.addr).map_err(|e| format!("{e:?}"))?;
        Ok(pc)
    }

    /// 行動を 1 つ実行する。ピア接続に関わらないもの（シグナリング・イベント・Webhook・中継）はそのまま返す。
    pub fn execute(&mut self, now: Instant, svc: &mut RoomService, a: Action) -> Option<Action> {
        match a {
            Action::CreatePeer { conn, spec } => match self.create(now, &spec) {
                Ok(pc) => {
                    self.peers.insert(conn, Hosted { pc, remote: None });
                }
                Err(e) => svc.disconnect(now, conn, DisconnectReason::Error, Some((ErrorCode::Internal, e))),
            },
            Action::AcceptOffer { conn, sdp } => {
                let Some(h) = self.peers.get_mut(&conn) else {
                    svc.on_offer_result(now, conn, Err("no peer".into()), &[]);
                    return None;
                };
                let r = h.pc.accept_offer(now, &sdp).map_err(|e| format!("{e:?}"));
                let tracks = h.pc.send_tracks();
                svc.on_offer_result(now, conn, r, &tracks);
            }
            Action::AddCandidate { conn, candidate } => {
                if let Some(h) = self.peers.get_mut(&conn) {
                    if candidate.is_empty() {
                        h.pc.end_of_remote_candidates(now);
                    } else {
                        let _ = h.pc.add_remote_candidate(now, &candidate);
                    }
                }
            }
            Action::ClosePeer { conn } => {
                if let Some(mut h) = self.peers.remove(&conn) {
                    h.pc.close(now);
                    while let Some(t) = h.pc.poll_transmit() {
                        self.tx.push_back(t);
                    }
                    if let Some(r) = h.remote {
                        self.by_remote.remove(&r);
                    }
                }
            }
            Action::Forward { conn, track, au, config } => {
                if let Some(h) = self.peers.get_mut(&conn) {
                    let _ = h.pc.send_access_unit(now, track, &au, config.as_ref());
                }
            }
            Action::RequestKeyframe { conn, track } => {
                if let Some(h) = self.peers.get_mut(&conn) {
                    let _ = h.pc.request_keyframe(now, track);
                }
            }
            Action::SetDesiredBitrate { conn, bps } => {
                if let Some(h) = self.peers.get_mut(&conn) {
                    h.pc.set_desired_bitrate(bps);
                }
            }
            Action::SendData { conn, label, data, binary } => {
                if let Some(h) = self.peers.get_mut(&conn)
                    && let Some(id) = h.pc.data_channel_by_label(&label)
                {
                    match (binary, std::str::from_utf8(&data)) {
                        (false, Ok(t)) => {
                            let _ = h.pc.send_data_text(now, id, t);
                        }
                        _ => {
                            let _ = h.pc.send_data_binary(now, id, data);
                        }
                    }
                }
            }
            Action::CloseDataChannel { conn, id } => {
                if let Some(h) = self.peers.get_mut(&conn) {
                    let _ = h.pc.close_data_channel(now, id);
                }
            }
            Action::Signal { conn, path: SignalPath::DataChannel, text } => {
                let sent = self.peers.get_mut(&conn).is_some_and(|h| {
                    h.pc.data_channel_by_label(SIGNALING_LABEL)
                        .is_some_and(|id| h.pc.send_data_text(now, id, &text).is_ok())
                });
                if !sent {
                    // DataChannel が使えなければ WebSocket で（開いていれば）
                    return Some(Action::Signal { conn, path: SignalPath::WebSocket, text });
                }
            }
            other => return Some(other),
        }
        None
    }

    /// 受けた datagram（自分のどの接続のものでもなければ偽）。
    pub fn handle_datagram(&mut self, now: Instant, from: SocketAddr, data: &[u8]) -> bool {
        let key = self.by_remote.get(&from).copied().or_else(|| {
            let u = stun_username_ufrag(data)?;
            self.peers.iter().find(|(_, h)| h.pc.local_ufrag() == u).map(|(k, _)| *k)
        });
        let Some(h) = key.and_then(|k| self.peers.get_mut(&k)) else {
            return false;
        };
        h.pc.handle_datagram(now, from, data);
        true
    }

    /// ピア接続の出来事を部屋へ入れ、出てきた行動を実行する。ピア接続以外の行動は `f` へ。
    pub fn pump(&mut self, now: Instant, svc: &mut RoomService, mut f: impl FnMut(Action)) {
        for _ in 0..10_000 {
            let mut progressed = false;
            let keys: Vec<ConnKey> = self.peers.keys().copied().collect();
            for k in keys {
                let mut evs = Vec::new();
                if let Some(h) = self.peers.get_mut(&k) {
                    while let Some(e) = h.pc.poll_event() {
                        evs.push(e);
                    }
                }
                for e in evs {
                    progressed = true;
                    svc.on_peer_event(now, k, e);
                }
            }
            if self.next_stats.is_none_or(|t| now >= t) {
                self.next_stats = Some(now + self.stats_interval);
                for (k, h) in &self.peers {
                    let (retransmitted, nack_requested) = h.pc.retransmission_counters();
                    let s = PeerStats {
                        rtt_ms: h.pc.rtt().as_millis() as u64,
                        bandwidth_bps: h.pc.bandwidth_estimate(),
                        send_bitrate_bps: h.pc.send_bitrate(),
                        retransmitted,
                        nack_requested,
                        dropped_srtp: h.pc.dropped_counters().2,
                    };
                    svc.on_peer_stats(now, *k, s);
                }
            }
            while let Some(a) = svc.poll_action() {
                progressed = true;
                if let Some(a) = self.execute(now, svc, a) {
                    f(a);
                }
            }
            self.collect_tx();
            if !progressed {
                break;
            }
        }
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        if self.tx.is_empty() {
            self.collect_tx();
        }
        self.tx.pop_front()
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        let mut t = self.next_stats;
        for h in self.peers.values() {
            if let Some(x) = h.pc.poll_timeout() {
                t = Some(t.map_or(x, |y| y.min(x)));
            }
        }
        t
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        for h in self.peers.values_mut() {
            if h.pc.poll_timeout().is_some_and(|t| t <= now) {
                h.pc.handle_timeout(now);
            }
        }
        self.collect_tx();
    }
}
