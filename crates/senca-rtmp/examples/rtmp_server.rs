//! 試験用の RTMP サーバー（1 接続だけ、ブロッキング I/O）。
//!
//! - `rtmp_server record <port> <out.flv>`: publish を 1 つ受け、FlvIngest → FlvMuxer を通して FLV に書く。
//! - `rtmp_server serve <port> <in.flv>`: play を 1 つ受け、FLV を FlvReader → FlvIngest → FlvMuxer で送る。
//!
//! 待ち受けを始めたら標準出力に `LISTENING <port>` を出す。stream key は表示しない。

use senca_flv::file::{FlvHeader, write_header, write_out_tag, write_tag};
use senca_flv::script::ScriptData;
use senca_flv::{FlvIngest, FlvMuxer, FlvReader, IngestEvent, OutTag, TagType};
use senca_rtmp::{MediaType, ServerConfig, ServerEvent, ServerSession};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: rtmp_server (record|serve) <port> <file.flv>");
        std::process::exit(2);
    }
    let port: u16 = args[2].parse()?;
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!("LISTENING {port}");
    std::io::stdout().flush()?;
    let (sock, _) = listener.accept()?;
    sock.set_read_timeout(Some(Duration::from_secs(30)))?;
    sock.set_nodelay(true)?;
    match args[1].as_str() {
        "record" => record(sock, &args[3]),
        "serve" => serve(sock, &args[3]),
        m => Err(format!("unknown mode {m}").into()),
    }
}

fn flush(sock: &mut TcpStream, s: &mut ServerSession) -> Res<()> {
    let out = s.take_output();
    if !out.is_empty() {
        sock.write_all(&out)?;
    }
    Ok(())
}

fn record(mut sock: TcpStream, path: &str) -> Res<()> {
    let mut s = ServerSession::new(ServerConfig::default());
    let mut ingest = FlvIngest::default();
    let mut mux = FlvMuxer::new();
    let mut file = Vec::new();
    write_header(
        FlvHeader {
            has_audio: true,
            has_video: true,
        },
        &mut file,
    );
    let mut buf = vec![0u8; 64 * 1024];
    let mut evs = Vec::new();
    let mut tags: Vec<OutTag> = Vec::new();
    let (mut n_video, mut n_audio) = (0u64, 0u64);
    'outer: loop {
        let n = match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            Err(e) => return Err(e.into()),
        };
        s.feed(&buf[..n])?;
        while let Some(ev) = s.poll_event() {
            match ev {
                ServerEvent::Connect(info) => {
                    eprintln!("connect app={}", info.app);
                    s.accept_connect()?;
                }
                ServerEvent::Publish { stream_id, .. } => {
                    eprintln!("publish stream_id={stream_id}");
                    s.accept_publish(stream_id)?;
                }
                ServerEvent::Play { stream_id, .. } => {
                    s.reject_play(stream_id, "NetStream.Play.Failed", "record mode")?;
                }
                ServerEvent::Metadata {
                    timestamp, payload, ..
                } => {
                    write_tag(TagType::Script, timestamp, &[&payload], &mut file)?;
                }
                ServerEvent::Media { message, .. } => {
                    let tt = match message.media_type {
                        MediaType::Audio => TagType::Audio,
                        MediaType::Video => TagType::Video,
                    };
                    ingest.push(tt, message.timestamp, &message.payload, &mut evs)?;
                    for e in evs.drain(..) {
                        match e {
                            IngestEvent::Config { config, .. } => mux.set_config(config)?,
                            IngestEvent::AccessUnit(au) => {
                                match tt {
                                    TagType::Video => n_video += 1,
                                    _ => n_audio += 1,
                                }
                                mux.access_unit(&au, &mut tags)?;
                            }
                            IngestEvent::EndOfSequence { .. } => {}
                        }
                    }
                    for t in tags.drain(..) {
                        write_out_tag(&t, &mut file)?;
                    }
                }
                ServerEvent::StreamClosed { stream_id, reason } => {
                    eprintln!("closed stream_id={stream_id} reason={reason:?}");
                    flush(&mut sock, &mut s)?;
                    break 'outer;
                }
                other => eprintln!("event {other:?}"),
            }
        }
        flush(&mut sock, &mut s)?;
    }
    std::fs::write(path, &file)?;
    eprintln!(
        "recorded video={n_video} audio={n_audio} dropped_before_config={}",
        ingest.dropped_before_config()
    );
    Ok(())
}

fn serve(mut sock: TcpStream, path: &str) -> Res<()> {
    let mut s = ServerSession::new(ServerConfig::default());
    let mut buf = vec![0u8; 64 * 1024];
    let sid = 'wait: loop {
        let n = sock.read(&mut buf)?;
        if n == 0 {
            return Err("peer closed before play".into());
        }
        s.feed(&buf[..n])?;
        while let Some(ev) = s.poll_event() {
            match ev {
                ServerEvent::Connect(_) => s.accept_connect()?,
                ServerEvent::Play { stream_id, .. } => {
                    s.accept_play(stream_id)?;
                    flush(&mut sock, &mut s)?;
                    break 'wait stream_id;
                }
                ServerEvent::Publish { stream_id, .. } => {
                    s.reject_publish(stream_id, "NetStream.Publish.Denied", "serve mode")?
                }
                other => eprintln!("event {other:?}"),
            }
        }
        flush(&mut sock, &mut s)?;
    };
    eprintln!("play stream_id={sid}");

    // 相手からの Ack などを読み捨てるスレッド（ここは試験用の例なので単純にする）。
    let mut rd = sock.try_clone()?;
    std::thread::spawn(move || {
        let mut b = [0u8; 4096];
        while matches!(rd.read(&mut b), Ok(n) if n > 0) {}
    });

    let data = std::fs::read(path)?;
    let mut reader = FlvReader::new();
    reader.feed(&data);
    let mut ingest = FlvIngest::default();
    let mut mux = FlvMuxer::new();
    let mut evs = Vec::new();
    let mut tags = Vec::new();
    let mut sent = 0u64;
    while let Some(tag) = reader.next_tag()? {
        match tag.tag_type {
            TagType::Script => {
                let sd = ScriptData::parse(&tag.body)?.strip_set_data_frame()?;
                if sd.name == "onMetaData" {
                    s.send_data(sid, tag.timestamp, &sd.encode()?)?;
                }
            }
            tt => {
                ingest.push(tt, tag.timestamp, &tag.body, &mut evs)?;
                for e in evs.drain(..) {
                    match e {
                        IngestEvent::Config { config, .. } => mux.set_config(config)?,
                        IngestEvent::AccessUnit(au) => mux.access_unit(&au, &mut tags)?,
                        IngestEvent::EndOfSequence { .. } => {}
                    }
                }
                for t in tags.drain(..) {
                    let mt = match t.tag_type {
                        TagType::Audio => MediaType::Audio,
                        _ => MediaType::Video,
                    };
                    s.send_media(sid, mt, t.timestamp, &t.body.parts())?;
                    sent += 1;
                }
            }
        }
        flush(&mut sock, &mut s)?;
    }
    s.end_play(sid)?;
    flush(&mut sock, &mut s)?;
    eprintln!("served {sent} media messages");
    std::thread::sleep(Duration::from_millis(500));
    sock.shutdown(std::net::Shutdown::Both)?;
    Ok(())
}
