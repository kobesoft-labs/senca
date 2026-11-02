//! 相互接続試験（`tools/interop/codec_sw.sh`）用の小さな道具。製品には入らない。
//!
//! IVF（VP8/VP9/AV1）と Ogg（Opus）の読み書きは試験のためだけの最小限の実装。
//!
//! ```text
//! codec_tool probe
//! codec_tool decode <vp8|vp9|av1> <in.ivf> <out.yuv>             # I420 を詰めて書く
//! codec_tool encode <vp8|vp9|av1> <w> <h> <fps> <in.yuv> <out.ivf> [bitrate_bps] [ll|ra]
//! codec_tool opus-encode <rate> <channels> <in.s16le> <out.ogg> [bitrate_bps]
//! codec_tool opus-decode <in.ogg> <out.s16le>                     # 48 kHz、pre_skip を除く
//! ```
//!
//! ライブラリの場所は `SENCA_CODEC_LIB_DIR`。

use bytes::Bytes;
use senca_codec::{
    AudioEncoderConfig, AudioFrame, DecoderConfig, EncoderConfig, Frame, PixelFormat, Policy,
    ProbeOptions, ProviderRegistry, RateControl, SampleFormat, VideoEncoderConfig, VideoFrame,
};
use senca_core::{AccessUnit, Codec, CodecConfig, Timebase, TrackId};
use std::error::Error;
use std::fs;

type R<T> = Result<T, Box<dyn Error>>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("codec_tool: {e}");
        std::process::exit(1);
    }
}

fn run(a: &[String]) -> R<()> {
    let reg = ProviderRegistry::probe(&ProbeOptions::from_env());
    match a.first().map(String::as_str) {
        Some("probe") => {
            for s in reg.statuses() {
                println!(
                    "provider {} available={} {}",
                    s.provider.as_str(),
                    s.available,
                    s.detail
                );
            }
            for c in reg.capabilities(&Policy::default()) {
                println!(
                    "cap {} {} {} hw={} max={}x{} device={}",
                    c.provider.as_str(),
                    c.codec,
                    c.direction.as_str(),
                    c.hardware,
                    c.max_width,
                    c.max_height,
                    c.device_name
                );
            }
            Ok(())
        }
        Some("decode") if a.len() == 4 => decode(&reg, codec(&a[1])?, &a[2], &a[3]),
        Some("encode") if a.len() >= 7 => {
            let bitrate = a
                .get(7)
                .map(|s| s.parse())
                .transpose()?
                .unwrap_or(1_000_000);
            let low_latency = a.get(8).map(String::as_str) != Some("ra");
            encode(
                &reg,
                codec(&a[1])?,
                a[2].parse()?,
                a[3].parse()?,
                a[4].parse()?,
                &a[5],
                &a[6],
                bitrate,
                low_latency,
            )
        }
        Some("opus-encode") if a.len() >= 5 => {
            let bitrate = a.get(5).map(|s| s.parse()).transpose()?.unwrap_or(96_000);
            opus_encode(&reg, a[1].parse()?, a[2].parse()?, &a[3], &a[4], bitrate)
        }
        Some("opus-decode") if a.len() == 3 => opus_decode(&reg, &a[1], &a[2]),
        _ => Err("usage: see the source (probe|decode|encode|opus-encode|opus-decode)".into()),
    }
}

fn codec(s: &str) -> R<Codec> {
    match s {
        "vp8" => Ok(Codec::Vp8),
        "vp9" => Ok(Codec::Vp9),
        "av1" => Ok(Codec::Av1),
        _ => Err(format!("unknown codec {s}").into()),
    }
}

// ---- IVF ----

struct Ivf {
    fourcc: [u8; 4],
    width: u16,
    height: u16,
    tb_den: u32,
    tb_num: u32,
    frames: Vec<(u64, Vec<u8>)>,
}

fn read_ivf(path: &str) -> R<Ivf> {
    let d = fs::read(path)?;
    if d.len() < 32 || &d[..4] != b"DKIF" {
        return Err("not an IVF file".into());
    }
    let hlen = u16::from_le_bytes([d[6], d[7]]) as usize;
    let u32le = |o: usize| u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]);
    let mut ivf = Ivf {
        fourcc: [d[8], d[9], d[10], d[11]],
        width: u16::from_le_bytes([d[12], d[13]]),
        height: u16::from_le_bytes([d[14], d[15]]),
        tb_den: u32le(16),
        tb_num: u32le(20),
        frames: Vec::new(),
    };
    let mut p = hlen.max(32);
    while p + 12 <= d.len() {
        let n = u32le(p) as usize;
        let pts = u64::from_le_bytes(d[p + 4..p + 12].try_into()?);
        p += 12;
        if n > d.len() - p {
            return Err("truncated IVF frame".into());
        }
        ivf.frames.push((pts, d[p..p + n].to_vec()));
        p += n;
    }
    Ok(ivf)
}

fn write_ivf(path: &str, ivf: &Ivf) -> R<()> {
    let mut o = Vec::new();
    o.extend_from_slice(b"DKIF");
    o.extend_from_slice(&0u16.to_le_bytes());
    o.extend_from_slice(&32u16.to_le_bytes());
    o.extend_from_slice(&ivf.fourcc);
    o.extend_from_slice(&ivf.width.to_le_bytes());
    o.extend_from_slice(&ivf.height.to_le_bytes());
    o.extend_from_slice(&ivf.tb_den.to_le_bytes());
    o.extend_from_slice(&ivf.tb_num.to_le_bytes());
    o.extend_from_slice(&(ivf.frames.len() as u32).to_le_bytes());
    o.extend_from_slice(&0u32.to_le_bytes());
    for (pts, f) in &ivf.frames {
        o.extend_from_slice(&(f.len() as u32).to_le_bytes());
        o.extend_from_slice(&pts.to_le_bytes());
        o.extend_from_slice(f);
    }
    fs::write(path, o)?;
    Ok(())
}

fn au(codec: Codec, tb: Timebase, seq: u64, pts: i64, payload: Vec<u8>) -> AccessUnit {
    AccessUnit {
        track: TrackId(0),
        codec,
        epoch: 0,
        sequence: seq,
        config_generation: 0,
        pts,
        dts: pts,
        duration: 0,
        timebase: tb,
        keyframe: false,
        discontinuity: false,
        capture_time_ns: None,
        payload: Bytes::from(payload),
    }
}

fn decode(reg: &ProviderRegistry, codec: Codec, input: &str, output: &str) -> R<()> {
    let ivf = read_ivf(input)?;
    let tb = Timebase::new(ivf.tb_num.max(1), ivf.tb_den.max(1));
    let (id, mut dec) = reg.create_decoder(&DecoderConfig::new(codec), &Policy::royalty_free())?;
    let mut out = Vec::new();
    let mut n = 0;
    let mut take = |f: Frame, out: &mut Vec<u8>| -> R<()> {
        let v = f
            .as_video()
            .ok_or("not video")?
            .convert(PixelFormat::I420)?;
        out.extend_from_slice(&v.to_packed()?);
        n += 1;
        Ok(())
    };
    for (i, (pts, data)) in ivf.frames.iter().enumerate() {
        // AV1 の IVF は Temporal Delimiter を含むので、内部形式（TD なし）にそろえる。
        let payload = if codec == Codec::Av1 {
            senca_codec::bitstream::av1::to_internal(data)?
        } else {
            data.clone()
        };
        dec.send(&au(codec, tb, i as u64, *pts as i64, payload))?;
        while let Some(f) = dec.receive()? {
            take(f, &mut out)?;
        }
    }
    dec.flush()?;
    while let Some(f) = dec.receive()? {
        take(f, &mut out)?;
    }
    fs::write(output, out)?;
    eprintln!("decoded {n} frames with {}", id.as_str());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn encode(
    reg: &ProviderRegistry,
    codec: Codec,
    w: u32,
    h: u32,
    fps: u32,
    input: &str,
    output: &str,
    bitrate: u32,
    low_latency: bool,
) -> R<()> {
    let raw = fs::read(input)?;
    let fsize = PixelFormat::I420.packed_size(w, h);
    let tb = Timebase::new(1, fps);
    let mut cfg = VideoEncoderConfig::new(codec, w, h, fps, bitrate);
    cfg.timebase = tb;
    cfg.gop = fps;
    if !low_latency {
        cfg.low_latency = false;
        cfg.b_frames = 3;
        cfg.rate_control = RateControl::Vbr;
    }
    let (id, mut enc) = reg.create_encoder(&EncoderConfig::Video(cfg), &Policy::royalty_free())?;
    let mut frames = Vec::new();
    let take = |a: AccessUnit, frames: &mut Vec<(u64, Vec<u8>)>| {
        let data = if codec == Codec::Av1 {
            senca_codec::bitstream::av1::with_temporal_delimiter(&a.payload)
        } else {
            a.payload.to_vec()
        };
        frames.push((a.pts as u64, data));
    };
    for (i, chunk) in raw.chunks_exact(fsize).enumerate() {
        let f = VideoFrame::from_packed(
            PixelFormat::I420,
            w,
            h,
            Bytes::copy_from_slice(chunk),
            i as i64,
            tb,
        )?;
        enc.send(&Frame::Video(f))?;
        while let Some(a) = enc.receive()? {
            take(a, &mut frames);
        }
    }
    enc.flush()?;
    while let Some(a) = enc.receive()? {
        take(a, &mut frames);
    }
    let fourcc = match codec {
        Codec::Vp8 => *b"VP80",
        Codec::Vp9 => *b"VP90",
        _ => *b"AV01",
    };
    let n = frames.len();
    write_ivf(
        output,
        &Ivf {
            fourcc,
            width: w as u16,
            height: h as u16,
            tb_den: fps,
            tb_num: 1,
            frames,
        },
    )?;
    eprintln!("encoded {n} frames with {}", id.as_str());
    Ok(())
}

// ---- Ogg（Opus）----

fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn ogg_page(out: &mut Vec<u8>, header_type: u8, granule: u64, seq: u32, packet: &[u8]) {
    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0);
    out.push(header_type);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&0x5345_4e43u32.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    let mut lacing = vec![255u8; packet.len() / 255];
    lacing.push((packet.len() % 255) as u8);
    out.push(lacing.len() as u8);
    out.extend_from_slice(&lacing);
    out.extend_from_slice(packet);
    let crc = ogg_crc(&out[start..]);
    out[start + 22..start + 26].copy_from_slice(&crc.to_le_bytes());
}

fn read_ogg_packets(path: &str) -> R<Vec<Vec<u8>>> {
    let d = fs::read(path)?;
    let mut p = 0;
    let mut packets = Vec::new();
    let mut cur = Vec::new();
    while p + 27 <= d.len() {
        if &d[p..p + 4] != b"OggS" {
            return Err("bad Ogg page".into());
        }
        let nseg = d[p + 26] as usize;
        let table = d.get(p + 27..p + 27 + nseg).ok_or("truncated Ogg page")?;
        let mut q = p + 27 + nseg;
        for &l in table {
            let seg = d.get(q..q + l as usize).ok_or("truncated Ogg segment")?;
            cur.extend_from_slice(seg);
            q += l as usize;
            if l < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        p = q;
    }
    Ok(packets)
}

fn opus_encode(
    reg: &ProviderRegistry,
    rate: u32,
    channels: u8,
    input: &str,
    output: &str,
    bitrate: u32,
) -> R<()> {
    let raw = fs::read(input)?;
    let cfg = AudioEncoderConfig {
        input_format: SampleFormat::I16,
        ..AudioEncoderConfig::opus(rate, channels, bitrate)
    };
    let (_, mut enc) = reg.create_encoder(&EncoderConfig::Audio(cfg), &Policy::royalty_free())?;
    let head: CodecConfig = enc.codec_config().ok_or("no OpusHead")?;
    let mut pkts = Vec::new();
    let unit = 2 * channels as usize * (rate as usize / 100);
    for (i, chunk) in raw.chunks(unit).enumerate() {
        let usable = chunk.len() - chunk.len() % (2 * channels as usize);
        let f = AudioFrame {
            format: SampleFormat::I16,
            sample_rate: rate,
            channels,
            data: Bytes::copy_from_slice(&chunk[..usable]),
            pts: (i * rate as usize / 100) as i64,
            timebase: Timebase::new(1, rate),
        };
        enc.send(&Frame::Audio(f))?;
        while let Some(a) = enc.receive()? {
            pkts.push(a);
        }
    }
    enc.flush()?;
    while let Some(a) = enc.receive()? {
        pkts.push(a);
    }
    let mut out = Vec::new();
    ogg_page(&mut out, 0x02, 0, 0, &head.record);
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&5u32.to_le_bytes());
    tags.extend_from_slice(b"senca");
    tags.extend_from_slice(&0u32.to_le_bytes());
    ogg_page(&mut out, 0, 0, 1, &tags);
    let pre_skip = u16::from_le_bytes([head.record[10], head.record[11]]) as u64;
    // 最後のページの granule は「実際の長さ + pre_skip」（末尾の詰め物を切る）。
    let total = (raw.len() / (2 * channels as usize)) as u64 * 48000 / rate as u64 + pre_skip;
    let mut granule = 0u64;
    let n = pkts.len();
    for (i, a) in pkts.iter().enumerate() {
        granule += a.duration;
        let last = i + 1 == n;
        let g = if last { total.min(granule) } else { granule };
        ogg_page(
            &mut out,
            if last { 0x04 } else { 0 },
            g,
            i as u32 + 2,
            &a.payload,
        );
    }
    fs::write(output, out)?;
    eprintln!("encoded {n} opus packets");
    Ok(())
}

fn opus_decode(reg: &ProviderRegistry, input: &str, output: &str) -> R<()> {
    let packets = read_ogg_packets(input)?;
    if packets.len() < 2 || !packets[0].starts_with(b"OpusHead") {
        return Err("not an Ogg Opus file".into());
    }
    let head = senca_codec::bitstream::opus::parse_opus_head(&packets[0])?;
    let mut dc = DecoderConfig::new(Codec::Opus);
    dc.sample_format = SampleFormat::I16;
    dc.config = Some(CodecConfig {
        codec: Codec::Opus,
        generation: 0,
        record: Bytes::from(packets[0].clone()),
        video: None,
        audio: None,
    });
    let (_, mut dec) = reg.create_decoder(&dc, &Policy::royalty_free())?;
    let mut out = Vec::new();
    let tb = Timebase::new(1, 48000);
    for (i, p) in packets[2..].iter().enumerate() {
        dec.send(&au(Codec::Opus, tb, i as u64, 0, p.clone()))?;
        while let Some(f) = dec.receive()? {
            out.extend_from_slice(&f.as_audio().ok_or("not audio")?.data);
        }
    }
    fs::write(output, out)?;
    eprintln!(
        "decoded {} packets ({} ch)",
        packets.len() - 2,
        head.channels
    );
    Ok(())
}
