//! 例の共通部品（std だけ）: 小さな HTTP/1.1 サーバー、IVF・Ogg Opus・Annex B・AV1 OBU の読み書き。
//! 試験用。製品の HTTP は senca-node が持つ。

#![allow(dead_code)]
#![allow(clippy::type_complexity)]

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use bytes::Bytes;

// ------------------------------------------------------------------ HTTP

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, ctype: &str, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            headers: vec![("Content-Type".into(), ctype.into())],
            body: body.into(),
        }
    }
    pub fn empty(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
    pub fn header(mut self, k: &str, v: &str) -> Response {
        self.headers.push((k.into(), v.into()));
        self
    }
}

pub type HttpMsg = (Request, mpsc::Sender<Response>);

/// HTTP を受け、要求を `tx` へ送り、応答を待って返す。
pub fn serve_http(addr: &str, tx: mpsc::Sender<HttpMsg>) -> std::io::Result<()> {
    let l = TcpListener::bind(addr)?;
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = handle_conn(s, tx);
            });
        }
    });
    Ok(())
}

fn handle_conn(mut s: TcpStream, tx: mpsc::Sender<HttpMsg>) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .min(1 << 20);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = s.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(len);
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let (rtx, rrx) = mpsc::channel();
    let req = Request {
        method,
        path: path.to_string(),
        query,
        headers,
        body,
    };
    if tx.send((req, rtx)).is_err() {
        return Ok(());
    }
    let resp = rrx
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|_| Response::empty(503));
    let reason = match resp.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Status",
    };
    let mut out = format!("HTTP/1.1 {} {reason}\r\n", resp.status);
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        resp.body.len()
    ));
    s.write_all(out.as_bytes())?;
    s.write_all(&resp.body)?;
    Ok(())
}

pub fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

// ------------------------------------------------------------------ IVF

pub struct IvfWriter {
    f: BufWriter<File>,
    frames: u32,
}

impl IvfWriter {
    pub fn create(path: &str, fourcc: &[u8; 4], w: u16, h: u16) -> std::io::Result<IvfWriter> {
        let mut f = BufWriter::new(File::create(path)?);
        let mut hd = Vec::with_capacity(32);
        hd.extend_from_slice(b"DKIF");
        hd.extend_from_slice(&0u16.to_le_bytes());
        hd.extend_from_slice(&32u16.to_le_bytes());
        hd.extend_from_slice(fourcc);
        hd.extend_from_slice(&w.to_le_bytes());
        hd.extend_from_slice(&h.to_le_bytes());
        hd.extend_from_slice(&90000u32.to_le_bytes());
        hd.extend_from_slice(&1u32.to_le_bytes());
        hd.extend_from_slice(&0u32.to_le_bytes());
        hd.extend_from_slice(&0u32.to_le_bytes());
        f.write_all(&hd)?;
        Ok(IvfWriter { f, frames: 0 })
    }

    pub fn write(&mut self, pts: i64, data: &[u8]) -> std::io::Result<()> {
        self.f.write_all(&(data.len() as u32).to_le_bytes())?;
        self.f.write_all(&(pts as u64).to_le_bytes())?;
        self.f.write_all(data)?;
        self.frames += 1;
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<u32> {
        self.f.flush()?;
        let mut f = self.f.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(24))?;
        f.write_all(&self.frames.to_le_bytes())?;
        Ok(self.frames)
    }
}

/// IVF を読む（fourcc, フレームの列 (pts, data)）。
pub fn read_ivf(path: &str) -> std::io::Result<([u8; 4], Vec<(u64, Bytes)>)> {
    let d = std::fs::read(path)?;
    if d.len() < 32 || &d[..4] != b"DKIF" {
        return Err(std::io::Error::other("not IVF"));
    }
    let hl = u16::from_le_bytes([d[6], d[7]]) as usize;
    let fourcc = [d[8], d[9], d[10], d[11]];
    let mut p = hl;
    let mut out = Vec::new();
    let all = Bytes::from(d);
    while p + 12 <= all.len() {
        let n = u32::from_le_bytes([all[p], all[p + 1], all[p + 2], all[p + 3]]) as usize;
        let pts = u64::from_le_bytes(all[p + 4..p + 12].try_into().unwrap_or([0; 8]));
        p += 12;
        if p + n > all.len() {
            break;
        }
        out.push((pts, all.slice(p..p + n)));
        p += n;
    }
    Ok((fourcc, out))
}

// ------------------------------------------------------------------ Ogg Opus

fn ogg_crc(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &b in data {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub struct OggOpusWriter {
    f: BufWriter<File>,
    serial: u32,
    seq: u32,
    granule: u64,
    packets: u32,
    held: Option<(Vec<u8>, u64)>,
}

impl OggOpusWriter {
    pub fn create(path: &str) -> std::io::Result<OggOpusWriter> {
        let mut w = OggOpusWriter {
            f: BufWriter::new(File::create(path)?),
            serial: 0x5E4C_A001,
            seq: 0,
            granule: 0,
            packets: 0,
            held: None,
        };
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(2);
        head.extend_from_slice(&312u16.to_le_bytes());
        head.extend_from_slice(&48000u32.to_le_bytes());
        head.extend_from_slice(&0u16.to_le_bytes());
        head.push(0);
        w.page(&head, 0, 0x02)?;
        let mut tags = b"OpusTags".to_vec();
        let vendor = b"senca-webrtc";
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());
        w.page(&tags, 0, 0)?;
        Ok(w)
    }

    fn page(&mut self, data: &[u8], granule: u64, flags: u8) -> std::io::Result<()> {
        let mut segs = Vec::new();
        let mut n = data.len();
        loop {
            let s = n.min(255);
            segs.push(s as u8);
            n -= s;
            if s < 255 {
                break;
            }
        }
        let mut p = Vec::with_capacity(27 + segs.len() + data.len());
        p.extend_from_slice(b"OggS");
        p.push(0);
        p.push(flags);
        p.extend_from_slice(&granule.to_le_bytes());
        p.extend_from_slice(&self.serial.to_le_bytes());
        p.extend_from_slice(&self.seq.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.push(segs.len() as u8);
        p.extend_from_slice(&segs);
        p.extend_from_slice(data);
        let c = ogg_crc(&p);
        p[22..26].copy_from_slice(&c.to_le_bytes());
        self.seq += 1;
        self.f.write_all(&p)
    }

    pub fn write(&mut self, samples48k: u64, data: &[u8]) -> std::io::Result<()> {
        if data.len() > 255 * 254 {
            return Ok(());
        }
        self.granule += samples48k;
        self.packets += 1;
        // 最後の packet に EOS を付けるため、1 つ遅らせて書く
        if let Some((d, g)) = self.held.take() {
            self.page(&d, g, 0)?;
        }
        self.held = Some((data.to_vec(), self.granule));
        Ok(())
    }

    pub fn finish(mut self) -> std::io::Result<u32> {
        if let Some((d, g)) = self.held.take() {
            self.page(&d, g, 0x04)?;
        }
        self.f.flush()?;
        Ok(self.packets)
    }
}

// ------------------------------------------------------------------ H.264 Annex B

/// Annex B の NAL を取り出す（開始符号を除く）。
pub fn annexb_nals(d: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() {
            starts[k + 1] - 3
        } else {
            d.len()
        };
        while e > s && d[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&d[s..e]);
        }
    }
    out
}

/// Annex B の H.264 を AU（AUD で区切る）にする: (AVCC の payload, キーフレーム, SPS, PPS)。
pub fn h264_aus(d: &[u8]) -> Vec<(Bytes, bool, Option<Vec<u8>>, Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut key = false;
    let mut sps = None;
    let mut pps = None;
    let mut has_vcl = false;
    for nal in annexb_nals(d) {
        let t = nal[0] & 0x1f;
        if t == 9 {
            if has_vcl {
                out.push((
                    Bytes::from(std::mem::take(&mut cur)),
                    key,
                    sps.take(),
                    pps.take(),
                ));
            }
            cur.clear();
            key = false;
            has_vcl = false;
            continue;
        }
        match t {
            7 => sps = Some(nal.to_vec()),
            8 => pps = Some(nal.to_vec()),
            _ => {
                if t == 5 {
                    key = true;
                }
                if (1..=5).contains(&t) {
                    has_vcl = true;
                }
                cur.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                cur.extend_from_slice(nal);
            }
        }
    }
    if has_vcl {
        out.push((Bytes::from(cur), key, sps, pps));
    }
    out
}

/// AVCC（4 byte 長さ）を Annex B にする。`params` はキーフレームの前に入れる SPS・PPS。
pub fn avcc_to_annexb(au: &[u8], params: &[Vec<u8>], out: &mut Vec<u8>) {
    for p in params {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(p);
    }
    let mut i = 0;
    while i + 4 <= au.len() {
        let n = u32::from_be_bytes([au[i], au[i + 1], au[i + 2], au[i + 3]]) as usize;
        i += 4;
        if i + n > au.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&au[i..i + n]);
        i += n;
    }
}

// ------------------------------------------------------------------ AV1 OBU

fn leb128(d: &[u8]) -> Option<(usize, usize)> {
    let mut v = 0usize;
    for i in 0..8 {
        let b = *d.get(i)?;
        v |= ((b & 0x7f) as usize) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// Low Overhead Bitstream Format の OBU 列を temporal unit に分ける: (TD を除いた OBU 列, 順序ヘッダーを含む)。
pub fn av1_temporal_units(d: &[u8]) -> Vec<(Bytes, bool)> {
    let mut out: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut p = 0;
    while p < d.len() {
        let h = d[p];
        let typ = (h >> 3) & 0xf;
        let ext = h & 0x04 != 0;
        let has_size = h & 0x02 != 0;
        let hl = 1 + ext as usize;
        if !has_size {
            break;
        }
        let Some((sz, n)) = leb128(&d[p + hl..]) else {
            break;
        };
        let end = p + hl + n + sz;
        if end > d.len() {
            break;
        }
        if typ == 2 {
            out.push((Vec::new(), false));
        } else if let Some(cur) = out.last_mut() {
            if typ == 1 {
                cur.1 = true;
            }
            cur.0.extend_from_slice(&d[p..end]);
        }
        p = end;
    }
    out.into_iter()
        .filter(|(v, _)| !v.is_empty())
        .map(|(v, k)| (Bytes::from(v), k))
        .collect()
}
