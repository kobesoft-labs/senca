//! DataChannel（RFC 8831・RFC 8832）。1 つの [`Association`] の上で DCEP を話す。

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::dcep::{self, Message as DcepMessage, Open};
use crate::{
    Association, ChannelInfo, ChannelOptions, DataChannelEvent, Reliability, SctpConfig, SctpError,
    SctpEvent, SendOptions, ppid,
};

/// チャネルの数の上限。
pub const MAX_CHANNELS: usize = 4096;
/// label・protocol の総量の上限（全チャネル合計）。
pub const MAX_CHANNEL_METADATA: usize = 4 * 1024 * 1024;
/// 使わない番号（RFC 8832 6: 65535 は予約）。
const RESERVED_ID: u16 = 65535;

/// 廃止された PPID（partial）。無視する。
const PPID_STRING_PARTIAL: u32 = 52;
const PPID_BINARY_PARTIAL: u32 = 54;

struct Channel {
    info: ChannelInfo,
    /// OPEN に載せる優先度。
    priority: u16,
    /// OPEN をまだ送っていない（association ができるのを待っている）。
    open_pending: bool,
    /// OPEN を送って ACK を待っている。
    awaiting_ack: bool,
    /// Open の出来事を出した。
    announced: bool,
    closing: bool,
    out_reset_requested: bool,
    out_reset_done: bool,
    in_reset_done: bool,
    /// 閉じ終わる前に届いた、同じ番号の相手の新しい OPEN（閉じ終わってから受ける）。
    pending_peer_open: Option<Open>,
    /// その新しいチャネルを相手がもう閉じた（送信ストリームをリセットした）。
    pending_open_reset: bool,
}

impl Channel {
    fn metadata_len(&self) -> usize {
        self.info.label.len() + self.info.protocol.len()
    }
}

/// DataChannel の束（1 つの association の上）。
pub struct DataChannels {
    assoc: Association,
    is_client: bool,
    num_streams: u16,
    channels: BTreeMap<u16, Channel>,
    metadata: usize,
    connected: bool,
    events: VecDeque<DataChannelEvent>,
    /// `events` にあるメッセージのバイト数。
    pub(crate) queued_bytes: usize,
    /// これを超えたら association から出来事を引き出さない（相手への受信窓で抑える）。
    max_queued_bytes: usize,
    /// 最後に渡された時刻（`poll_event` から association を動かすときに使う）。
    now: Instant,
}

fn send_options(info: &ChannelInfo, before_ack: bool) -> SendOptions {
    let (max_retransmits, max_lifetime) = match info.reliability {
        Reliability::Reliable => (None, None),
        Reliability::MaxRetransmits(n) => (Some(n), None),
        Reliability::MaxLifetimeMs(ms) => (None, Some(Duration::from_millis(ms as u64))),
    };
    SendOptions {
        // ACK を受けるまでは順序付きで送る（RFC 8832 6）。
        unordered: !info.ordered && !before_ack,
        max_retransmits,
        max_lifetime,
    }
}

impl DataChannels {
    /// `is_dtls_client` は DTLS の役割（client は偶数、server は奇数の番号を使う。RFC 8832 6）。
    pub fn new(cfg: SctpConfig, is_dtls_client: bool, now: Instant) -> Result<Self, SctpError> {
        let num_streams = cfg.num_streams;
        let max_queued_bytes = cfg.receive_window as usize;
        Ok(DataChannels {
            assoc: Association::new(cfg, now)?,
            is_client: is_dtls_client,
            num_streams,
            channels: BTreeMap::new(),
            metadata: 0,
            connected: false,
            events: VecDeque::new(),
            queued_bytes: 0,
            max_queued_bytes,
            now,
        })
    }

    pub fn connect(&mut self, now: Instant) {
        self.assoc.connect(now);
        self.pump(now);
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn association(&self) -> &Association {
        &self.assoc
    }

    pub fn association_mut(&mut self) -> &mut Association {
        &mut self.assoc
    }

    /// 使える番号の上限（この値未満）。
    fn id_limit(&self) -> u16 {
        let n = if self.connected {
            self.assoc
                .outbound_streams()
                .min(self.assoc.inbound_streams())
        } else {
            self.num_streams
        };
        // 65535 は予約（num_streams が 65535 なら番号は 0..=65534）。
        n
    }

    fn own_parity(&self) -> u16 {
        if self.is_client { 0 } else { 1 }
    }

    /// チャネルを開く。番号を返す（DCEP の OPEN を送る。negotiated ならすぐ開く）。
    /// association ができる前に呼んでもよい（できてから OPEN を送る）。
    pub fn open(&mut self, now: Instant, opts: ChannelOptions) -> Result<u16, SctpError> {
        if self.assoc.is_terminated() {
            return Err(SctpError::Closed);
        }
        if opts.label.len() > u16::MAX as usize || opts.protocol.len() > u16::MAX as usize {
            return Err(SctpError::TooLarge);
        }
        if self.channels.len() >= MAX_CHANNELS
            || self.metadata + opts.label.len() + opts.protocol.len() > MAX_CHANNEL_METADATA
        {
            return Err(SctpError::ChannelIdUnavailable);
        }
        let limit = self.id_limit();
        let negotiated = opts.negotiated_id.is_some();
        let id = match opts.negotiated_id {
            Some(id) => {
                if id >= limit || self.channels.contains_key(&id) {
                    return Err(SctpError::ChannelIdUnavailable);
                }
                id
            }
            None => {
                let mut id = self.own_parity();
                loop {
                    if id >= limit {
                        return Err(SctpError::ChannelIdUnavailable);
                    }
                    if !self.channels.contains_key(&id) {
                        break id;
                    }
                    id = match id.checked_add(2) {
                        Some(v) => v,
                        None => return Err(SctpError::ChannelIdUnavailable),
                    };
                }
            }
        };
        let info = ChannelInfo {
            id,
            label: opts.label,
            protocol: opts.protocol,
            ordered: opts.ordered,
            reliability: opts.reliability,
            negotiated,
            open: negotiated,
        };
        let mut ch = Channel {
            info,
            priority: opts.priority,
            open_pending: !negotiated,
            awaiting_ack: false,
            announced: false,
            closing: false,
            out_reset_requested: false,
            out_reset_done: false,
            in_reset_done: false,
            pending_peer_open: None,
            pending_open_reset: false,
        };
        if negotiated {
            if self.connected {
                ch.announced = true;
                self.events
                    .push_back(DataChannelEvent::Open(ch.info.clone()));
            }
        } else if self.connected {
            self.send_open(now, &mut ch)?;
        }
        self.metadata += ch.metadata_len();
        self.channels.insert(id, ch);
        Ok(id)
    }

    fn send_open(&mut self, now: Instant, ch: &mut Channel) -> Result<(), SctpError> {
        let msg = Open {
            ordered: ch.info.ordered,
            reliability: ch.info.reliability,
            priority: ch.priority,
            label: ch.info.label.clone(),
            protocol: ch.info.protocol.clone(),
        }
        .encode()?;
        self.assoc
            .send(now, ch.info.id, ppid::DCEP, msg, SendOptions::default())?;
        ch.open_pending = false;
        ch.awaiting_ack = true;
        Ok(())
    }

    pub fn send_text(&mut self, now: Instant, id: u16, text: &str) -> Result<(), SctpError> {
        let (p, data) = if text.is_empty() {
            (ppid::STRING_EMPTY, Bytes::from_static(&[0]))
        } else {
            (ppid::STRING, Bytes::copy_from_slice(text.as_bytes()))
        };
        self.send_user(now, id, p, data)
    }

    pub fn send_binary(&mut self, now: Instant, id: u16, data: Bytes) -> Result<(), SctpError> {
        let (p, data) = if data.is_empty() {
            (ppid::BINARY_EMPTY, Bytes::from_static(&[0]))
        } else {
            (ppid::BINARY, data)
        };
        self.send_user(now, id, p, data)
    }

    fn send_user(&mut self, now: Instant, id: u16, p: u32, data: Bytes) -> Result<(), SctpError> {
        let ch = self.channels.get(&id).ok_or(SctpError::UnknownChannel)?;
        if ch.closing {
            return Err(SctpError::UnknownChannel);
        }
        if ch.open_pending || !self.connected {
            return Err(SctpError::NotConnected);
        }
        let opts = send_options(&ch.info, ch.awaiting_ack);
        let r = self.assoc.send(now, id, p, data, opts);
        self.pump(now);
        r
    }

    /// チャネルを閉じる（送信ストリームをリセットする）。
    pub fn close(&mut self, now: Instant, id: u16) -> Result<(), SctpError> {
        let ch = self
            .channels
            .get_mut(&id)
            .ok_or(SctpError::UnknownChannel)?;
        if ch.closing {
            return Ok(());
        }
        ch.closing = true;
        if !self.connected {
            let _ = self.remove_channel(id);
            return Ok(());
        }
        self.request_out_reset(now, id);
        self.check_closed(now, id);
        self.pump(now);
        Ok(())
    }

    fn request_out_reset(&mut self, now: Instant, id: u16) {
        let Some(ch) = self.channels.get_mut(&id) else {
            return;
        };
        if ch.out_reset_requested {
            return;
        }
        ch.out_reset_requested = true;
        if self.assoc.reset_streams(now, &[id]).is_err() {
            // リセットできない（相手が RE-CONFIG を使えないなど）: 閉じたことにする。
            if let Some(ch) = self.channels.get_mut(&id) {
                ch.out_reset_done = true;
                ch.in_reset_done = true;
            }
        }
    }

    fn check_closed(&mut self, now: Instant, id: u16) {
        if self
            .channels
            .get(&id)
            .is_some_and(|c| c.out_reset_done && c.in_reset_done)
            && let Some((open, reset)) = self.remove_channel(id)
        {
            self.accept_open(now, id, open);
            if reset && let Some(ch) = self.channels.get_mut(&id) {
                ch.in_reset_done = true;
                ch.closing = true;
                self.request_out_reset(now, id);
                self.check_closed(now, id);
            }
        }
    }

    /// チャネルを除いて Closed を出す。閉じ終わる前に届いていた相手の OPEN を返す。
    fn remove_channel(&mut self, id: u16) -> Option<(Open, bool)> {
        let ch = self.channels.remove(&id)?;
        self.metadata -= ch.metadata_len();
        self.events.push_back(DataChannelEvent::Closed { id });
        ch.pending_peer_open.map(|o| (o, ch.pending_open_reset))
    }

    pub fn channel(&self, id: u16) -> Option<&ChannelInfo> {
        self.channels.get(&id).map(|c| &c.info)
    }

    pub fn channels(&self) -> Vec<ChannelInfo> {
        self.channels.values().map(|c| c.info.clone()).collect()
    }

    pub fn buffered_amount(&self, id: u16) -> usize {
        self.assoc.buffered_amount(id)
    }

    /// チャネルの BufferedAmountLow の閾値（RTCDataChannel.bufferedAmountLowThreshold）。
    pub fn set_buffered_amount_low_threshold(&mut self, id: u16, bytes: usize) {
        self.assoc.set_buffered_amount_low_threshold(id, bytes);
    }

    pub fn handle_packet(&mut self, now: Instant, packet: &[u8]) -> Result<(), SctpError> {
        let r = self.assoc.handle_packet(now, packet);
        self.pump(now);
        r
    }

    pub fn poll_transmit(&mut self) -> Option<Bytes> {
        self.assoc.poll_transmit()
    }

    pub fn poll_event(&mut self) -> Option<DataChannelEvent> {
        if self.events.is_empty() {
            self.pump(self.now);
        }
        let e = self.events.pop_front()?;
        if let DataChannelEvent::Message { data, .. } = &e {
            self.queued_bytes = self.queued_bytes.saturating_sub(data.len());
        }
        Some(e)
    }

    pub fn poll_timeout(&self) -> Option<Instant> {
        self.assoc.poll_timeout()
    }

    pub fn handle_timeout(&mut self, now: Instant) {
        self.assoc.handle_timeout(now);
        self.pump(now);
    }

    pub fn shutdown(&mut self, now: Instant) {
        self.assoc.shutdown(now);
        self.pump(now);
    }

    /// association の出来事を DataChannel の出来事に直す。
    ///
    /// 取り出されていないメッセージが多いときは引き出さない（association の受信窓が
    /// 閉じて相手が止まる）。
    fn pump(&mut self, now: Instant) {
        self.now = now;
        while self.queued_bytes < self.max_queued_bytes {
            let Some(e) = self.assoc.poll_event() else {
                break;
            };
            match e {
                SctpEvent::Connected => self.on_connected(now),
                SctpEvent::Message {
                    stream, ppid, data, ..
                } => self.on_message(now, stream, ppid, data),
                SctpEvent::IncomingStreamsReset { streams } => {
                    let ids: Vec<u16> = if streams.is_empty() {
                        self.channels.keys().copied().collect()
                    } else {
                        streams
                    };
                    for id in ids {
                        let Some(ch) = self.channels.get_mut(&id) else {
                            continue;
                        };
                        if ch.pending_peer_open.is_some() {
                            // 相手は使い直した新しいチャネルもすぐ閉じた。
                            ch.pending_open_reset = true;
                            continue;
                        }
                        ch.in_reset_done = true;
                        ch.closing = true;
                        // 相手が閉じたらこちらも送信ストリームをリセットする。
                        self.request_out_reset(now, id);
                        self.check_closed(now, id);
                    }
                }
                SctpEvent::OutgoingStreamsReset { streams } => {
                    for id in streams {
                        if let Some(ch) = self.channels.get_mut(&id) {
                            ch.out_reset_done = true;
                            self.check_closed(now, id);
                        }
                    }
                }
                SctpEvent::BufferedAmountLow { stream } => {
                    if self.channels.contains_key(&stream) {
                        self.events
                            .push_back(DataChannelEvent::BufferedAmountLow { id: stream });
                    }
                }
                SctpEvent::Closed { reason } => {
                    self.connected = false;
                    let ids: Vec<u16> = self.channels.keys().copied().collect();
                    for id in ids {
                        let _ = self.remove_channel(id);
                    }
                    self.events
                        .push_back(DataChannelEvent::Disconnected { reason });
                }
            }
        }
    }

    fn on_connected(&mut self, now: Instant) {
        self.connected = true;
        self.events.push_back(DataChannelEvent::Connected);
        let limit = self.id_limit();
        let ids: Vec<u16> = self.channels.keys().copied().collect();
        for id in ids {
            let Some(mut ch) = self.channels.remove(&id) else {
                continue;
            };
            if id >= limit {
                // 相手のストリーム数を超えた番号は使えない。
                self.metadata -= ch.metadata_len();
                self.events.push_back(DataChannelEvent::Closed { id });
                continue;
            }
            if ch.info.negotiated && !ch.announced {
                ch.announced = true;
                self.events
                    .push_back(DataChannelEvent::Open(ch.info.clone()));
            } else if ch.open_pending && self.send_open(now, &mut ch).is_err() {
                self.metadata -= ch.metadata_len();
                self.events.push_back(DataChannelEvent::Closed { id });
                continue;
            }
            self.channels.insert(id, ch);
        }
    }

    fn on_message(&mut self, now: Instant, stream: u16, p: u32, data: Bytes) {
        match p {
            ppid::DCEP => self.on_dcep(now, stream, &data),
            ppid::STRING | ppid::BINARY | ppid::STRING_EMPTY | ppid::BINARY_EMPTY => {
                let Some(ch) = self.channels.get_mut(&stream) else {
                    return;
                };
                if ch.in_reset_done {
                    return;
                }
                if ch.awaiting_ack {
                    // ACK より先にデータが来たら ACK とみなす（RFC 8832 6）。
                    ch.awaiting_ack = false;
                    ch.info.open = true;
                    ch.announced = true;
                    self.events
                        .push_back(DataChannelEvent::Open(ch.info.clone()));
                }
                if !ch.info.open {
                    return;
                }
                let binary = p == ppid::BINARY || p == ppid::BINARY_EMPTY;
                let data = if p == ppid::STRING_EMPTY || p == ppid::BINARY_EMPTY {
                    Bytes::new()
                } else {
                    data
                };
                self.queued_bytes += data.len();
                self.events.push_back(DataChannelEvent::Message {
                    id: stream,
                    data,
                    binary,
                });
            }
            PPID_STRING_PARTIAL | PPID_BINARY_PARTIAL => {}
            _ => {}
        }
    }

    /// 相手の OPEN を受けてチャネルを作り、ACK を返す。
    fn accept_open(&mut self, now: Instant, stream: u16, o: Open) {
        if self.channels.len() >= MAX_CHANNELS
            || self.metadata + o.label.len() + o.protocol.len() > MAX_CHANNEL_METADATA
        {
            // 受けられない: ストリームをリセットして断る。
            let _ = self.assoc.reset_streams(now, &[stream]);
            return;
        }
        if self
            .assoc
            .send(
                now,
                stream,
                ppid::DCEP,
                dcep::encode_ack(),
                SendOptions::default(),
            )
            .is_err()
        {
            return;
        }
        let info = ChannelInfo {
            id: stream,
            label: o.label,
            protocol: o.protocol,
            ordered: o.ordered,
            reliability: o.reliability,
            negotiated: false,
            open: true,
        };
        let ch = Channel {
            info: info.clone(),
            priority: o.priority,
            open_pending: false,
            awaiting_ack: false,
            announced: true,
            closing: false,
            out_reset_requested: false,
            out_reset_done: false,
            in_reset_done: false,
            pending_peer_open: None,
            pending_open_reset: false,
        };
        self.metadata += ch.metadata_len();
        self.channels.insert(stream, ch);
        self.events.push_back(DataChannelEvent::Open(info));
    }

    fn on_dcep(&mut self, now: Instant, stream: u16, data: &[u8]) {
        let Ok(msg) = dcep::parse(data) else {
            return;
        };
        match msg {
            DcepMessage::Open(o) => {
                // 相手は自分と違う偶奇の番号を使う。
                if stream % 2 == self.own_parity() || stream == RESERVED_ID {
                    return;
                }
                if let Some(ch) = self.channels.get_mut(&stream) {
                    // 相手は閉じ終わって同じ番号を使い直した（こちらの送信ストリームのリセットの
                    // 終わりを待っている）。閉じ終わってから受ける。
                    if ch.in_reset_done && ch.pending_peer_open.is_none() {
                        ch.pending_peer_open = Some(o);
                    }
                    return;
                }
                self.accept_open(now, stream, o);
            }
            DcepMessage::Ack => {
                if let Some(ch) = self.channels.get_mut(&stream)
                    && ch.awaiting_ack
                {
                    ch.awaiting_ack = false;
                    ch.info.open = true;
                    ch.announced = true;
                    self.events
                        .push_back(DataChannelEvent::Open(ch.info.clone()));
                }
            }
        }
    }
}
