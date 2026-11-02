//! セッション単体の試験（QUIC を使わず、stream を直接つなぐ）: 期限・優先度・不正な入力。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use senca_moq::params::Params;
use senca_moq::*;

/// 書き込みを記録する transport。
#[derive(Default)]
struct Pipe {
    client: bool,
    next_bi: u64,
    next_uni: u64,
    writes: Vec<(u64, Bytes)>,
    fins: Vec<u64>,
    resets: Vec<(u64, u64)>,
    stops: Vec<u64>,
    prio: HashMap<u64, i32>,
    datagrams: Vec<Bytes>,
    closed: Option<(u64, String)>,
}

impl Pipe {
    fn new(client: bool) -> Self {
        Self {
            client,
            ..Default::default()
        }
    }
}

impl Transport for Pipe {
    fn open_bi(&mut self) -> Option<u64> {
        let id = self.next_bi * 4 + u64::from(!self.client);
        self.next_bi += 1;
        Some(id)
    }
    fn open_uni(&mut self) -> Option<u64> {
        let id = self.next_uni * 4 + 2 + u64::from(!self.client);
        self.next_uni += 1;
        Some(id)
    }
    fn write(&mut self, stream: u64, data: Bytes) -> Result<usize, transport::StreamGone> {
        let n = data.len();
        self.writes.push((stream, data));
        Ok(n)
    }
    fn finish(&mut self, stream: u64) {
        self.fins.push(stream);
    }
    fn reset(&mut self, stream: u64, code: u64) {
        self.resets.push((stream, code));
    }
    fn stop(&mut self, stream: u64, _code: u64) {
        self.stops.push(stream);
    }
    fn set_priority(&mut self, stream: u64, priority: i32) {
        self.prio.insert(stream, priority);
    }
    fn send_datagram(&mut self, data: Bytes) -> bool {
        self.datagrams.push(data);
        true
    }
    fn max_datagram_size(&self) -> Option<usize> {
        Some(1100)
    }
    fn unacked(&self, _stream: u64) -> Option<u64> {
        Some(1)
    }
    fn close(&mut self, code: u64, reason: &str) {
        self.closed = Some((code, reason.to_string()));
    }
}

/// 片方の書き込みをもう片方に渡す。
fn deliver(from: &mut Pipe, to: &mut Session, now: Instant) {
    let writes = std::mem::take(&mut from.writes);
    let fins = std::mem::take(&mut from.fins);
    for (s, d) in writes {
        to.on_stream_data(s, s % 4 < 2, d, false, now);
    }
    for s in fins {
        to.on_stream_data(s, s % 4 < 2, Bytes::new(), true, now);
    }
    for (s, code) in std::mem::take(&mut from.resets) {
        to.on_stream_reset(s, code);
    }
    for d in std::mem::take(&mut from.datagrams) {
        to.on_datagram(d, now);
    }
}

fn pair(now: Instant) -> (Session, Pipe, Session, Pipe) {
    let mut c = Session::new(SessionConfig::new(Role::Client), now);
    let mut s = Session::new(SessionConfig::new(Role::Server), now);
    let mut cp = Pipe::new(true);
    let mut sp = Pipe::new(false);
    for _ in 0..3 {
        c.flush(&mut cp, now);
        s.flush(&mut sp, now);
        deliver(&mut cp, &mut s, now);
        deliver(&mut sp, &mut c, now);
    }
    assert!(c.peer_setup().is_some() && s.peer_setup().is_some());
    while c.poll_event().is_some() {}
    while s.poll_event().is_some() {}
    (c, cp, s, sp)
}

fn exchange(c: &mut Session, cp: &mut Pipe, s: &mut Session, sp: &mut Pipe, now: Instant) {
    for _ in 0..3 {
        c.flush(cp, now);
        s.flush(sp, now);
        deliver(cp, s, now);
        deliver(sp, c, now);
    }
}

fn obj(group: u64, id: u64, payload: &'static [u8], priority: u8) -> Object {
    Object {
        group,
        subgroup: Some(0),
        id,
        priority,
        status: ObjectStatus::Normal,
        properties: Bytes::new(),
        payload: Bytes::from_static(payload),
    }
}

/// client が subscribe し、server（publisher）が受け付けた Request ID を返す。
fn subscribe(c: &mut Session, cp: &mut Pipe, s: &mut Session, sp: &mut Pipe, now: Instant, params: Params) -> (u64, u64) {
    let creq = c
        .subscribe(SubscribeArgs {
            namespace: Namespace::from_path("a"),
            track: Bytes::from_static(b"t"),
            params,
        })
        .unwrap();
    exchange(c, cp, s, sp, now);
    let mut sreq = None;
    while let Some(e) = s.poll_event() {
        if let SessionEvent::Subscribe { request_id, .. } = e {
            sreq = Some(request_id);
        }
    }
    let sreq = sreq.expect("subscribe not received");
    s.subscribe_ok(sreq, None, Bytes::new()).unwrap();
    exchange(c, cp, s, sp, now);
    while let Some(e) = c.poll_event() {
        assert!(matches!(e, SessionEvent::SubscribeOk { .. }), "{e:?}");
    }
    (creq, sreq)
}

#[test]
fn object_delivery_timeout_drops_and_resets() {
    let now = Instant::now();
    let (mut c, mut cp, mut s, mut sp) = pair(now);
    let (_, sreq) = subscribe(
        &mut c,
        &mut cp,
        &mut s,
        &mut sp,
        now,
        Params {
            object_delivery_timeout: Some(200),
            ..Default::default()
        },
    );
    // 古い object は送らない（stream も開かない）
    s.send_object(sreq, &obj(1, 0, b"stale", 128), now - Duration::from_secs(1), now).unwrap();
    // 新しい object は送るが、書き出す前に期限が過ぎたら stream ごと打ち切る
    s.send_object(sreq, &obj(2, 0, b"fresh-but-queued", 128), now, now).unwrap();
    s.send_object(sreq, &obj(3, 0, b"fresh", 128), now, now).unwrap();
    let later = now + Duration::from_millis(500);
    // group 3 だけ書き出す前に期限内に送られたことにする: 先に group 3 の object を作り直す
    s.send_object(sreq, &obj(4, 0, b"on-time", 128), later, later).unwrap();
    exchange(&mut c, &mut cp, &mut s, &mut sp, later);
    let mut got = Vec::new();
    while let Some(e) = c.poll_event() {
        if let SessionEvent::Object { object, .. } = e {
            got.push(object.group);
        }
    }
    assert_eq!(got, vec![4], "only the on-time object is delivered");
    assert!(
        sp.resets.is_empty() || sp.resets.iter().all(|(_, code)| *code == reset::DELIVERY_TIMEOUT),
        "{:?}",
        sp.resets
    );
}

#[test]
fn subgroup_streams_get_priority_from_subscriber_and_publisher() {
    let now = Instant::now();
    let (mut c, mut cp, mut s, mut sp) = pair(now);
    let (_, low) = subscribe(
        &mut c,
        &mut cp,
        &mut s,
        &mut sp,
        now,
        Params {
            subscriber_priority: Some(200),
            ..Default::default()
        },
    );
    let (_, high) = subscribe(
        &mut c,
        &mut cp,
        &mut s,
        &mut sp,
        now,
        Params {
            subscriber_priority: Some(10),
            ..Default::default()
        },
    );
    // 先に低い方を積んでも、高い方が先に書かれる
    s.send_object(low, &obj(0, 0, b"low", 128), now, now).unwrap();
    s.send_object(high, &obj(0, 0, b"high-pub-low", 250), now, now).unwrap();
    let mut o = obj(0, 1, b"high", 128);
    o.subgroup = Some(1);
    s.send_object(high, &o, now, now).unwrap();
    s.send_object(high, &obj(5, 0, b"high-later-group", 128), now, now).unwrap();
    s.flush(&mut sp, now);
    let first_payload_stream = sp.writes.iter().find(|(_, d)| &d[..] == b"low").map(|(s, _)| *s).unwrap();
    let high_stream = sp.writes.iter().find(|(_, d)| &d[..] == b"high").map(|(s, _)| *s).unwrap();
    assert!(sp.prio[&high_stream] > sp.prio[&first_payload_stream]);
    let pos = |p: &[u8]| sp.writes.iter().position(|(_, d)| &d[..] == p).unwrap();
    assert!(pos(b"high") < pos(b"low"));
    // 出版者の優先度（同じ購読者の優先度のとき）
    let pub_low = sp.writes.iter().find(|(_, d)| &d[..] == b"high-pub-low").map(|(s, _)| *s).unwrap();
    assert!(sp.prio[&high_stream] > sp.prio[&pub_low]);
    // 群の昇順: 古い group が先
    let g5 = sp.writes.iter().find(|(_, d)| &d[..] == b"high-later-group").map(|(s, _)| *s).unwrap();
    assert!(sp.prio[&high_stream] > sp.prio[&g5]);
}

#[test]
fn protocol_violations_close_the_session() {
    let now = Instant::now();
    // 不正な Request ID（client なのに奇数）
    let (_c, _cp, mut s, mut sp) = pair(now);
    let m = message::Message::Subscribe {
        request_id: 1,
        namespace: Namespace::from_path("a"),
        track: Bytes::from_static(b"t"),
        params: Params::default(),
    }
    .encode()
    .unwrap();
    s.on_stream_data(0, true, Bytes::from(m), false, now);
    s.flush(&mut sp, now);
    assert_eq!(sp.closed.as_ref().map(|c| c.0), Some(SessionErrorCode::INVALID_REQUEST_ID.0));

    // 制御 stream に SETUP 以外
    let (_c, _cp, mut s, mut sp) = pair(now);
    let m = message::Message::Namespace {
        suffix: Namespace::from_path("x"),
    }
    .encode()
    .unwrap();
    s.on_stream_data(2, false, Bytes::from(m), false, now);
    // 二本目の制御 stream（SETUP の型で始まる）
    let setup = message::Message::Setup(params::SetupOptions::default()).encode().unwrap();
    s.on_stream_data(6, false, Bytes::from(setup), false, now);
    s.flush(&mut sp, now);
    assert_eq!(sp.closed.as_ref().map(|c| c.0), Some(SessionErrorCode::PROTOCOL_VIOLATION.0));

    // 未知の単方向 stream の種別
    let (_c, _cp, mut s, mut sp) = pair(now);
    s.on_stream_data(10, false, Bytes::from_static(&[0x77]), false, now);
    s.flush(&mut sp, now);
    assert!(sp.closed.is_some());

    // 未知の datagram の型
    let (_c, _cp, mut s, mut sp) = pair(now);
    s.on_datagram(Bytes::from_static(&[0x40, 0x01, 0x00]), now);
    s.flush(&mut sp, now);
    assert!(sp.closed.is_some());
}

#[test]
fn unknown_track_alias_streams_are_buffered_until_subscribe_ok() {
    let now = Instant::now();
    let (mut c, mut cp, mut s, mut sp) = pair(now);
    let creq = c
        .subscribe(SubscribeArgs {
            namespace: Namespace::from_path("a"),
            track: Bytes::from_static(b"t"),
            params: Params::default(),
        })
        .unwrap();
    exchange(&mut c, &mut cp, &mut s, &mut sp, now);
    let mut sreq = None;
    while let Some(e) = s.poll_event() {
        if let SessionEvent::Subscribe { request_id, .. } = e {
            sreq = Some(request_id);
        }
    }
    let sreq = sreq.unwrap();
    s.subscribe_ok(sreq, None, Bytes::new()).unwrap();
    s.send_object(sreq, &obj(0, 0, b"early", 128), now, now).unwrap();
    s.flush(&mut sp, now);
    // データ stream を先に、応答（要求 stream）を後に届ける
    let (data, ctrl): (Vec<_>, Vec<_>) = sp.writes.drain(..).partition(|(id, _)| id % 4 == 3);
    for (id, d) in data {
        c.on_stream_data(id, false, d, false, now);
    }
    assert!(c.poll_event().is_none());
    for (id, d) in ctrl {
        c.on_stream_data(id, true, d, false, now);
    }
    let mut evs = Vec::new();
    while let Some(e) = c.poll_event() {
        evs.push(e);
    }
    assert!(matches!(evs[0], SessionEvent::SubscribeOk { request_id, .. } if request_id == creq));
    assert!(matches!(&evs[1], SessionEvent::Object { object, .. } if &object.payload[..] == b"early"));
}

#[test]
fn fetch_point_to_point_with_descending_order() {
    let now = Instant::now();
    let (mut c, mut cp, mut s, mut sp) = pair(now);
    let creq = c
        .fetch(
            Namespace::from_path("a"),
            Bytes::from_static(b"t"),
            Params {
                group_order: Some(params::GroupOrder::Descending),
                ..Default::default()
            },
        )
        .unwrap();
    exchange(&mut c, &mut cp, &mut s, &mut sp, now);
    let mut sreq = None;
    while let Some(e) = s.poll_event() {
        if let SessionEvent::Fetch { request_id, .. } = e {
            sreq = Some(request_id);
        }
    }
    let sreq = sreq.unwrap();
    s.fetch_ok(sreq, true, Location::new(3, 1), Bytes::new()).unwrap();
    for (g, o, p) in [(3u64, 0u64, &b"g3o0"[..]), (3, 1, b"g3o1"), (1, 0, b"g1o0")] {
        s.fetch_object(
            sreq,
            &Object {
                group: g,
                subgroup: Some(0),
                id: o,
                priority: 7,
                status: ObjectStatus::Normal,
                properties: Bytes::new(),
                payload: Bytes::copy_from_slice(p),
            },
        );
    }
    s.fetch_finish(sreq);
    exchange(&mut c, &mut cp, &mut s, &mut sp, now);
    let mut got = Vec::new();
    let mut complete = false;
    while let Some(e) = c.poll_event() {
        match e {
            SessionEvent::FetchOk { end_of_track, .. } => assert!(end_of_track),
            SessionEvent::Object { object, request_id, .. } => {
                assert_eq!(request_id, creq);
                got.push((object.group, object.id));
            }
            SessionEvent::FetchComplete { .. } => complete = true,
            _ => {}
        }
    }
    assert_eq!(got, vec![(3, 0), (3, 1), (1, 0)]);
    assert!(complete);
}
