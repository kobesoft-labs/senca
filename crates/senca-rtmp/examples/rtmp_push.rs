//! 試験用の RTMP クライアント（publish、ブロッキング I/O）。FLV ファイルのタグをそのまま送る。
//!
//! `rtmp_push <host:port> <app> <stream_key> <in.flv>`

use senca_flv::script::ScriptData;
use senca_flv::{FlvReader, TagType};
use senca_rtmp::{ClientConfig, ClientEvent, ClientMode, ClientSession, MediaType, Secret};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 5 {
        eprintln!("usage: rtmp_push <host:port> <app> <stream_key> <in.flv>");
        std::process::exit(2);
    }
    let addr = &args[1];
    let app = &args[2];
    let key = Secret::new(args[3].clone());
    let mut sock = TcpStream::connect(addr)?;
    sock.set_read_timeout(Some(Duration::from_secs(30)))?;
    sock.set_nodelay(true)?;
    let mut c = ClientSession::new(ClientConfig::new(
        app,
        &format!("rtmp://{addr}/{app}"),
        ClientMode::Publish {
            stream_key: key,
            publish_type: "live".into(),
        },
    ));
    let mut buf = vec![0u8; 64 * 1024];
    sock.write_all(&c.take_output())?;
    'wait: loop {
        let n = sock.read(&mut buf)?;
        if n == 0 {
            return Err("server closed".into());
        }
        c.feed(&buf[..n])?;
        while let Some(ev) = c.poll_event() {
            match ev {
                ClientEvent::PublishStarted { stream_id } => {
                    eprintln!("publishing stream_id={stream_id}");
                    sock.write_all(&c.take_output())?;
                    break 'wait;
                }
                ClientEvent::Rejected { command, code, .. } => {
                    return Err(format!("rejected: {command} {code:?}").into());
                }
                other => eprintln!("event {other:?}"),
            }
        }
        sock.write_all(&c.take_output())?;
    }

    // 相手からの Ack などを読み、Ack を返す処理はセッションに任せたいが、
    // ここは単純に読み捨てる（送信だけの試験）。
    let mut rd = sock.try_clone()?;
    std::thread::spawn(move || {
        let mut b = [0u8; 4096];
        while matches!(rd.read(&mut b), Ok(n) if n > 0) {}
    });

    let data = std::fs::read(&args[4])?;
    let mut r = FlvReader::new();
    r.feed(&data);
    let mut sent = 0;
    while let Some(tag) = r.next_tag()? {
        match tag.tag_type {
            TagType::Script => {
                let sd = ScriptData::parse(&tag.body)?.strip_set_data_frame()?;
                if sd.name == "onMetaData" {
                    c.send_metadata(tag.timestamp, &sd.encode()?)?;
                }
            }
            TagType::Audio => c.send_media(MediaType::Audio, tag.timestamp, &[&tag.body])?,
            TagType::Video => c.send_media(MediaType::Video, tag.timestamp, &[&tag.body])?,
        }
        sent += 1;
        sock.write_all(&c.take_output())?;
    }
    c.close()?;
    sock.write_all(&c.take_output())?;
    eprintln!("pushed {sent} tags");
    std::thread::sleep(Duration::from_millis(500));
    sock.shutdown(std::net::Shutdown::Both)?;
    Ok(())
}
