//! 一つの MoQ セッション（draft-22）。sans-io の状態機械。
//!
//! - 制御 stream: 自分と相手の単方向 stream が一本ずつ。最初は SETUP（型 0x2F00 が stream の型を兼ねる）。
//! - 要求: 双方向 stream を一本ずつ使う（SUBSCRIBE・PUBLISH・FETCH・TRACK_STATUS・PUBLISH_NAMESPACE・
//!   SUBSCRIBE_NAMESPACE）。応答と後続のメッセージは同じ stream。取り消しは RESET_STREAM・STOP_SENDING。
//! - object: subgroup ごとの単方向 stream、datagram、fetch の単方向 stream。
//!
//! 書き込みはセッションの中の送信待ちに積み、[`Session::flush`] で [`Transport`] へ書く。
//! 受信は [`Session::on_stream_data`] などに渡す（[`crate::transport::drive_quic`] が行う）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::data::{
    self, DataStreamReader, FetchPrior, StreamHeader, StreamItem, SubgroupHeader, SubgroupId, encode_fetch_header,
    encode_fetch_object, encode_subgroup_object,
};
pub use crate::data::{ObjectStatus, RangeEnd};
use crate::error::{MoqError, RequestErrorCode, SessionErrorCode, reset};
use crate::message::{Message, MessageReader};
use crate::params::{GroupOrder, LocationFilter, Params, SetupOptions, Token, TrackProperties};
use crate::transport::Transport;
use crate::{Location, Namespace};

/// セッションのどちら側か。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

/// 設定と上限。
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub role: Role,
    /// MOQT_IMPLEMENTATION。
    pub implementation: Option<String>,
    /// client が native QUIC で送る PATH・AUTHORITY（WebTransport では送らない）。
    pub path: Option<String>,
    pub authority: Option<String>,
    /// client が SETUP に入れる認可トークン。
    pub setup_tokens: Vec<Token>,
    /// native QUIC か（WebTransport なら PATH・AUTHORITY は誤り）。
    pub native_quic: bool,
    /// 一つの object の上限。
    pub max_object_size: usize,
    /// 同時に持つ要求の上限（相手から）。
    pub max_peer_requests: usize,
    /// track alias が分からないまま溜める stream の数。
    pub max_unknown_alias_streams: usize,
    /// その stream ごとに溜める object の数。
    pub max_unknown_alias_objects: usize,
    /// 送信待ちの上限（stream ごと、超えたら古い subgroup から捨てる）。
    pub max_stream_queue: usize,
}

impl SessionConfig {
    pub fn new(role: Role) -> Self {
        Self {
            role,
            implementation: Some(format!("senca-moq/{}", env!("CARGO_PKG_VERSION"))),
            path: None,
            authority: None,
            setup_tokens: Vec::new(),
            native_quic: true,
            max_object_size: 16 << 20,
            max_peer_requests: 1000,
            max_unknown_alias_streams: 16,
            max_unknown_alias_objects: 64,
            max_stream_queue: 8 << 20,
        }
    }
}

/// object の運ばれ方。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Subgroup,
    Datagram,
    Fetch,
}

/// object。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    pub group: u64,
    /// datagram の object は `None`。
    pub subgroup: Option<u64>,
    pub id: u64,
    pub priority: u8,
    pub status: ObjectStatus,
    /// Object Properties（Key-Value-Pair の並び、そのまま転送する）。
    pub properties: Bytes,
    pub payload: Bytes,
}

impl Object {
    pub fn location(&self) -> Location {
        Location::new(self.group, self.id)
    }
}

/// SUBSCRIBE の引数。
#[derive(Clone, Debug)]
pub struct SubscribeArgs {
    pub namespace: Namespace,
    pub track: Bytes,
    pub params: Params,
}

/// アプリケーション・relay へのイベント。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// 相手の SETUP を受けた。
    Ready { peer: SetupOptions },
    Subscribe {
        request_id: u64,
        namespace: Namespace,
        track: Bytes,
        params: Params,
    },
    Fetch {
        request_id: u64,
        namespace: Namespace,
        track: Bytes,
        params: Params,
    },
    TrackStatus {
        request_id: u64,
        namespace: Namespace,
        track: Bytes,
        params: Params,
    },
    PublishNamespace {
        request_id: u64,
        prefix: Namespace,
        params: Params,
    },
    SubscribeNamespace {
        request_id: u64,
        prefix: Namespace,
        params: Params,
    },
    /// 相手が track を出す（publisher 起点の購読）。
    Publish {
        request_id: u64,
        namespace: Namespace,
        track: Bytes,
        track_alias: u64,
        params: Params,
        properties: Bytes,
    },
    RequestUpdate {
        request_id: u64,
        params: Params,
    },
    /// 相手が要求を取り消した（購読の解除・namespace の取り下げ）。
    RequestCancelled { request_id: u64 },
    SubscribeOk {
        request_id: u64,
        track_alias: u64,
        params: Params,
        properties: Bytes,
    },
    RequestOk {
        request_id: u64,
        params: Params,
        properties: Bytes,
    },
    RequestError {
        request_id: u64,
        code: u64,
        retry_interval: u64,
        reason: String,
    },
    FetchOk {
        request_id: u64,
        end_of_track: bool,
        end: Location,
        properties: Bytes,
    },
    PublishDone {
        request_id: u64,
        status: u64,
        stream_count: u64,
        reason: String,
    },
    PublishStateNotify { request_id: u64, params: Params },
    Namespace { request_id: u64, suffix: Namespace },
    NamespaceDone { request_id: u64, suffix: Namespace },
    Object {
        request_id: u64,
        object: Object,
        delivery: Delivery,
        received: Instant,
    },
    /// subgroup stream が FIN で終わった（この subgroup の object はすべて届いた）。
    SubgroupEnd { request_id: u64, group: u64, subgroup: u64 },
    /// subgroup stream がリセットされた。
    SubgroupReset {
        request_id: u64,
        group: u64,
        subgroup: u64,
        code: u64,
    },
    /// fetch の範囲の印（存在しない・不明・時間切れ）。
    FetchRangeEnd {
        request_id: u64,
        kind: RangeEnd,
        end: Location,
    },
    FetchComplete { request_id: u64 },
    GoAway { uri: Bytes },
    Closed { error: Option<MoqError> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReqKind {
    Subscribe,
    Publish,
    Fetch,
    TrackStatus,
    PublishNamespace,
    SubscribeNamespace,
}

impl ReqKind {
    fn of(m: &Message) -> Option<Self> {
        Some(match m {
            Message::Subscribe { .. } => Self::Subscribe,
            Message::Publish { .. } => Self::Publish,
            Message::Fetch { .. } => Self::Fetch,
            Message::TrackStatus { .. } => Self::TrackStatus,
            Message::PublishNamespace { .. } => Self::PublishNamespace,
            Message::SubscribeNamespace { .. } => Self::SubscribeNamespace,
            _ => return None,
        })
    }
}

/// 要求（自分が出したもの・相手から受けたもの）。
#[derive(Debug)]
struct Req {
    kind: ReqKind,
    /// 自分が出した要求か。
    local: bool,
    /// 送信側の handle。
    out: Handle,
    /// 受信側の transport の stream（自分が出した要求は open の後に決まる）。
    stream: Option<u64>,
    /// 最初の応答を送った・受けた。
    answered: bool,
    /// 購読の track alias（購読者側は SUBSCRIBE_OK・PUBLISH で、出版者側は自分で決める）。
    alias: Option<u64>,
    /// 購読のパラメーター（優先度・群の順序・期限）。
    sub: Option<SubState>,
    /// fetch の群の順序（データ stream の差分の向き）。
    fetch_order: GroupOrder,
    /// 出版者側: 開いたデータ stream の数（PUBLISH_DONE の Stream Count）。
    stream_count: u64,
    /// 既定の出版者優先度（Track Property、無ければ 128）。
    default_priority: u8,
    done: bool,
}

/// 出版者側の購読の状態（期限・優先度、5.1・5.2）。
#[derive(Debug, Clone)]
struct SubState {
    subscriber_priority: u8,
    group_order: GroupOrder,
    object_timeout: Option<Duration>,
    subgroup_timeout: Option<Duration>,
    forward: bool,
    /// 開いている subgroup stream。
    subgroups: HashMap<(u64, u64), SubgroupOut>,
    /// 期限切れで打ち切った subgroup（同じ subgroup の stream を開き直さない）。
    dead: HashSet<(u64, u64)>,
    /// fetch の送信の差分の基準。
    fetch_prior: FetchPrior,
    fetch_out: Option<Handle>,
}

#[derive(Debug, Clone)]
struct SubgroupOut {
    handle: Handle,
    last: Option<u64>,
    /// 見出しで PROPERTIES を立てたか（最初の object に Properties があるときだけ立てる）。
    props: bool,
}

type Handle = u64;

#[derive(Debug)]
struct Chunk {
    data: Bytes,
    /// これより後に書くなら stream をリセットする（object の期限）。
    deadline: Option<Instant>,
}

/// 送信側の stream。
#[derive(Debug)]
struct OutStream {
    transport: Option<u64>,
    uni: bool,
    queue: VecDeque<Chunk>,
    queued: usize,
    fin: bool,
    fin_sent: bool,
    reset: Option<u64>,
    reset_sent: bool,
    priority: i32,
    priority_dirty: bool,
    /// 自分が出した要求の stream なら、その Request ID（open の後に受信側を結びつける）。
    req: Option<u64>,
    /// FIN の後、この時刻までに全部 ACK されなければリセットする（subgroup の期限）。
    ack_deadline: Option<Instant>,
}

/// 受信側の stream。
#[derive(Debug)]
enum InStream {
    /// 相手の制御 stream。
    Control,
    /// 単方向のデータ stream。
    Data(DataIn),
    /// 双方向の要求 stream。
    Request { reader: MessageReader, req: Option<u64> },
    /// 読み捨てる（取り消した購読・padding）。
    Ignore,
}

#[derive(Debug)]
struct DataIn {
    reader: DataStreamReader,
    header: Option<StreamHeader>,
    req: Option<u64>,
    /// track alias が未知の間に溜めた object。
    pending: Vec<StreamItem>,
}

const PRIO_CONTROL: i32 = i32::MAX;
const PRIO_REQUEST: i32 = i32::MAX - 1;

/// 一つの MoQ セッション。
pub struct Session {
    cfg: SessionConfig,
    control_out: Handle,
    control_in: Option<u64>,
    control_reader: MessageReader,
    peer_setup: Option<SetupOptions>,
    goaway_received: bool,
    goaway_sent: bool,
    out: HashMap<Handle, OutStream>,
    out_by_transport: HashMap<u64, Handle>,
    pending_open: VecDeque<Handle>,
    next_handle: Handle,
    in_streams: HashMap<u64, InStream>,
    next_request_id: u64,
    peer_request_ids: HashSet<u64>,
    reqs: HashMap<u64, Req>,
    /// 購読者側: track alias → Request ID（複数の購読が同じ alias を共有できる、3.1.3）。
    alias_to_req: HashMap<u64, Vec<u64>>,
    unknown_alias_streams: usize,
    pending_datagrams: VecDeque<(u64, data::ObjectData, Instant)>,
    datagrams_out: VecDeque<(Bytes, Option<Instant>)>,
    next_alias: u64,
    events: VecDeque<SessionEvent>,
    closed: Option<Option<MoqError>>,
    close_pending: Option<MoqError>,
    /// 送る STOP_SENDING。
    stops: Vec<u64>,
    /// 最後に渡された時刻（受信の時刻の記録に使う）。
    last_now: Instant,
    /// SETUP の前に届いた要求（6.3: SETUP まで溜める）。
    pre_setup: Vec<(u64, Message)>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("role", &self.cfg.role)
            .field("ready", &self.peer_setup.is_some())
            .field("requests", &self.reqs.len())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// セッションを作り、自分の制御 stream と SETUP を送信待ちに積む。
    pub fn new(cfg: SessionConfig, now: Instant) -> Self {
        let mut s = Self {
            next_request_id: if cfg.role == Role::Client { 0 } else { 1 },
            cfg,
            control_out: 0,
            control_in: None,
            control_reader: MessageReader::default(),
            peer_setup: None,
            goaway_received: false,
            goaway_sent: false,
            out: HashMap::new(),
            out_by_transport: HashMap::new(),
            pending_open: VecDeque::new(),
            next_handle: 1,
            in_streams: HashMap::new(),
            peer_request_ids: HashSet::new(),
            reqs: HashMap::new(),
            alias_to_req: HashMap::new(),
            unknown_alias_streams: 0,
            pending_datagrams: VecDeque::new(),
            datagrams_out: VecDeque::new(),
            next_alias: 1,
            events: VecDeque::new(),
            closed: None,
            close_pending: None,
            stops: Vec::new(),
            last_now: now,
            pre_setup: Vec::new(),
        };
        let h = s.new_out(true, PRIO_CONTROL, None);
        s.control_out = h;
        let setup = SetupOptions {
            path: if s.cfg.role == Role::Client && s.cfg.native_quic { s.cfg.path.clone() } else { None },
            authority: if s.cfg.role == Role::Client && s.cfg.native_quic { s.cfg.authority.clone() } else { None },
            auth_tokens: if s.cfg.role == Role::Client { s.cfg.setup_tokens.clone() } else { Vec::new() },
            implementation: s.cfg.implementation.clone(),
            ..Default::default()
        };
        if let Some(m) = Message::Setup(setup).encode() {
            s.push(h, Bytes::from(m), None);
        }
        s
    }

    pub fn role(&self) -> Role {
        self.cfg.role
    }

    /// 相手の SETUP（セッションが使えるようになった）。
    pub fn peer_setup(&self) -> Option<&SetupOptions> {
        self.peer_setup.as_ref()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    pub fn poll_event(&mut self) -> Option<SessionEvent> {
        self.events.pop_front()
    }

    // ------------------------------------------------------------------ 送信待ち

    fn new_out(&mut self, uni: bool, priority: i32, req: Option<u64>) -> Handle {
        let h = self.next_handle;
        self.next_handle += 1;
        self.out.insert(
            h,
            OutStream {
                transport: None,
                uni,
                queue: VecDeque::new(),
                queued: 0,
                fin: false,
                fin_sent: false,
                reset: None,
                reset_sent: false,
                priority,
                priority_dirty: true,
                req,
                ack_deadline: None,
            },
        );
        self.pending_open.push_back(h);
        h
    }

    /// 相手が開いた双方向 stream の送信側。
    fn bound_out(&mut self, transport: u64, priority: i32) -> Handle {
        if let Some(&h) = self.out_by_transport.get(&transport) {
            return h;
        }
        let h = self.next_handle;
        self.next_handle += 1;
        self.out.insert(
            h,
            OutStream {
                transport: Some(transport),
                uni: false,
                queue: VecDeque::new(),
                queued: 0,
                fin: false,
                fin_sent: false,
                reset: None,
                reset_sent: false,
                priority,
                priority_dirty: true,
                req: None,
                ack_deadline: None,
            },
        );
        self.out_by_transport.insert(transport, h);
        h
    }

    fn push(&mut self, h: Handle, data: Bytes, deadline: Option<Instant>) {
        let max = self.cfg.max_stream_queue;
        if let Some(o) = self.out.get_mut(&h)
            && !o.fin
            && o.reset.is_none()
        {
            if o.queued + data.len() > max {
                // 送れていない量が多すぎる（遅い相手）: この stream を打ち切る
                o.reset = Some(reset::TOO_FAR_BEHIND);
                o.queue.clear();
                o.queued = 0;
                return;
            }
            o.queued += data.len();
            o.queue.push_back(Chunk { data, deadline });
        }
    }

    fn send_msg(&mut self, h: Handle, m: &Message) {
        match m.encode() {
            Some(b) => self.push(h, Bytes::from(b), None),
            None => self.fail(MoqError::new(SessionErrorCode::INTERNAL_ERROR, "制御メッセージが大きすぎる")),
        }
    }

    fn finish_out(&mut self, h: Handle) {
        if let Some(o) = self.out.get_mut(&h) {
            o.fin = true;
        }
    }

    fn reset_out(&mut self, h: Handle, code: u64) {
        if let Some(o) = self.out.get_mut(&h)
            && !o.fin_sent
            && o.reset.is_none()
        {
            o.reset = Some(code);
            o.queue.clear();
            o.queued = 0;
        }
    }

    /// 送信待ちを transport に書く。
    pub fn flush(&mut self, t: &mut dyn Transport, now: Instant) {
        self.last_now = now;
        if let Some(e) = self.close_pending.take() {
            t.close(e.code.0, &e.reason);
            self.events.push_back(SessionEvent::Closed { error: Some(e.clone()) });
            self.closed = Some(Some(e));
        }
        if self.closed.is_some() {
            return;
        }
        for st in self.stops.drain(..) {
            t.stop(st, reset::CANCELLED);
        }
        // stream を開く（上限に達したら残す）
        let mut still = VecDeque::new();
        while let Some(h) = self.pending_open.pop_front() {
            let Some(o) = self.out.get(&h) else { continue };
            if o.reset.is_some() && o.transport.is_none() {
                self.out.remove(&h);
                continue;
            }
            let id = if o.uni { t.open_uni() } else { t.open_bi() };
            match id {
                Some(id) => {
                    let req = o.req;
                    if let Some(o) = self.out.get_mut(&h) {
                        o.transport = Some(id);
                    }
                    self.out_by_transport.insert(id, h);
                    if let Some(r) = req {
                        if let Some(rq) = self.reqs.get_mut(&r) {
                            rq.stream = Some(id);
                        }
                        self.in_streams.insert(
                            id,
                            InStream::Request {
                                reader: MessageReader::default(),
                                req: Some(r),
                            },
                        );
                    }
                }
                None => {
                    still.push_back(h);
                    // 以降も開けない
                    while let Some(h) = self.pending_open.pop_front() {
                        still.push_back(h);
                    }
                }
            }
        }
        self.pending_open = still;
        // 書く（優先度の高い stream から）
        let mut hs: Vec<(i32, Handle)> = self.out.iter().filter(|(_, o)| o.transport.is_some()).map(|(h, o)| (o.priority, *h)).collect();
        hs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut done = Vec::new();
        for (_, h) in hs {
            let Some(o) = self.out.get_mut(&h) else { continue };
            let Some(id) = o.transport else { continue };
            if o.priority_dirty {
                t.set_priority(id, o.priority);
                o.priority_dirty = false;
            }
            if let Some(code) = o.reset {
                if !o.reset_sent {
                    t.reset(id, code);
                    o.reset_sent = true;
                }
                done.push(h);
                continue;
            }
            while let Some(c) = o.queue.front_mut() {
                if c.deadline.is_some_and(|d| now > d) {
                    // 期限切れ: 送らずに stream を打ち切る（5.2）
                    t.reset(id, reset::DELIVERY_TIMEOUT);
                    o.reset = Some(reset::DELIVERY_TIMEOUT);
                    o.reset_sent = true;
                    o.queue.clear();
                    o.queued = 0;
                    break;
                }
                c.deadline = None; // 書き始めたら途中で止めない
                match t.write(id, c.data.clone()) {
                    Ok(n) if n == c.data.len() => {
                        o.queued -= n;
                        o.queue.pop_front();
                    }
                    Ok(n) => {
                        o.queued -= n;
                        let _ = c.data.split_to(n);
                        break;
                    }
                    Err(_) => {
                        o.queue.clear();
                        o.queued = 0;
                        o.reset = Some(reset::INTERNAL_ERROR);
                        o.reset_sent = true;
                        break;
                    }
                }
            }
            if o.reset.is_some() {
                done.push(h);
                continue;
            }
            if o.fin && o.queue.is_empty() && !o.fin_sent {
                t.finish(id);
                o.fin_sent = true;
            }
            if o.fin_sent {
                // subgroup の期限: FIN 後の ACK 待ち（5.2）
                match o.ack_deadline {
                    Some(d) if now > d => {
                        if t.unacked(id).is_some_and(|n| n > 0) {
                            t.reset(id, reset::DELIVERY_TIMEOUT);
                        }
                        done.push(h);
                    }
                    Some(_) => {}
                    None => {
                        if o.uni {
                            done.push(h);
                        }
                    }
                }
            }
        }
        for h in done {
            if let Some(o) = self.out.remove(&h)
                && let Some(id) = o.transport
            {
                self.out_by_transport.remove(&id);
            }
        }
        // datagram
        let max = t.max_datagram_size();
        while let Some((d, deadline)) = self.datagrams_out.pop_front() {
            if deadline.is_some_and(|x| now > x) {
                continue; // 期限切れは捨てる
            }
            if max.is_some_and(|m| d.len() <= m) {
                t.send_datagram(d);
            }
        }
    }

    /// 次に [`Self::handle_timeout`] を呼ぶ時刻（subgroup の ACK 期限）。
    pub fn poll_timeout(&self) -> Option<Instant> {
        self.out.values().filter_map(|o| o.ack_deadline).min()
    }

    pub fn handle_timeout(&mut self, _now: Instant) {
        // 期限の処理は flush で行う（transport の ACK の状態が要るため）
    }

    fn fail(&mut self, e: MoqError) {
        if self.closed.is_none() && self.close_pending.is_none() {
            self.close_pending = Some(e);
        }
    }

    /// セッションを閉じる。
    pub fn close(&mut self, code: SessionErrorCode, reason: &str) {
        self.fail(MoqError::new(code, reason));
    }

    // ------------------------------------------------------------------ 受信

    /// transport が閉じた。
    pub fn on_transport_closed(&mut self, why: String) {
        if self.closed.is_none() {
            let e = MoqError::new(SessionErrorCode::NO_ERROR, why);
            self.closed = Some(Some(e.clone()));
            self.events.push_back(SessionEvent::Closed { error: Some(e) });
        }
    }

    /// stream のデータ。`bidi` は双方向 stream か。
    pub fn on_stream_data(&mut self, stream: u64, bidi: bool, data: Bytes, fin: bool, now: Instant) {
        if self.closed.is_some() || self.close_pending.is_some() {
            return;
        }
        self.last_now = now;
        if let Err(e) = self.stream_data(stream, bidi, data, fin, now) {
            self.fail(e);
        }
    }

    fn stream_data(&mut self, stream: u64, bidi: bool, data: Bytes, fin: bool, now: Instant) -> Result<(), MoqError> {
        if !self.in_streams.contains_key(&stream) {
            if bidi {
                if self.peer_setup.is_none() && self.control_in.is_none() {
                    // SETUP の前の要求は溜めない（6.3 は溜めてもよいとするが、資源を守る）
                }
                self.in_streams.insert(
                    stream,
                    InStream::Request {
                        reader: MessageReader::default(),
                        req: None,
                    },
                );
            } else {
                self.in_streams.insert(
                    stream,
                    InStream::Data(DataIn {
                        reader: DataStreamReader::new(self.cfg.max_object_size),
                        header: None,
                        req: None,
                        pending: Vec::new(),
                    }),
                );
            }
        }
        match self.in_streams.get_mut(&stream) {
            Some(InStream::Control) => {
                self.control_reader.push(&data);
                if fin {
                    return Err(MoqError::proto("制御 stream が閉じた"));
                }
                self.read_control()
            }
            Some(InStream::Request { reader, .. }) => {
                reader.push(&data);
                self.read_request_stream(stream, fin)
            }
            Some(InStream::Data(d)) => {
                d.reader.push(&data);
                self.read_data_stream(stream, fin, now)
            }
            Some(InStream::Ignore) | None => Ok(()),
        }
    }

    fn read_control(&mut self) -> Result<(), MoqError> {
        while let Some((t, body)) = self.control_reader.next()? {
            let m = Message::decode_body(t, body, self.cfg.role == Role::Client)?;
            match m {
                Message::Setup(opts) => {
                    if self.peer_setup.is_some() {
                        return Err(MoqError::proto("二度目の SETUP"));
                    }
                    if self.cfg.role == Role::Server && !self.cfg.native_quic {
                        if opts.path.is_some() {
                            return Err(MoqError::new(SessionErrorCode::INVALID_PATH, "WebTransport で PATH"));
                        }
                        if opts.authority.is_some() {
                            return Err(MoqError::new(SessionErrorCode::INVALID_AUTHORITY, "WebTransport で AUTHORITY"));
                        }
                    }
                    self.peer_setup = Some(opts.clone());
                    self.events.push_back(SessionEvent::Ready { peer: opts });
                    for (stream, m) in std::mem::take(&mut self.pre_setup) {
                        self.on_new_request(stream, m)?;
                        // 溜めている間に届いた後続のメッセージ
                        self.read_request_stream(stream, false)?;
                    }
                }
                Message::GoAway { uri, .. } => {
                    if self.peer_setup.is_none() {
                        return Err(MoqError::proto("SETUP の前の GOAWAY"));
                    }
                    if self.goaway_received {
                        return Err(MoqError::proto("二度目の GOAWAY"));
                    }
                    self.goaway_received = true;
                    self.events.push_back(SessionEvent::GoAway { uri });
                }
                _ => return Err(MoqError::proto(format!("制御 stream に使えないメッセージ 0x{t:x}"))),
            }
        }
        Ok(())
    }

    fn read_request_stream(&mut self, stream: u64, fin: bool) -> Result<(), MoqError> {
        loop {
            if self.pre_setup.iter().any(|(s, _)| *s == stream) {
                // SETUP 待ちの要求の stream は、SETUP の後でまとめて読む
                return Ok(());
            }
            let (next, req) = match self.in_streams.get_mut(&stream) {
                Some(InStream::Request { reader, req }) => (reader.next()?, *req),
                _ => return Ok(()),
            };
            let Some((t, body)) = next else { break };
            let m = Message::decode_body(t, body, self.cfg.role == Role::Client)?;
            match req {
                None => self.on_new_request(stream, m)?,
                Some(r) => self.on_request_message(r, stream, m)?,
            }
        }
        if fin {
            let (empty, req) = match self.in_streams.get(&stream) {
                Some(InStream::Request { reader, req }) => (reader.is_empty(), *req),
                _ => return Ok(()),
            };
            if !empty {
                return Err(MoqError::proto("要求 stream がメッセージの途中で閉じた"));
            }
            self.in_streams.remove(&stream);
            if let Some(r) = req {
                self.on_request_fin(r);
            }
        }
        Ok(())
    }

    fn on_new_request(&mut self, stream: u64, m: Message) -> Result<(), MoqError> {
        let Some(kind) = ReqKind::of(&m) else {
            if matches!(m, Message::SubscribeTracks { .. }) {
                // SUBSCRIBE_TRACKS には対応しない（NOT_SUPPORTED で拒否する、1.6）
                let h = self.bound_out(stream, PRIO_REQUEST);
                let e = Message::RequestError {
                    code: RequestErrorCode::NOT_SUPPORTED.0,
                    retry_interval: 0,
                    reason: "SUBSCRIBE_TRACKS is not supported".into(),
                    redirect: None,
                };
                self.send_msg(h, &e);
                self.finish_out(h);
                self.in_streams.insert(stream, InStream::Ignore);
                return Ok(());
            }
            return Err(MoqError::proto("要求 stream の最初が要求でない"));
        };
        if self.peer_setup.is_none() {
            // 制御 stream より先に届いた要求は SETUP まで溜める（6.3）
            if self.pre_setup.len() >= 16 {
                return Err(MoqError::proto("SETUP の前の要求が多すぎる"));
            }
            self.pre_setup.push((stream, m));
            return Ok(());
        }
        let id = m.request_id().unwrap_or(0);
        let peer_parity = if self.cfg.role == Role::Client { 1 } else { 0 };
        if id % 2 != peer_parity || self.peer_request_ids.contains(&id) {
            return Err(MoqError::new(SessionErrorCode::INVALID_REQUEST_ID, format!("不正な Request ID {id}")));
        }
        if self.reqs.values().filter(|r| !r.local).count() >= self.cfg.max_peer_requests {
            return Err(MoqError::proto("相手の要求が多すぎる"));
        }
        if self.peer_request_ids.len() < 1_000_000 {
            self.peer_request_ids.insert(id);
        }
        let h = self.bound_out(stream, PRIO_REQUEST);
        if let Some(InStream::Request { req, .. }) = self.in_streams.get_mut(&stream) {
            *req = Some(id);
        }
        let mut r = Req {
            kind: kind.clone(),
            local: false,
            out: h,
            stream: Some(stream),
            answered: false,
            alias: None,
            sub: None,
            fetch_order: GroupOrder::Ascending,
            stream_count: 0,
            default_priority: 128,
            done: false,
        };
        let ev = match m {
            Message::Subscribe {
                request_id,
                namespace,
                track,
                params,
            } => {
                if !params.range_filters.is_empty() {
                    self.reqs.insert(id, r);
                    self.request_error(id, RequestErrorCode::INVALID_FILTER, "range filters are not supported");
                    return Ok(());
                }
                r.sub = Some(SubState::from_params(&params));
                SessionEvent::Subscribe {
                    request_id,
                    namespace,
                    track,
                    params,
                }
            }
            Message::Fetch {
                request_id,
                namespace,
                track,
                params,
            } => {
                r.fetch_order = params.group_order.unwrap_or(GroupOrder::Ascending);
                let mut st = SubState::from_params(&params);
                st.group_order = r.fetch_order;
                r.sub = Some(st);
                if !params.range_filters.is_empty() {
                    self.reqs.insert(id, r);
                    self.request_error(id, RequestErrorCode::INVALID_FILTER, "range filters are not supported");
                    return Ok(());
                }
                SessionEvent::Fetch {
                    request_id,
                    namespace,
                    track,
                    params,
                }
            }
            Message::TrackStatus {
                request_id,
                namespace,
                track,
                params,
            } => SessionEvent::TrackStatus {
                request_id,
                namespace,
                track,
                params,
            },
            Message::PublishNamespace { request_id, prefix, params } => SessionEvent::PublishNamespace {
                request_id,
                prefix,
                params,
            },
            Message::SubscribeNamespace { request_id, prefix, params } => SessionEvent::SubscribeNamespace {
                request_id,
                prefix,
                params,
            },
            Message::Publish {
                request_id,
                namespace,
                track,
                track_alias,
                params,
                properties,
            } => {
                // 同じ alias を別の track に使ってはならない（3.1.3）
                if let Some(rs) = self.alias_to_req.get(&track_alias)
                    && rs.iter().any(|x| self.reqs.get(x).is_some_and(|q| !q.done))
                {
                    return Err(MoqError::new(SessionErrorCode::DUPLICATE_TRACK_ALIAS, "track alias の重複"));
                }
                let tp = TrackProperties::parse(properties.clone())?;
                r.alias = Some(track_alias);
                r.default_priority = tp.default_publisher_priority.unwrap_or(128);
                self.alias_to_req.entry(track_alias).or_default().push(request_id);
                SessionEvent::Publish {
                    request_id,
                    namespace,
                    track,
                    track_alias,
                    params,
                    properties,
                }
            }
            _ => return Err(MoqError::proto("要求 stream の最初が要求でない")),
        };
        self.reqs.insert(id, r);
        self.events.push_back(ev);
        if matches!(kind, ReqKind::Publish) {
            self.release_pending_alias(self.last_now);
        }
        Ok(())
    }

    fn on_request_message(&mut self, id: u64, _stream: u64, m: Message) -> Result<(), MoqError> {
        let Some(r) = self.reqs.get_mut(&id) else { return Ok(()) };
        let first = !r.answered;
        match m {
            Message::GoAway { .. } => {
                // 要求単位の移行の合図。ここでは通知だけ（9.2）
                self.events.push_back(SessionEvent::GoAway { uri: Bytes::new() });
            }
            Message::RequestUpdate { request_id, params } => {
                // 要求を出した側（PUBLISH は購読者）だけが更新できる（9.5）
                let allowed = !r.local || r.kind == ReqKind::Publish;
                if !allowed {
                    return Err(MoqError::proto("想定外の REQUEST_UPDATE"));
                }
                // REQUEST_UPDATE は新しい Request ID を使う（6.4.2.1）
                let peer_parity = if self.cfg.role == Role::Client { 1 } else { 0 };
                if request_id % 2 != peer_parity || self.peer_request_ids.contains(&request_id) {
                    return Err(MoqError::new(SessionErrorCode::INVALID_REQUEST_ID, format!("不正な Request ID {request_id}")));
                }
                if self.peer_request_ids.len() < 1_000_000 {
                    self.peer_request_ids.insert(request_id);
                }
                if let Some(r) = self.reqs.get_mut(&id)
                    && let Some(s) = r.sub.as_mut()
                {
                    s.update(&params);
                }
                self.events.push_back(SessionEvent::RequestUpdate { request_id: id, params });
            }
            Message::SubscribeOk {
                track_alias,
                params,
                properties,
            } => {
                if !r.local || r.kind != ReqKind::Subscribe || !first {
                    return Err(MoqError::proto("想定外の SUBSCRIBE_OK"));
                }
                if let Some(rs) = self.alias_to_req.get(&track_alias)
                    && rs.iter().any(|x| *x != id && self.reqs.get(x).is_some_and(|q| !q.done && q.alias == Some(track_alias)))
                {
                    // 同じ track への別の購読と共有するのは許される。ここでは名前を比べられないので許す
                }
                let tp = TrackProperties::parse(properties.clone())?;
                let r = self.reqs.get_mut(&id).ok_or_else(|| MoqError::proto("要求"))?;
                r.answered = true;
                r.alias = Some(track_alias);
                r.default_priority = tp.default_publisher_priority.unwrap_or(128);
                let v = self.alias_to_req.entry(track_alias).or_default();
                if !v.contains(&id) {
                    v.push(id);
                }
                self.events.push_back(SessionEvent::SubscribeOk {
                    request_id: id,
                    track_alias,
                    params,
                    properties,
                });
                self.release_pending_alias(self.last_now);
            }
            Message::FetchOk {
                end_of_track,
                end,
                properties,
                ..
            } => {
                if !r.local || r.kind != ReqKind::Fetch || !first {
                    return Err(MoqError::proto("想定外の FETCH_OK"));
                }
                r.answered = true;
                self.events.push_back(SessionEvent::FetchOk {
                    request_id: id,
                    end_of_track,
                    end,
                    properties,
                });
            }
            Message::RequestOk { params, properties } => {
                // 要求への応答、または REQUEST_UPDATE への応答
                if r.local && first && matches!(r.kind, ReqKind::Subscribe | ReqKind::Fetch) {
                    return Err(MoqError::proto("SUBSCRIBE・FETCH への REQUEST_OK"));
                }
                if first && r.kind != ReqKind::TrackStatus && !properties.is_empty() {
                    return Err(MoqError::proto("REQUEST_OK に Track Properties"));
                }
                r.answered = true;
                self.events.push_back(SessionEvent::RequestOk {
                    request_id: id,
                    params,
                    properties,
                });
            }
            Message::RequestError {
                code,
                retry_interval,
                reason,
                ..
            } => {
                r.answered = true;
                if first {
                    r.done = true;
                }
                self.events.push_back(SessionEvent::RequestError {
                    request_id: id,
                    code,
                    retry_interval,
                    reason,
                });
            }
            Message::PublishDone {
                status,
                stream_count,
                reason,
            } => {
                let ok = (r.local && r.kind == ReqKind::Subscribe) || (!r.local && r.kind == ReqKind::Publish);
                if !ok {
                    return Err(MoqError::proto("想定外の PUBLISH_DONE"));
                }
                r.done = true;
                self.events.push_back(SessionEvent::PublishDone {
                    request_id: id,
                    status,
                    stream_count,
                    reason,
                });
            }
            Message::PublishStateNotify { params } => {
                let ok = (r.local && r.kind == ReqKind::Subscribe) || (!r.local && r.kind == ReqKind::Publish);
                if !ok {
                    return Err(MoqError::proto("想定外の PUBLISH_STATE_NOTIFY"));
                }
                self.events.push_back(SessionEvent::PublishStateNotify { request_id: id, params });
            }
            Message::Namespace { .. } | Message::NamespaceDone { .. } if r.kind != ReqKind::SubscribeNamespace => {
                return Err(MoqError::proto("SUBSCRIBE_NAMESPACE 以外に NAMESPACE"));
            }
            Message::Namespace { suffix } => self.events.push_back(SessionEvent::Namespace { request_id: id, suffix }),
            Message::NamespaceDone { suffix } => self.events.push_back(SessionEvent::NamespaceDone { request_id: id, suffix }),
            other => return Err(MoqError::proto(format!("要求 stream に想定外のメッセージ 0x{:x}", other.type_id()))),
        }
        Ok(())
    }

    fn on_request_fin(&mut self, id: u64) {
        // 相手がこの方向を閉じた。応答前なら失敗とみなす（6.4.2.2）
        let Some(r) = self.reqs.get_mut(&id) else { return };
        if r.local && !r.answered {
            r.done = true;
            self.events.push_back(SessionEvent::RequestError {
                request_id: id,
                code: RequestErrorCode::INTERNAL_ERROR.0,
                retry_interval: 0,
                reason: "request stream closed".into(),
            });
        }
        if r.local && r.kind == ReqKind::SubscribeNamespace || !r.local && r.kind == ReqKind::PublishNamespace {
            // namespace の購読・公開の終了
            self.events.push_back(SessionEvent::RequestCancelled { request_id: id });
        }
    }

    /// stream がリセットされた。
    pub fn on_stream_reset(&mut self, stream: u64, code: u64) {
        match self.in_streams.remove(&stream) {
            Some(InStream::Control) => self.fail(MoqError::proto("制御 stream がリセットされた")),
            Some(InStream::Request { req: Some(r), .. }) => self.cancelled(r),
            Some(InStream::Data(d)) => {
                let had_header = d.header.is_some();
                if let (Some(r), Some(StreamHeader::Subgroup(h))) = (d.req, d.header) {
                    let sg = match h.subgroup {
                        SubgroupId::Explicit(s) => s,
                        _ => 0,
                    };
                    self.events.push_back(SessionEvent::SubgroupReset {
                        request_id: r,
                        group: h.group,
                        subgroup: sg,
                        code,
                    });
                } else if let Some(r) = d.req {
                    self.events.push_back(SessionEvent::FetchComplete { request_id: r });
                }
                if d.req.is_none() && had_header {
                    self.unknown_alias_streams = self.unknown_alias_streams.saturating_sub(1);
                }
            }
            _ => {}
        }
    }

    /// 相手が STOP_SENDING を送った。
    pub fn on_stop_sending(&mut self, stream: u64, code: u64) {
        let Some(&h) = self.out_by_transport.get(&stream) else { return };
        if h == self.control_out {
            self.fail(MoqError::proto("制御 stream に STOP_SENDING"));
            return;
        }
        // 要求の取り消し（購読の解除など）
        let req = self.reqs.iter().find(|(_, r)| r.out == h).map(|(id, _)| *id);
        if let Some(r) = req {
            self.cancelled(r);
            return;
        }
        // データ stream: その subgroup を止める
        let _ = code;
        self.reset_out(h, reset::CANCELLED);
        for r in self.reqs.values_mut() {
            if let Some(s) = r.sub.as_mut() {
                let dead: Vec<(u64, u64)> = s.subgroups.iter().filter(|(_, g)| g.handle == h).map(|(k, _)| *k).collect();
                for k in dead {
                    s.subgroups.remove(&k);
                    s.dead.insert(k);
                }
            }
        }
    }

    fn cancelled(&mut self, id: u64) {
        let Some(r) = self.reqs.remove(&id) else { return };
        let h = r.out;
        self.reset_out(h, reset::CANCELLED);
        if let Some(s) = &r.sub {
            for g in s.subgroups.values() {
                self.reset_out(g.handle, reset::CANCELLED);
            }
            if let Some(f) = s.fetch_out {
                self.reset_out(f, reset::CANCELLED);
            }
        }
        if let Some(st) = r.stream {
            self.in_streams.insert(st, InStream::Ignore);
        }
        if let Some(a) = r.alias
            && let Some(v) = self.alias_to_req.get_mut(&a)
        {
            v.retain(|x| *x != id);
        }
        self.events.push_back(SessionEvent::RequestCancelled { request_id: id });
    }

    fn read_data_stream(&mut self, stream: u64, fin: bool, now: Instant) -> Result<(), MoqError> {
        loop {
            let item = match self.in_streams.get_mut(&stream) {
                Some(InStream::Data(d)) => d.reader.next_item()?,
                _ => return Ok(()),
            };
            let Some(item) = item else { break };
            match &item {
                StreamItem::Header(StreamHeader::Control) => {
                    if self.control_in.is_some() {
                        return Err(MoqError::proto("二本目の制御 stream"));
                    }
                    self.control_in = Some(stream);
                    let rest = match self.in_streams.get_mut(&stream) {
                        Some(InStream::Data(d)) => d.reader.take_control_bytes(),
                        _ => Bytes::new(),
                    };
                    self.in_streams.insert(stream, InStream::Control);
                    self.control_reader.push(&rest);
                    if fin {
                        return Err(MoqError::proto("制御 stream が閉じた"));
                    }
                    return self.read_control();
                }
                StreamItem::Header(StreamHeader::Padding) => {
                    self.in_streams.insert(stream, InStream::Ignore);
                    return Ok(());
                }
                StreamItem::Header(StreamHeader::Subgroup(h)) => {
                    let req = self.req_for_alias(h.track_alias);
                    if let Some(InStream::Data(d)) = self.in_streams.get_mut(&stream) {
                        d.header = Some(StreamHeader::Subgroup(h.clone()));
                        d.req = req;
                    }
                    if req.is_none() {
                        if self.unknown_alias_streams >= self.cfg.max_unknown_alias_streams {
                            // 溜めきれない（3.1.3.1）
                            self.in_streams.insert(stream, InStream::Ignore);
                            self.stop_stream(stream);
                            return Ok(());
                        }
                        self.unknown_alias_streams += 1;
                    }
                }
                StreamItem::Header(StreamHeader::Fetch { request_id }) => {
                    let rq = self.reqs.get(request_id);
                    let ok = rq.is_some_and(|r| r.local && r.kind == ReqKind::Fetch);
                    if !ok {
                        // 取り消した fetch・知らない fetch は読み捨てる
                        self.in_streams.insert(stream, InStream::Ignore);
                        self.stop_stream(stream);
                        return Ok(());
                    }
                    let order = rq.map_or(GroupOrder::Ascending, |r| r.fetch_order);
                    if let Some(InStream::Data(d)) = self.in_streams.get_mut(&stream) {
                        d.header = Some(StreamHeader::Fetch { request_id: *request_id });
                        d.req = Some(*request_id);
                        d.reader.fetch_order = order;
                    }
                }
                StreamItem::Object(_) | StreamItem::RangeEnd { .. } => {
                    let (req, header) = match self.in_streams.get(&stream) {
                        Some(InStream::Data(d)) => (d.req, d.header.clone()),
                        _ => (None, None),
                    };
                    match req {
                        Some(r) => self.deliver_item(r, header.as_ref(), item, now),
                        None => {
                            let max = self.cfg.max_unknown_alias_objects;
                            let mut drop = false;
                            if let Some(InStream::Data(d)) = self.in_streams.get_mut(&stream) {
                                if d.pending.len() >= max {
                                    drop = true;
                                } else {
                                    d.pending.push(item);
                                }
                            }
                            if drop {
                                self.in_streams.insert(stream, InStream::Ignore);
                                self.unknown_alias_streams = self.unknown_alias_streams.saturating_sub(1);
                                self.stop_stream(stream);
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        if fin {
            let Some(InStream::Data(d)) = self.in_streams.remove(&stream) else { return Ok(()) };
            if d.reader.mid_object() {
                return Err(MoqError::proto("データ stream が object の途中で閉じた"));
            }
            match (d.req, d.header) {
                (Some(r), Some(StreamHeader::Subgroup(h))) => {
                    let sg = match h.subgroup {
                        SubgroupId::Explicit(s) => s,
                        SubgroupId::Zero => 0,
                        SubgroupId::FirstObject => d
                            .pending
                            .iter()
                            .find_map(|i| if let StreamItem::Object(o) = i { o.subgroup } else { None })
                            .unwrap_or(0),
                    };
                    self.events.push_back(SessionEvent::SubgroupEnd {
                        request_id: r,
                        group: h.group,
                        subgroup: sg,
                    });
                }
                (Some(r), Some(StreamHeader::Fetch { .. })) => {
                    self.events.push_back(SessionEvent::FetchComplete { request_id: r });
                }
                (None, Some(_)) => {
                    // alias が分からないまま終わった: 溜めた分は捨てる
                    self.unknown_alias_streams = self.unknown_alias_streams.saturating_sub(1);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn stop_stream(&mut self, stream: u64) {
        // STOP_SENDING は flush で transport に書く
        if self.stops.len() < 1024 {
            self.stops.push(stream);
        }
    }

    fn req_for_alias(&self, alias: u64) -> Option<u64> {
        self.alias_to_req.get(&alias)?.iter().copied().find(|r| self.reqs.get(r).is_some_and(|q| !q.done || q.local))
    }

    fn deliver_item(&mut self, req: u64, header: Option<&StreamHeader>, item: StreamItem, now: Instant) {
        let default_priority = self.reqs.get(&req).map_or(128, |r| r.default_priority);
        match item {
            StreamItem::Object(o) => {
                let delivery = match header {
                    Some(StreamHeader::Fetch { .. }) => Delivery::Fetch,
                    _ => Delivery::Subgroup,
                };
                self.events.push_back(SessionEvent::Object {
                    request_id: req,
                    object: Object {
                        group: o.group,
                        subgroup: o.subgroup,
                        id: o.object,
                        priority: o.priority.unwrap_or(default_priority),
                        status: o.status,
                        properties: o.properties,
                        payload: o.payload,
                    },
                    delivery,
                    received: now,
                });
            }
            StreamItem::RangeEnd { kind, group, object } => self.events.push_back(SessionEvent::FetchRangeEnd {
                request_id: req,
                kind,
                end: Location::new(group, object),
            }),
            StreamItem::Header(_) => {}
        }
    }

    /// alias が分かった購読の、溜めていた stream・datagram を流す。
    fn release_pending_alias(&mut self, now: Instant) {
        let ids: Vec<u64> = self
            .in_streams
            .iter()
            .filter_map(|(id, s)| match s {
                InStream::Data(d) if d.req.is_none() && matches!(d.header, Some(StreamHeader::Subgroup(_))) => Some(*id),
                _ => None,
            })
            .collect();
        for id in ids {
            let alias = match self.in_streams.get(&id) {
                Some(InStream::Data(DataIn {
                    header: Some(StreamHeader::Subgroup(h)),
                    ..
                })) => h.track_alias,
                _ => continue,
            };
            let Some(req) = self.req_for_alias(alias) else { continue };
            let (pending, header) = match self.in_streams.get_mut(&id) {
                Some(InStream::Data(d)) => {
                    d.req = Some(req);
                    (std::mem::take(&mut d.pending), d.header.clone())
                }
                _ => continue,
            };
            self.unknown_alias_streams = self.unknown_alias_streams.saturating_sub(1);
            for it in pending {
                self.deliver_item(req, header.as_ref(), it, now);
            }
        }
        let pend: Vec<_> = self.pending_datagrams.drain(..).collect();
        for (alias, o, at) in pend {
            self.on_datagram_object(alias, o, at, true);
        }
    }

    /// datagram を受けた。
    pub fn on_datagram(&mut self, d: Bytes, now: Instant) {
        self.last_now = now;
        match data::decode_datagram(d) {
            Ok(data::Datagram::Object {
                track_alias, object, ..
            }) => self.on_datagram_object(track_alias, object, now, false),
            Ok(data::Datagram::Padding) => {}
            Err(e) => self.fail(e),
        }
    }

    fn on_datagram_object(&mut self, alias: u64, o: data::ObjectData, now: Instant, replay: bool) {
        match self.req_for_alias(alias) {
            Some(req) => {
                let default_priority = self.reqs.get(&req).map_or(128, |r| r.default_priority);
                self.events.push_back(SessionEvent::Object {
                    request_id: req,
                    object: Object {
                        group: o.group,
                        subgroup: None,
                        id: o.object,
                        priority: o.priority.unwrap_or(default_priority),
                        status: o.status,
                        properties: o.properties,
                        payload: o.payload,
                    },
                    delivery: Delivery::Datagram,
                    received: now,
                });
            }
            None if !replay && self.pending_datagrams.len() < 64 => {
                self.pending_datagrams.push_back((alias, o, now));
            }
            None => {}
        }
    }

    // ------------------------------------------------------------------ 要求（自分から）

    fn alloc_request(&mut self) -> Result<u64, MoqError> {
        if self.closed.is_some() {
            return Err(MoqError::new(SessionErrorCode::INTERNAL_ERROR, "セッションが閉じている"));
        }
        let id = self.next_request_id;
        self.next_request_id += 2;
        Ok(id)
    }

    fn start_request(&mut self, id: u64, kind: ReqKind, m: &Message, sub: Option<SubState>) {
        let h = self.new_out(false, PRIO_REQUEST, Some(id));
        self.reqs.insert(
            id,
            Req {
                kind,
                local: true,
                out: h,
                stream: None,
                answered: false,
                alias: None,
                sub,
                fetch_order: GroupOrder::Ascending,
                stream_count: 0,
                default_priority: 128,
                done: false,
            },
        );
        self.send_msg(h, m);
    }

    /// SUBSCRIBE を送る。Request ID を返す。
    pub fn subscribe(&mut self, args: SubscribeArgs) -> Result<u64, MoqError> {
        let id = self.alloc_request()?;
        let m = Message::Subscribe {
            request_id: id,
            namespace: args.namespace,
            track: args.track,
            params: args.params,
        };
        self.start_request(id, ReqKind::Subscribe, &m, None);
        Ok(id)
    }

    /// FETCH を送る。
    pub fn fetch(&mut self, namespace: Namespace, track: Bytes, params: Params) -> Result<u64, MoqError> {
        let id = self.alloc_request()?;
        let order = params.group_order.unwrap_or(GroupOrder::Ascending);
        let m = Message::Fetch {
            request_id: id,
            namespace,
            track,
            params,
        };
        self.start_request(id, ReqKind::Fetch, &m, None);
        if let Some(r) = self.reqs.get_mut(&id) {
            r.fetch_order = order;
        }
        Ok(id)
    }

    pub fn track_status(&mut self, namespace: Namespace, track: Bytes, params: Params) -> Result<u64, MoqError> {
        let id = self.alloc_request()?;
        let m = Message::TrackStatus {
            request_id: id,
            namespace,
            track,
            params,
        };
        self.start_request(id, ReqKind::TrackStatus, &m, None);
        Ok(id)
    }

    pub fn publish_namespace(&mut self, prefix: Namespace, params: Params) -> Result<u64, MoqError> {
        let id = self.alloc_request()?;
        let m = Message::PublishNamespace {
            request_id: id,
            prefix,
            params,
        };
        self.start_request(id, ReqKind::PublishNamespace, &m, None);
        Ok(id)
    }

    pub fn subscribe_namespace(&mut self, prefix: Namespace, params: Params) -> Result<u64, MoqError> {
        let id = self.alloc_request()?;
        let m = Message::SubscribeNamespace {
            request_id: id,
            prefix,
            params,
        };
        self.start_request(id, ReqKind::SubscribeNamespace, &m, None);
        Ok(id)
    }

    /// PUBLISH を送る（出版者起点の購読）。`(Request ID, track alias)` を返す。
    pub fn publish(&mut self, namespace: Namespace, track: Bytes, params: Params, properties: Bytes) -> Result<(u64, u64), MoqError> {
        let id = self.alloc_request()?;
        let alias = self.next_alias;
        self.next_alias += 1;
        let tp = TrackProperties::parse(properties.clone())?;
        let mut st = SubState::from_params(&params);
        st.apply_track(&tp);
        let m = Message::Publish {
            request_id: id,
            namespace,
            track,
            track_alias: alias,
            params,
            properties,
        };
        self.start_request(id, ReqKind::Publish, &m, Some(st));
        if let Some(r) = self.reqs.get_mut(&id) {
            r.alias = Some(alias);
            r.default_priority = tp.default_publisher_priority.unwrap_or(128);
        }
        Ok((id, alias))
    }

    /// REQUEST_UPDATE を送る（新しい Request ID を使う、6.4.2.1）。応答は元の要求の Request ID の
    /// REQUEST_OK・REQUEST_ERROR イベントで届く。
    pub fn request_update(&mut self, request_id: u64, params: Params) -> Result<(), MoqError> {
        let r = self.reqs.get(&request_id).ok_or_else(|| MoqError::proto("未知の要求"))?;
        let allowed = r.local || r.kind == ReqKind::Publish;
        if !allowed {
            return Err(MoqError::proto("この要求は更新できない"));
        }
        let h = r.out;
        let new_id = self.alloc_request()?;
        self.send_msg(h, &Message::RequestUpdate { request_id: new_id, params });
        Ok(())
    }

    /// 要求を取り消す（購読の解除・namespace の取り下げ、6.4.2.3）。
    pub fn cancel(&mut self, request_id: u64) {
        let Some(r) = self.reqs.remove(&request_id) else { return };
        self.reset_out(r.out, reset::CANCELLED);
        if let Some(st) = r.stream {
            self.stops.push(st);
            self.in_streams.insert(st, InStream::Ignore);
        }
        if let Some(a) = r.alias
            && let Some(v) = self.alias_to_req.get_mut(&a)
        {
            v.retain(|x| *x != request_id);
        }
        // 受信中のデータ stream も止める
        let data: Vec<u64> = self
            .in_streams
            .iter()
            .filter_map(|(id, s)| match s {
                InStream::Data(d) if d.req == Some(request_id) => Some(*id),
                _ => None,
            })
            .collect();
        for id in data {
            self.in_streams.insert(id, InStream::Ignore);
            self.stops.push(id);
        }
    }

    /// GOAWAY を送る（server は新しい URI を入れられる）。
    pub fn goaway(&mut self, uri: &str, timeout_ms: u64) {
        if self.goaway_sent {
            return;
        }
        self.goaway_sent = true;
        let uri = if self.cfg.role == Role::Server { uri } else { "" };
        let h = self.control_out;
        self.send_msg(
            h,
            &Message::GoAway {
                uri: Bytes::copy_from_slice(uri.as_bytes()),
                timeout_ms,
            },
        );
    }

    // ------------------------------------------------------------------ 応答

    fn answer(&mut self, request_id: u64) -> Result<Handle, MoqError> {
        let r = self.reqs.get_mut(&request_id).ok_or_else(|| MoqError::proto("未知の要求"))?;
        if r.local {
            return Err(MoqError::proto("自分の要求には応答しない"));
        }
        r.answered = true;
        Ok(r.out)
    }

    /// SUBSCRIBE を受け付ける。track alias を返す。
    pub fn subscribe_ok(&mut self, request_id: u64, largest: Option<Location>, properties: Bytes) -> Result<u64, MoqError> {
        let tp = TrackProperties::parse(properties.clone())?;
        let alias = self.next_alias;
        self.next_alias += 1;
        let h = self.answer(request_id)?;
        if let Some(r) = self.reqs.get_mut(&request_id) {
            r.alias = Some(alias);
            r.default_priority = tp.default_publisher_priority.unwrap_or(128);
            if let Some(s) = r.sub.as_mut() {
                s.apply_track(&tp);
            }
        }
        let m = Message::SubscribeOk {
            track_alias: alias,
            params: Params {
                largest_object: largest,
                ..Default::default()
            },
            properties,
        };
        self.send_msg(h, &m);
        Ok(alias)
    }

    /// REQUEST_OK（PUBLISH・PUBLISH_NAMESPACE・SUBSCRIBE_NAMESPACE・TRACK_STATUS・REQUEST_UPDATE への応答）。
    pub fn request_ok(&mut self, request_id: u64, params: Params, properties: Bytes) -> Result<(), MoqError> {
        let h = self.answer(request_id)?;
        self.send_msg(h, &Message::RequestOk { params, properties });
        if self.reqs.get(&request_id).is_some_and(|r| r.kind == ReqKind::TrackStatus) {
            self.finish_out(h);
        }
        Ok(())
    }

    /// REQUEST_ERROR を送って要求を終える。
    pub fn request_error(&mut self, request_id: u64, code: RequestErrorCode, reason: &str) {
        let Ok(h) = self.answer(request_id) else { return };
        self.send_msg(
            h,
            &Message::RequestError {
                code: code.0,
                retry_interval: 0,
                reason: reason.chars().take(256).collect(),
                redirect: None,
            },
        );
        self.finish_out(h);
        if let Some(r) = self.reqs.remove(&request_id)
            && let Some(st) = r.stream
        {
            self.in_streams.insert(st, InStream::Ignore);
        }
    }

    /// FETCH_OK を送る。
    pub fn fetch_ok(&mut self, request_id: u64, end_of_track: bool, end: Location, properties: Bytes) -> Result<(), MoqError> {
        let h = self.answer(request_id)?;
        self.send_msg(
            h,
            &Message::FetchOk {
                end_of_track,
                end,
                params: Params::default(),
                properties,
            },
        );
        Ok(())
    }

    /// SUBSCRIBE_NAMESPACE への NAMESPACE・NAMESPACE_DONE。
    pub fn namespace(&mut self, request_id: u64, suffix: Namespace, done: bool) {
        let Some(r) = self.reqs.get(&request_id) else { return };
        let h = r.out;
        let m = if done {
            Message::NamespaceDone { suffix }
        } else {
            Message::Namespace { suffix }
        };
        self.send_msg(h, &m);
    }

    /// PUBLISH_DONE を送る。開いている subgroup stream を閉じてから（9.9）。
    pub fn publish_done(&mut self, request_id: u64, status: u64, reason: &str) {
        let Some(r) = self.reqs.get_mut(&request_id) else { return };
        let h = r.out;
        let count = r.stream_count;
        let subs: Vec<Handle> = r.sub.as_ref().map(|s| s.subgroups.values().map(|g| g.handle).collect()).unwrap_or_default();
        r.done = true;
        if let Some(s) = r.sub.as_mut() {
            s.subgroups.clear();
        }
        for sh in subs {
            self.finish_out(sh);
        }
        self.send_msg(
            h,
            &Message::PublishDone {
                status,
                stream_count: count,
                reason: reason.chars().take(256).collect(),
            },
        );
        self.finish_out(h);
    }

    /// 要求の購読者の優先度・群の順序など（出版者側）。
    pub fn subscription_priority(&self, request_id: u64) -> Option<(u8, GroupOrder)> {
        let s = self.reqs.get(&request_id)?.sub.as_ref()?;
        Some((s.subscriber_priority, s.group_order))
    }

    /// 購読が一時停止（FORWARD=0）か。
    pub fn is_forwarding(&self, request_id: u64) -> bool {
        self.reqs.get(&request_id).and_then(|r| r.sub.as_ref()).is_none_or(|s| s.forward)
    }

    /// 応答済みの購読（出版者側）か。
    pub fn is_established_publication(&self, request_id: u64) -> bool {
        self.reqs
            .get(&request_id)
            .is_some_and(|r| (r.answered || r.local) && r.alias.is_some() && !r.done && r.sub.is_some())
    }

    // ------------------------------------------------------------------ object の送信（出版者側）

    fn stream_priority(sub: &SubState, publisher_priority: u8, group: u64) -> i32 {
        // MoQ は小さいほど優先、QUIC の stream の優先度は大きいほど先（5.1.2 の近似）
        let g = (group & 0x7fff) as i32;
        let order = match sub.group_order {
            GroupOrder::Ascending => 0x7fff - g,
            GroupOrder::Descending => g,
        };
        let base = (255 - i32::from(sub.subscriber_priority)) << 23 | (255 - i32::from(publisher_priority)) << 15;
        base | order
    }

    /// subgroup で object を送る。`created` は object の見出しを受け取った・作った時刻（OBJECT_DELIVERY_TIMEOUT の基準）。
    pub fn send_object(&mut self, request_id: u64, o: &Object, created: Instant, now: Instant) -> Result<(), MoqError> {
        let max = self.cfg.max_object_size;
        if o.payload.len() > max {
            return Err(MoqError::proto("object が大きすぎる"));
        }
        let Some(r) = self.reqs.get_mut(&request_id) else { return Ok(()) };
        let Some(alias) = r.alias else { return Ok(()) };
        let default_priority = r.default_priority;
        let Some(sub) = r.sub.as_mut() else { return Ok(()) };
        if !sub.forward || r.done {
            return Ok(());
        }
        let sg = o.subgroup.unwrap_or(0);
        let key = (o.group, sg);
        if sub.dead.contains(&key) {
            return Ok(());
        }
        let deadline = sub.object_timeout.map(|d| created + d);
        if deadline.is_some_and(|d| now > d) {
            // もう間に合わない: stream があれば打ち切り、以後この subgroup は送らない
            if let Some(g) = sub.subgroups.remove(&key) {
                let hh = g.handle;
                sub.dead.insert(key);
                self.reset_out(hh, reset::DELIVERY_TIMEOUT);
            } else {
                sub.dead.insert(key);
            }
            return Ok(());
        }
        let prio = Self::stream_priority(sub, o.priority, o.group);
        let existing = sub.subgroups.get(&key).cloned();
        let (handle, last, props) = match existing {
            Some(g) if g.last.is_none_or(|l| l < o.id) && (g.props || o.properties.is_empty()) => (g.handle, g.last, g.props),
            Some(g) => {
                // 順序が戻った・Properties を書けない: stream を打ち切って開き直す（11.3.2）
                self.reset_out(g.handle, reset::CANCELLED);
                let Some(r) = self.reqs.get_mut(&request_id) else { return Ok(()) };
                let Some(sub) = r.sub.as_mut() else { return Ok(()) };
                sub.subgroups.remove(&key);
                self.open_subgroup(request_id, alias, o, prio, default_priority)
            }
            None => self.open_subgroup(request_id, alias, o, prio, default_priority),
        };
        let hdr = encode_subgroup_object(last, o.id, props, &o.properties, o.status, o.payload.len());
        self.push(handle, Bytes::from(hdr), deadline);
        if !o.payload.is_empty() {
            self.push(handle, o.payload.clone(), None);
        }
        if let Some(r) = self.reqs.get_mut(&request_id)
            && let Some(sub) = r.sub.as_mut()
            && let Some(g) = sub.subgroups.get_mut(&key)
        {
            g.last = Some(o.id);
        }
        if o.status != ObjectStatus::Normal {
            self.end_subgroup(request_id, o.group, sg, now);
        }
        Ok(())
    }

    fn open_subgroup(&mut self, request_id: u64, alias: u64, o: &Object, prio: i32, default_priority: u8) -> (Handle, Option<u64>, bool) {
        let h = self.new_out(true, prio, None);
        // 空の Properties の欄は送らない（PROPERTIES は最初の object が持つときだけ立てる）
        let props = !o.properties.is_empty();
        let header = SubgroupHeader {
            track_alias: alias,
            group: o.group,
            subgroup: SubgroupId::Explicit(o.subgroup.unwrap_or(0)),
            priority: if o.priority == default_priority { None } else { Some(o.priority) },
            has_properties: props,
            end_of_group: false,
            first_object: false,
        };
        self.push(h, Bytes::from(header.encode()), None);
        if let Some(r) = self.reqs.get_mut(&request_id) {
            r.stream_count += 1;
            if let Some(sub) = r.sub.as_mut() {
                sub.subgroups.insert((o.group, o.subgroup.unwrap_or(0)), SubgroupOut { handle: h, last: None, props });
            }
        }
        (h, None, props)
    }

    /// subgroup を閉じる（FIN）。SUBGROUP_DELIVERY_TIMEOUT があれば ACK の期限を張る。
    pub fn end_subgroup(&mut self, request_id: u64, group: u64, subgroup: u64, now: Instant) {
        let Some(r) = self.reqs.get_mut(&request_id) else { return };
        let Some(sub) = r.sub.as_mut() else { return };
        let timeout = sub.subgroup_timeout;
        if let Some(g) = sub.subgroups.remove(&(group, subgroup))
            && let Some(o) = self.out.get_mut(&g.handle)
        {
            o.fin = true;
            o.ack_deadline = timeout.map(|t| now + t);
        }
    }

    /// subgroup を打ち切る（RESET_STREAM）。
    pub fn reset_subgroup(&mut self, request_id: u64, group: u64, subgroup: u64, code: u64) {
        let Some(r) = self.reqs.get_mut(&request_id) else { return };
        let Some(sub) = r.sub.as_mut() else { return };
        if let Some(g) = sub.subgroups.remove(&(group, subgroup)) {
            let h = g.handle;
            sub.dead.insert((group, subgroup));
            self.reset_out(h, code);
        }
    }

    /// datagram で object を送る。
    pub fn send_object_datagram(&mut self, request_id: u64, o: &Object, created: Instant) {
        let Some(r) = self.reqs.get(&request_id) else { return };
        let Some(alias) = r.alias else { return };
        let Some(sub) = r.sub.as_ref() else { return };
        if !sub.forward {
            return;
        }
        let timeout = match (sub.object_timeout, sub.subgroup_timeout) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let d = data::ObjectData {
            group: o.group,
            subgroup: None,
            object: o.id,
            priority: if o.priority == r.default_priority { None } else { Some(o.priority) },
            properties: o.properties.clone(),
            status: o.status,
            payload: o.payload.clone(),
        };
        let enc = data::encode_datagram(alias, &d, o.status == ObjectStatus::EndOfGroup);
        if self.datagrams_out.len() < 256 {
            self.datagrams_out.push_back((Bytes::from(enc), timeout.map(|t| created + t)));
        }
    }

    /// fetch の応答の object を送る（FETCH を受けた側）。
    pub fn fetch_object(&mut self, request_id: u64, o: &Object) {
        let h = self.fetch_stream(request_id);
        let Some(h) = h else { return };
        let Some(sub) = self.reqs.get_mut(&request_id).and_then(|r| r.sub.as_mut()) else { return };
        let d = data::ObjectData {
            group: o.group,
            subgroup: o.subgroup,
            object: o.id,
            priority: Some(o.priority),
            properties: o.properties.clone(),
            status: o.status,
            payload: o.payload.clone(),
        };
        let order = sub.group_order;
        let hdr = encode_fetch_object(&mut sub.fetch_prior, order, &d);
        self.push(h, Bytes::from(hdr), None);
        if !o.payload.is_empty() {
            self.push(h, o.payload.clone(), None);
        }
    }

    /// fetch の範囲の印を送る。
    pub fn fetch_range_end(&mut self, request_id: u64, kind: RangeEnd, end: Location) {
        let Some(h) = self.fetch_stream(request_id) else { return };
        let Some(sub) = self.reqs.get_mut(&request_id).and_then(|r| r.sub.as_mut()) else { return };
        let b = data::encode_fetch_range_end(&mut sub.fetch_prior, kind, end.group, end.object);
        self.push(h, Bytes::from(b), None);
    }

    /// fetch の応答を終える（データ stream の FIN と要求 stream の FIN）。
    pub fn fetch_finish(&mut self, request_id: u64) {
        let h = self.fetch_stream(request_id);
        if let Some(h) = h {
            self.finish_out(h);
        }
        if let Some(r) = self.reqs.remove(&request_id) {
            self.finish_out(r.out);
        }
    }

    /// SUBSCRIBE の FILL_PARAMETERS への応答（draft-22 3.5 の current-group join）: 購読の始まりより前の
    /// object を fetch stream（FETCH_HEADER に SUBSCRIBE の Request ID）で送る。購読の応答（SUBSCRIBE_OK）の後に呼ぶ。
    pub fn fill_object(&mut self, request_id: u64, o: &Object) {
        if !self.reqs.get(&request_id).is_some_and(|r| r.kind == ReqKind::Subscribe && r.alias.is_some()) {
            return;
        }
        self.fetch_object(request_id, o);
    }

    /// fill の fetch stream を閉じる（購読はそのまま続く）。
    pub fn fill_finish(&mut self, request_id: u64) {
        let h = self
            .reqs
            .get_mut(&request_id)
            .filter(|r| r.kind == ReqKind::Subscribe)
            .and_then(|r| r.sub.as_mut())
            .and_then(|s| s.fetch_out.take());
        if let Some(h) = h {
            self.finish_out(h);
        }
    }

    fn fetch_stream(&mut self, request_id: u64) -> Option<Handle> {
        let r = self.reqs.get(&request_id)?;
        if !matches!(r.kind, ReqKind::Fetch | ReqKind::Subscribe) || r.local {
            return None;
        }
        if let Some(h) = r.sub.as_ref().and_then(|s| s.fetch_out) {
            return Some(h);
        }
        let prio = r
            .sub
            .as_ref()
            .map_or(0, |s| Self::stream_priority(s, 128, 0) - (1 << 22));
        let h = self.new_out(true, prio, None);
        self.push(h, Bytes::from(encode_fetch_header(request_id)), None);
        if let Some(sub) = self.reqs.get_mut(&request_id).and_then(|r| r.sub.as_mut()) {
            sub.fetch_out = Some(h);
        }
        Some(h)
    }

}

impl SubState {
    fn from_params(p: &Params) -> Self {
        Self {
            subscriber_priority: p.subscriber_priority.unwrap_or(128),
            group_order: p.group_order.unwrap_or(GroupOrder::Ascending),
            object_timeout: p.object_delivery_timeout.filter(|&v| v > 0).map(Duration::from_millis),
            subgroup_timeout: p.subgroup_delivery_timeout.filter(|&v| v > 0).map(Duration::from_millis),
            forward: p.forward.unwrap_or(true),
            subgroups: HashMap::new(),
            dead: HashSet::new(),
            fetch_prior: FetchPrior::default(),
            fetch_out: None,
        }
    }

    /// 出版者の Track Property と合わせる（両方 0 でなければ小さい方、5.2）。
    fn apply_track(&mut self, tp: &TrackProperties) {
        let min = |a: Option<Duration>, b: Option<u64>| match (a, b.filter(|&v| v > 0).map(Duration::from_millis)) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        };
        self.object_timeout = min(self.object_timeout, tp.object_delivery_timeout);
        self.subgroup_timeout = min(self.subgroup_timeout, tp.subgroup_delivery_timeout);
        if let Some(g) = tp.default_group_order
            && self.group_order == GroupOrder::Ascending
        {
            self.group_order = g;
        }
    }

    fn update(&mut self, p: &Params) {
        if let Some(v) = p.subscriber_priority {
            self.subscriber_priority = v;
        }
        if let Some(v) = p.forward {
            self.forward = v;
        }
        if let Some(v) = p.object_delivery_timeout {
            self.object_timeout = (v > 0).then(|| Duration::from_millis(v));
        }
        if let Some(v) = p.subgroup_delivery_timeout {
            self.subgroup_timeout = (v > 0).then(|| Duration::from_millis(v));
        }
    }
}

/// Location Filter から開始位置を決める（3.3.1、`largest` は Largest Object）。
pub fn filter_start(f: Option<&LocationFilter>, largest: Option<Location>) -> Location {
    let next_object = largest.map_or(Location::new(0, 0), |l| Location::new(l.group, l.object + 1));
    match f {
        None | Some(LocationFilter::None) | Some(LocationFilter::NextObject) => next_object,
        Some(LocationFilter::RelativeStart(n)) => {
            let next_group = largest.map_or(0, |l| l.group.saturating_add(1));
            Location::new(next_group.saturating_sub(*n), 0)
        }
        Some(LocationFilter::AbsoluteStart(l)) | Some(LocationFilter::AbsoluteRange { start: l, .. }) => *l,
    }
}

/// Location Filter の終わり（含む）。`None` は終わりなし。
pub fn filter_end(f: Option<&LocationFilter>) -> Option<Location> {
    match f {
        Some(LocationFilter::AbsoluteRange { end_group, end_object, .. }) => Some(Location::new(*end_group, end_object.unwrap_or(u64::MAX))),
        _ => None,
    }
}

