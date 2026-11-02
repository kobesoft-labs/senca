//! DataChannels の試験。

use std::time::{Duration, Instant};

use bytes::Bytes;

use super::{Link, Sim};
use crate::{
    ChannelInfo, ChannelOptions, DataChannelEvent, DataChannels, Reliability, SctpConfig, SctpError,
};

type DSim = Sim<DataChannels, DataChannels>;

fn pair(seed: u64, link: Link) -> DSim {
    let now = Instant::now();
    let a = DataChannels::new(SctpConfig::default(), true, now).unwrap();
    let b = DataChannels::new(SctpConfig::default(), false, now).unwrap();
    Sim::new(a, b, now, seed, link)
}

fn connect_both(s: &mut DSim) {
    let now = s.now;
    s.a.connect(now);
    s.b.connect(now);
    assert!(s.run_until(Duration::from_secs(30), |s| s.a.is_connected()
        && s.b.is_connected()));
    assert!(s.a_events.contains(&DataChannelEvent::Connected));
    assert!(s.b_events.contains(&DataChannelEvent::Connected));
}

fn opened(ev: &[DataChannelEvent], id: u16) -> Option<ChannelInfo> {
    ev.iter().find_map(|e| match e {
        DataChannelEvent::Open(i) if i.id == id => Some(i.clone()),
        _ => None,
    })
}

fn msgs(ev: &[DataChannelEvent], id: u16) -> Vec<(Bytes, bool)> {
    ev.iter()
        .filter_map(|e| match e {
            DataChannelEvent::Message {
                id: i,
                data,
                binary,
            } if *i == id => Some((data.clone(), *binary)),
            _ => None,
        })
        .collect()
}

fn closed(ev: &[DataChannelEvent], id: u16) -> bool {
    ev.contains(&DataChannelEvent::Closed { id })
}

fn opts(label: &str) -> ChannelOptions {
    ChannelOptions {
        label: label.into(),
        ..Default::default()
    }
}

#[test]
fn open_from_both_sides_and_messages() {
    for (seed, link) in [(1u64, Link::default()), (2, Link::lossy(15, 5, 15))] {
        let mut s = pair(seed, link);
        let now = s.now;
        // つながる前に開く（OPEN は後で送る）。
        let a_id = s.a.open(now, opts("from-a")).unwrap();
        assert_eq!(a_id, 0);
        assert_eq!(
            s.a.send_text(now, a_id, "too early"),
            Err(SctpError::NotConnected)
        );
        connect_both(&mut s);
        let now = s.now;
        let b_id =
            s.b.open(
                now,
                ChannelOptions {
                    label: "from-b".into(),
                    protocol: "proto".into(),
                    ordered: false,
                    reliability: Reliability::MaxRetransmits(2),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(b_id, 1);
        assert!(s.run_until(Duration::from_secs(30), |s| {
            opened(&s.a_events, a_id).is_some()
                && opened(&s.b_events, a_id).is_some()
                && opened(&s.a_events, b_id).is_some()
                && opened(&s.b_events, b_id).is_some()
        }));
        let info = opened(&s.a_events, b_id).unwrap();
        assert_eq!(info.label, "from-b");
        assert_eq!(info.protocol, "proto");
        assert!(!info.ordered);
        assert_eq!(info.reliability, Reliability::MaxRetransmits(2));
        assert!(info.open && !info.negotiated);
        assert_eq!(opened(&s.b_events, a_id).unwrap().label, "from-a");
        assert_eq!(s.a.channels().len(), 2);
        assert!(s.a.channel(a_id).unwrap().open);

        let now = s.now;
        s.a.send_text(now, a_id, "こんにちは").unwrap();
        s.a.send_text(now, a_id, "").unwrap();
        s.a.send_binary(now, a_id, Bytes::from_static(&[1, 2, 3]))
            .unwrap();
        s.a.send_binary(now, a_id, Bytes::new()).unwrap();
        s.b.send_text(now, a_id, "reply").unwrap();
        s.b.send_binary(now, b_id, Bytes::from(vec![7u8; 5000]))
            .unwrap();
        assert!(s.run_until(Duration::from_secs(60), |s| {
            msgs(&s.b_events, a_id).len() == 4
                && msgs(&s.a_events, a_id).len() == 1
                && msgs(&s.a_events, b_id).len() == 1
        }));
        let got = msgs(&s.b_events, a_id);
        assert_eq!(got[0], (Bytes::from("こんにちは"), false));
        assert_eq!(got[1], (Bytes::new(), false));
        assert_eq!(got[2], (Bytes::from_static(&[1, 2, 3]), true));
        assert_eq!(got[3], (Bytes::new(), true));
        assert_eq!(msgs(&s.a_events, a_id)[0], (Bytes::from("reply"), false));
        assert_eq!(msgs(&s.a_events, b_id)[0].0.len(), 5000);
        assert!(s.run_until(Duration::from_secs(10), |s| s.b.buffered_amount(b_id) == 0));
        assert!(
            s.b_events
                .contains(&DataChannelEvent::BufferedAmountLow { id: b_id })
        );
    }
}

#[test]
fn ids_follow_dtls_role() {
    let mut s = pair(3, Link::default());
    connect_both(&mut s);
    let now = s.now;
    let a: Vec<u16> = (0..3).map(|_| s.a.open(now, opts("x")).unwrap()).collect();
    let b: Vec<u16> = (0..3).map(|_| s.b.open(now, opts("y")).unwrap()).collect();
    assert_eq!(a, vec![0, 2, 4]);
    assert_eq!(b, vec![1, 3, 5]);
    assert!(
        s.run_until(Duration::from_secs(10), |s| s.a.channels().len() == 6
            && s.b.channels().len() == 6)
    );
}

#[test]
fn negotiated_channel() {
    let mut s = pair(4, Link::default());
    let now = s.now;
    let o = ChannelOptions {
        label: "neg".into(),
        negotiated_id: Some(7),
        ..Default::default()
    };
    assert_eq!(s.a.open(now, o.clone()).unwrap(), 7);
    assert_eq!(
        s.a.open(now, o.clone()),
        Err(SctpError::ChannelIdUnavailable)
    );
    connect_both(&mut s);
    assert!(opened(&s.a_events, 7).is_some());
    let now = s.now;
    assert_eq!(s.b.open(now, o).unwrap(), 7);
    assert!(opened(&s.b_events, 7).is_none());
    s.flush();
    assert!(opened(&s.b_events, 7).unwrap().negotiated);
    s.a.send_text(now, 7, "neg-hello").unwrap();
    s.b.send_binary(now, 7, Bytes::from_static(b"\x00\x01"))
        .unwrap();
    assert!(s.run_until(Duration::from_secs(10), |s| {
        msgs(&s.b_events, 7).len() == 1 && msgs(&s.a_events, 7).len() == 1
    }));
    assert_eq!(msgs(&s.b_events, 7)[0].0, Bytes::from("neg-hello"));
}

#[test]
fn close_from_each_side() {
    for (seed, link) in [(5u64, Link::default()), (6, Link::lossy(20, 5, 20))] {
        let mut s = pair(seed, link);
        connect_both(&mut s);
        let now = s.now;
        let a_id = s.a.open(now, opts("a")).unwrap();
        let b_id = s.b.open(now, opts("b")).unwrap();
        assert!(s.run_until(Duration::from_secs(30), |s| {
            opened(&s.a_events, b_id).is_some()
                && opened(&s.b_events, a_id).is_some()
                && opened(&s.a_events, a_id).is_some()
                && opened(&s.b_events, b_id).is_some()
        }));
        let now = s.now;
        for i in 0..10 {
            s.a.send_text(now, a_id, &format!("m{i}")).unwrap();
        }
        // a が閉じる。送った分は先に届く。
        s.a.close(now, a_id).unwrap();
        assert_eq!(
            s.a.send_text(now, a_id, "x"),
            Err(SctpError::UnknownChannel)
        );
        assert!(s.run_until(Duration::from_secs(60), |s| {
            closed(&s.a_events, a_id) && closed(&s.b_events, a_id)
        }));
        assert_eq!(msgs(&s.b_events, a_id).len(), 10);
        assert!(s.a.channel(a_id).is_none() && s.b.channel(a_id).is_none());
        // b が閉じる。
        let now = s.now;
        s.b.close(now, b_id).unwrap();
        assert!(s.run_until(Duration::from_secs(60), |s| {
            closed(&s.a_events, b_id) && closed(&s.b_events, b_id)
        }));
        // 閉じた番号はまた使える。
        let now = s.now;
        let again = s.a.open(now, opts("again")).unwrap();
        assert_eq!(again, a_id);
        let opens_before = s
            .b_events
            .iter()
            .filter(|e| matches!(e, DataChannelEvent::Open(i) if i.id == again))
            .count();
        assert!(s.run_until(Duration::from_secs(30), |s| {
            s.b_events
                .iter()
                .filter(|e| matches!(e, DataChannelEvent::Open(i) if i.id == again))
                .count()
                > opens_before
        }));
        let now = s.now;
        s.a.send_text(now, again, "after reopen").unwrap();
        assert!(s.run_until(Duration::from_secs(30), |s| {
            msgs(&s.b_events, again)
                .iter()
                .any(|m| m.0 == "after reopen")
        }));
    }
}

#[test]
fn close_simultaneously() {
    let mut s = pair(8, Link::default());
    connect_both(&mut s);
    let now = s.now;
    let id = s.a.open(now, opts("x")).unwrap();
    assert!(s.run_until(
        Duration::from_secs(10),
        |s| opened(&s.a_events, id).is_some() && opened(&s.b_events, id).is_some()
    ));
    let now = s.now;
    s.a.close(now, id).unwrap();
    s.b.close(now, id).unwrap();
    assert!(
        s.run_until(Duration::from_secs(30), |s| closed(&s.a_events, id)
            && closed(&s.b_events, id))
    );
}

#[test]
fn shutdown_reports_disconnect() {
    let mut s = pair(9, Link::default());
    connect_both(&mut s);
    let now = s.now;
    let id = s.a.open(now, opts("x")).unwrap();
    assert!(s.run_until(Duration::from_secs(10), |s| {
        opened(&s.b_events, id).is_some()
    }));
    let now = s.now;
    s.a.shutdown(now);
    assert!(s.run_until(Duration::from_secs(30), |s| {
        s.a_events
            .iter()
            .any(|e| matches!(e, DataChannelEvent::Disconnected { .. }))
            && s.b_events
                .iter()
                .any(|e| matches!(e, DataChannelEvent::Disconnected { .. }))
    }));
    assert!(closed(&s.b_events, id));
    assert!(!s.a.is_connected());
    let now = s.now;
    assert_eq!(s.a.open(now, opts("late")), Err(SctpError::Closed));
}

/// 開く・送る・閉じるを両側からでたらめに繰り返す（損失のある回線で）。
#[test]
fn randomized_open_send_close() {
    let seeds: u64 = std::env::var("SENCA_SCTP_STRESS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let mut total = 0usize;
    for seed in 0..seeds {
        let mut r = super::XorShift::new(seed + 77);
        let link = Link::lossy(r.below(25) as u32, r.below(10) as u32, r.below(30) as u32);
        let mut s = pair(seed + 300, link);
        connect_both(&mut s);
        // (送った側が a か, id) → 送った文字列
        let mut sent: std::collections::BTreeMap<(bool, u16), Vec<String>> = Default::default();
        let mut n = 0u32;
        for _ in 0..60 {
            let now = s.now;
            let side_a = r.chance(50);
            match r.below(10) {
                0 | 1 => {
                    let o = opts("r");
                    let _ = if side_a {
                        s.a.open(now, o)
                    } else {
                        s.b.open(now, o)
                    };
                }
                2 => {
                    let chans = if side_a {
                        s.a.channels()
                    } else {
                        s.b.channels()
                    };
                    if let Some(c) = chans.get(r.below(chans.len().max(1) as u64) as usize) {
                        let id = c.id;
                        if side_a {
                            s.a.close(now, id).unwrap()
                        } else {
                            s.b.close(now, id).unwrap()
                        }
                    }
                }
                _ => {
                    let chans = if side_a {
                        s.a.channels()
                    } else {
                        s.b.channels()
                    };
                    for c in chans.iter().filter(|c| c.open) {
                        n += 1;
                        let text = format!("{n}");
                        let ok = if side_a {
                            s.a.send_text(now, c.id, &text)
                        } else {
                            s.b.send_text(now, c.id, &text)
                        };
                        if ok.is_ok() {
                            sent.entry((side_a, c.id)).or_default().push(text);
                        }
                    }
                }
            }
            s.run_for(Duration::from_millis(r.below(400)));
        }
        total += sent.values().map(|v| v.len()).sum::<usize>();
        // すべて閉じる。
        let now = s.now;
        for c in s.a.channels() {
            s.a.close(now, c.id).unwrap();
        }
        for c in s.b.channels() {
            s.b.close(now, c.id).unwrap();
        }
        assert!(
            s.run_until(Duration::from_secs(600), |s| s.a.channels().is_empty()
                && s.b.channels().is_empty()),
            "seed {seed}: {:?} / {:?}",
            s.a.channels(),
            s.b.channels()
        );
        assert!(s.a.is_connected() && s.b.is_connected());
        // 送れたメッセージは、同じ番号の（同じ世代の）チャネルで順に届いている。
        let recv = |ev: &[DataChannelEvent], id: u16| -> Vec<String> {
            msgs(ev, id)
                .into_iter()
                .map(|(d, _)| String::from_utf8(d.to_vec()).unwrap())
                .collect()
        };
        for ((from_a, id), texts) in &sent {
            let got = if *from_a {
                recv(&s.b_events, *id)
            } else {
                recv(&s.a_events, *id)
            };
            let got: Vec<&String> = got.iter().filter(|t| texts.contains(t)).collect();
            let want: Vec<&String> = texts.iter().collect();
            assert_eq!(got, want, "seed {seed} id {id}");
        }
        // Open と Closed の数は両側で合う。
        let count = |ev: &[DataChannelEvent], open: bool| {
            ev.iter()
                .filter(|e| {
                    if open {
                        matches!(e, DataChannelEvent::Open(_))
                    } else {
                        matches!(e, DataChannelEvent::Closed { .. })
                    }
                })
                .count()
        };
        assert_eq!(
            count(&s.a_events, true),
            count(&s.b_events, true),
            "seed {seed}"
        );
        assert_eq!(
            count(&s.a_events, false),
            count(&s.b_events, false),
            "seed {seed}"
        );
    }
    assert!(total as u64 > seeds * 10, "too little traffic: {total}");
}

#[test]
fn slow_receiver_backpressure() {
    let mut s = pair(11, Link::default());
    connect_both(&mut s);
    let now = s.now;
    let id = s.a.open(now, opts("bulk")).unwrap();
    assert!(s.run_until(Duration::from_secs(10), |s| {
        opened(&s.b_events, id).is_some()
    }));
    // b のアプリが出来事を取り出さない間に、a が窓の数倍を送ろうとする。
    s.hold_events[1] = true;
    let chunk = Bytes::from(vec![5u8; 60_000]);
    let mut queued = 0usize;
    for _ in 0..200 {
        let now = s.now;
        while s.a.send_binary(now, id, chunk.clone()).is_ok() {
            queued += chunk.len();
        }
        s.run_for(Duration::from_millis(200));
    }
    let window = SctpConfig::default().receive_window as usize;
    let st = s.b.association().stats();
    assert!(st.rx_buffered <= 2 * window, "{st:?}");
    assert!(s.b.queued_bytes <= window + chunk.len());
    assert!(
        queued > 4 * window,
        "sender kept going until its buffer filled"
    );
    // 取り出し始めればすべて届く。
    s.hold_events[1] = false;
    assert!(s.run_until(Duration::from_secs(600), |s| {
        msgs(&s.b_events, id).len() * chunk.len() == queued
    }));
}
