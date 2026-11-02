//! QPACK（RFC 9204）。
//!
//! - [`Encoder`]: 静的テーブルとリテラルだけで符号化する（動的テーブルを使わない。Required Insert Count は常に 0）。
//! - [`Decoder`]: 相手の encoder stream の命令で動的テーブルを作り、field section を復号する。
//!   動的テーブルの容量・blocked streams・field section の大きさに上限を置く。

use std::collections::{BTreeMap, VecDeque};

use crate::headers::{Field, Headers};
use crate::huffman;
use crate::static_table::STATIC;

/// QPACK の誤り。種別で接続エラーのコードが変わる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QpackError {
    /// field section の復号に失敗（QPACK_DECOMPRESSION_FAILED）。
    Decompression(&'static str),
    /// encoder stream の誤り（QPACK_ENCODER_STREAM_ERROR）。
    EncoderStream(&'static str),
    /// decoder stream の誤り（QPACK_DECODER_STREAM_ERROR）。
    DecoderStream(&'static str),
    /// field section が上限（SETTINGS_MAX_FIELD_SECTION_SIZE）を超えた（stream エラー）。
    TooLarge,
}

type R<T> = Result<T, QpackError>;

// ---------------------------------------------------------------------------
// 整数と文字列（RFC 7541 5.1・5.2）
// ---------------------------------------------------------------------------

/// N ビット前置の整数を書く。`first` は前置の上位ビット（フラグ）。
pub(crate) fn put_int(out: &mut Vec<u8>, first: u8, n: u8, v: u64) {
    let max = (1u64 << n) - 1;
    if v < max {
        out.push(first | v as u8);
        return;
    }
    out.push(first | max as u8);
    let mut r = v - max;
    while r >= 128 {
        out.push((r % 128) as u8 | 0x80);
        r /= 128;
    }
    out.push(r as u8);
}

/// N ビット前置の整数を読む。`(値, 消費バイト数)`。足りなければ `Ok(None)`。2^62 を超えたら誤り。
pub(crate) fn get_int(buf: &[u8], n: u8) -> Result<Option<(u64, usize)>, ()> {
    let Some(&b0) = buf.first() else { return Ok(None) };
    let max = (1u64 << n) - 1;
    let v = u64::from(b0) & max;
    if v < max {
        return Ok(Some((v, 1)));
    }
    let mut v = max;
    let mut shift = 0u32;
    for (i, &b) in buf[1..].iter().enumerate() {
        if shift > 56 {
            return Err(());
        }
        v = v.checked_add(u64::from(b & 0x7f) << shift).ok_or(())?;
        if v > 1 << 62 {
            return Err(());
        }
        shift += 7;
        if b & 0x80 == 0 {
            return Ok(Some((v, i + 2)));
        }
    }
    Ok(None)
}

/// 文字列リテラルを書く（Huffman を使わない）。`first` は H ビットより上のフラグ、`n` は長さの前置ビット数。
fn put_str(out: &mut Vec<u8>, first: u8, n: u8, s: &[u8]) {
    put_int(out, first, n, s.len() as u64);
    out.extend_from_slice(s);
}

/// 前から読む。
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }
    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }
    fn int(&mut self, n: u8, e: QpackError) -> R<Option<u64>> {
        match get_int(&self.buf[self.pos..], n) {
            Ok(Some((v, l))) => {
                self.pos += l;
                Ok(Some(v))
            }
            Ok(None) => Ok(None),
            Err(()) => Err(e),
        }
    }
    /// 文字列。`h_bit` は H フラグのビット位置のマスク、`n` は長さの前置ビット数。
    fn string(&mut self, h_mask: u8, n: u8, max: usize, e: QpackError) -> R<Option<Vec<u8>>> {
        let Some(b0) = self.peek() else { return Ok(None) };
        let huff = b0 & h_mask != 0;
        let save = self.pos;
        let Some(len) = self.int(n, e.clone())? else { return Ok(None) };
        let len = usize::try_from(len).map_err(|_| e.clone())?;
        if len > max.max(1) * 2 {
            return Err(QpackError::TooLarge);
        }
        if self.buf.len() - self.pos < len {
            self.pos = save;
            return Ok(None);
        }
        let raw = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        let s = if huff {
            huffman::decode(raw, max).map_err(|_| e)?
        } else {
            if raw.len() > max {
                return Err(QpackError::TooLarge);
            }
            raw.to_vec()
        };
        Ok(Some(s))
    }
}

// ---------------------------------------------------------------------------
// encoder（静的テーブルとリテラルだけ）
// ---------------------------------------------------------------------------

/// 静的テーブルとリテラルだけで符号化する encoder。
#[derive(Debug, Default, Clone)]
pub struct Encoder;

impl Encoder {
    /// field section を符号化する。
    pub fn encode(&self, fields: &Headers) -> Vec<u8> {
        let mut out = vec![0x00, 0x00]; // Required Insert Count = 0、Base = 0
        for f in fields.iter() {
            let mut name_idx = None;
            let mut full_idx = None;
            for (i, (n, v)) in STATIC.iter().enumerate() {
                if n.as_bytes() == f.name.as_slice() {
                    if v.as_bytes() == f.value.as_slice() {
                        full_idx = Some(i);
                        break;
                    }
                    if name_idx.is_none() {
                        name_idx = Some(i);
                    }
                }
            }
            if let Some(i) = full_idx {
                put_int(&mut out, 0xc0, 6, i as u64); // 1 T=1 index
            } else if let Some(i) = name_idx {
                put_int(&mut out, 0x50, 4, i as u64); // 01 N=0 T=1 index
                put_str(&mut out, 0x00, 7, &f.value);
            } else {
                put_str(&mut out, 0x20, 3, &f.name); // 001 N=0 H=0
                put_str(&mut out, 0x00, 7, &f.value);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// decoder（動的テーブルあり）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Entry {
    name: Vec<u8>,
    value: Vec<u8>,
}

impl Entry {
    fn size(&self) -> usize {
        self.name.len() + self.value.len() + 32
    }
}

/// 動的テーブル（RFC 9204 3.2）。
#[derive(Debug, Default)]
struct DynTable {
    entries: VecDeque<Entry>,
    /// 先頭（最も古い）の絶対 index。
    dropped: u64,
    size: usize,
    capacity: usize,
}

impl DynTable {
    fn insert_count(&self) -> u64 {
        self.dropped + self.entries.len() as u64
    }

    fn get(&self, abs: u64) -> Option<&Entry> {
        let i = abs.checked_sub(self.dropped)?;
        self.entries.get(usize::try_from(i).ok()?)
    }

    fn evict_to(&mut self, cap: usize) {
        while self.size > cap {
            let Some(e) = self.entries.pop_front() else { break };
            self.size -= e.size();
            self.dropped += 1;
        }
    }

    fn insert(&mut self, e: Entry) -> R<()> {
        if e.size() > self.capacity {
            return Err(QpackError::EncoderStream("項目が容量を超える"));
        }
        self.evict_to(self.capacity - e.size());
        self.size += e.size();
        self.entries.push_back(e);
        Ok(())
    }
}

/// 復号の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    Done(Headers),
    /// 動的テーブルの挿入を待つ（blocked stream）。
    Blocked,
}

/// QPACK の decoder。
#[derive(Debug)]
pub struct Decoder {
    table: DynTable,
    max_capacity: usize,
    max_blocked: usize,
    max_field_section: usize,
    /// 相手の encoder stream の受信バッファ。
    enc_buf: Vec<u8>,
    /// decoder stream へ送る命令。
    out: Vec<u8>,
    /// encoder へ ACK した挿入数（Section ACK・Insert Count Increment）。
    acked: u64,
    /// blocked stream: stream ID → Required Insert Count。
    blocked: BTreeMap<u64, u64>,
}

impl Decoder {
    /// `max_capacity` は SETTINGS_QPACK_MAX_TABLE_CAPACITY、`max_blocked` は SETTINGS_QPACK_BLOCKED_STREAMS、
    /// `max_field_section` は SETTINGS_MAX_FIELD_SECTION_SIZE として広告する値。
    pub fn new(max_capacity: usize, max_blocked: usize, max_field_section: usize) -> Self {
        Self {
            table: DynTable::default(),
            max_capacity,
            max_blocked,
            max_field_section,
            enc_buf: Vec::new(),
            out: Vec::new(),
            acked: 0,
            blocked: BTreeMap::new(),
        }
    }

    /// decoder stream へ送る命令を取り出す。
    pub fn take_instructions(&mut self) -> Vec<u8> {
        // 未 ACK の挿入があれば Insert Count Increment（RFC 9204 4.4.3）
        let ic = self.table.insert_count();
        if ic > self.acked {
            put_int(&mut self.out, 0x00, 6, ic - self.acked);
            self.acked = ic;
        }
        std::mem::take(&mut self.out)
    }

    /// stream が中断された（Stream Cancellation、RFC 9204 4.4.2）。
    pub fn cancel_stream(&mut self, stream_id: u64) {
        if self.blocked.remove(&stream_id).is_some() || self.max_capacity > 0 {
            put_int(&mut self.out, 0x40, 6, stream_id);
        }
    }

    /// 相手の encoder stream のデータ。
    pub fn on_encoder_stream(&mut self, data: &[u8]) -> R<()> {
        if self.enc_buf.len() + data.len() > 64 * 1024 + self.max_capacity {
            return Err(QpackError::EncoderStream("encoder stream の命令が大きすぎる"));
        }
        self.enc_buf.extend_from_slice(data);
        let buf = std::mem::take(&mut self.enc_buf);
        let mut c = Cursor::new(&buf);
        let e = || QpackError::EncoderStream("encoder stream の命令が不正");
        let max_str = self.max_capacity.max(1);
        loop {
            let start = c.pos;
            let Some(b) = c.peek() else { break };
            let done = if b & 0x80 != 0 {
                // Insert with Name Reference
                let is_static = b & 0x40 != 0;
                let Some(idx) = c.int(6, e())? else { break };
                let Some(value) = c.string(0x80, 7, max_str, e())? else {
                    c.pos = start;
                    break;
                };
                let name = if is_static {
                    STATIC.get(idx as usize).ok_or_else(e)?.0.as_bytes().to_vec()
                } else {
                    let abs = self.table.insert_count().checked_sub(1 + idx).ok_or_else(e)?;
                    self.table.get(abs).ok_or_else(e)?.name.clone()
                };
                self.table.insert(Entry { name, value })?;
                true
            } else if b & 0x40 != 0 {
                // Insert with Literal Name
                let Some(name) = c.string(0x20, 5, max_str, e())? else { break };
                let Some(value) = c.string(0x80, 7, max_str, e())? else {
                    c.pos = start;
                    break;
                };
                self.table.insert(Entry { name, value })?;
                true
            } else if b & 0x20 != 0 {
                // Set Dynamic Table Capacity
                let Some(cap) = c.int(5, e())? else { break };
                if cap > self.max_capacity as u64 {
                    return Err(QpackError::EncoderStream("容量が広告した上限を超える"));
                }
                self.table.capacity = cap as usize;
                self.table.evict_to(cap as usize);
                true
            } else {
                // Duplicate
                let Some(idx) = c.int(5, e())? else { break };
                let abs = self.table.insert_count().checked_sub(1 + idx).ok_or_else(e)?;
                let en = self.table.get(abs).ok_or_else(e)?.clone();
                self.table.insert(en)?;
                true
            };
            if !done {
                break;
            }
            if c.is_empty() {
                break;
            }
        }
        self.enc_buf = buf[c.pos..].to_vec();
        Ok(())
    }

    /// blocked の stream のうち、今復号できるもの。
    pub fn unblocked(&mut self) -> Vec<u64> {
        let ic = self.table.insert_count();
        let ready: Vec<u64> = self.blocked.iter().filter(|(_, ric)| **ric <= ic).map(|(s, _)| *s).collect();
        for s in &ready {
            self.blocked.remove(s);
        }
        ready
    }

    fn decode_ric(&self, enc: u64) -> R<u64> {
        if enc == 0 {
            return Ok(0);
        }
        let max_entries = (self.max_capacity / 32) as u64;
        let full = 2 * max_entries;
        if full == 0 || enc > full {
            return Err(QpackError::Decompression("Required Insert Count が不正"));
        }
        let total = self.table.insert_count();
        let max_value = total + max_entries;
        let max_wrapped = (max_value / full) * full;
        let mut ric = max_wrapped + enc - 1;
        if ric > max_value {
            if ric <= full {
                return Err(QpackError::Decompression("Required Insert Count が不正"));
            }
            ric -= full;
        }
        if ric == 0 {
            return Err(QpackError::Decompression("Required Insert Count が 0"));
        }
        Ok(ric)
    }

    /// field section を復号する。動的テーブルが足りなければ `Blocked`（後で同じ入力で呼び直す）。
    pub fn decode(&mut self, stream_id: u64, block: &[u8]) -> R<Decoded> {
        let d = || QpackError::Decompression("field section が不正");
        let mut c = Cursor::new(block);
        let enc_ric = c.int(8, d())?.ok_or_else(d)?;
        let ric = self.decode_ric(enc_ric)?;
        let sign = c.peek().ok_or_else(d)? & 0x80 != 0;
        let delta = c.int(7, d())?.ok_or_else(d)?;
        let base = if sign {
            ric.checked_sub(delta + 1).ok_or_else(d)?
        } else {
            ric.checked_add(delta).ok_or_else(d)?
        };
        if ric > self.table.insert_count() {
            if !self.blocked.contains_key(&stream_id) && self.blocked.len() >= self.max_blocked {
                return Err(QpackError::Decompression("blocked streams の上限を超えた"));
            }
            self.blocked.insert(stream_id, ric);
            return Ok(Decoded::Blocked);
        }
        self.blocked.remove(&stream_id);
        let max = self.max_field_section;
        let mut fields = Headers::new();
        let mut total = 0usize;
        let dynamic = |abs: u64, s: &Self| -> R<Entry> {
            if abs >= ric {
                return Err(QpackError::Decompression("Required Insert Count を超える参照"));
            }
            s.table
                .get(abs)
                .cloned()
                .ok_or(QpackError::Decompression("動的テーブルにない項目"))
        };
        while !c.is_empty() {
            let b = c.peek().ok_or_else(d)?;
            let (name, value) = if b & 0x80 != 0 {
                // Indexed Field Line
                let st = b & 0x40 != 0;
                let idx = c.int(6, d())?.ok_or_else(d)?;
                if st {
                    let (n, v) = STATIC.get(idx as usize).ok_or_else(d)?;
                    (n.as_bytes().to_vec(), v.as_bytes().to_vec())
                } else {
                    let abs = base.checked_sub(1 + idx).ok_or_else(d)?;
                    let en = dynamic(abs, self)?;
                    (en.name, en.value)
                }
            } else if b & 0x40 != 0 {
                // Literal with Name Reference
                let st = b & 0x10 != 0;
                let idx = c.int(4, d())?.ok_or_else(d)?;
                let name = if st {
                    STATIC.get(idx as usize).ok_or_else(d)?.0.as_bytes().to_vec()
                } else {
                    let abs = base.checked_sub(1 + idx).ok_or_else(d)?;
                    dynamic(abs, self)?.name
                };
                let value = c.string(0x80, 7, max, d())?.ok_or_else(d)?;
                (name, value)
            } else if b & 0x20 != 0 {
                // Literal with Literal Name
                let name = c.string(0x08, 3, max, d())?.ok_or_else(d)?;
                let value = c.string(0x80, 7, max, d())?.ok_or_else(d)?;
                (name, value)
            } else if b & 0x10 != 0 {
                // Indexed Field Line with Post-Base Index
                let idx = c.int(4, d())?.ok_or_else(d)?;
                let abs = base.checked_add(idx).ok_or_else(d)?;
                let en = dynamic(abs, self)?;
                (en.name, en.value)
            } else {
                // Literal with Post-Base Name Reference
                let idx = c.int(3, d())?.ok_or_else(d)?;
                let abs = base.checked_add(idx).ok_or_else(d)?;
                let name = dynamic(abs, self)?.name;
                let value = c.string(0x80, 7, max, d())?.ok_or_else(d)?;
                (name, value)
            };
            total += name.len() + value.len() + 32;
            if total > max {
                return Err(QpackError::TooLarge);
            }
            fields.push(Field { name, value });
        }
        if ric > 0 {
            // Section Acknowledgment（RFC 9204 4.4.1）
            put_int(&mut self.out, 0x80, 7, stream_id);
            self.acked = self.acked.max(ric);
        }
        Ok(Decoded::Done(fields))
    }

    /// 相手の decoder stream（自分の encoder への命令）。動的テーブルを使わないので、
    /// Section Acknowledgment・Insert Count Increment は誤り、Stream Cancellation は無視する。
    pub fn on_decoder_stream(buf: &mut Vec<u8>, data: &[u8]) -> R<()> {
        buf.extend_from_slice(data);
        let e = || QpackError::DecoderStream("decoder stream の命令が不正");
        let mut pos = 0;
        while pos < buf.len() {
            let b = buf[pos];
            if b & 0x80 != 0 {
                return Err(QpackError::DecoderStream("動的テーブルを使っていないのに Section Acknowledgment"));
            }
            match get_int(&buf[pos..], 6) {
                Ok(Some((v, l))) => {
                    if b & 0x40 == 0 && v > 0 {
                        return Err(QpackError::DecoderStream("挿入していないのに Insert Count Increment"));
                    }
                    if b & 0x40 == 0 && v == 0 {
                        return Err(e());
                    }
                    pos += l;
                }
                Ok(None) => break,
                Err(()) => return Err(e()),
            }
        }
        buf.drain(..pos);
        if buf.len() > 64 {
            return Err(e());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> Headers {
        let mut v = Headers::new();
        for (n, val) in pairs {
            v.push(Field::new(n.as_bytes(), val.as_bytes()));
        }
        v
    }

    #[test]
    fn integer_codec() {
        // RFC 7541 C.1
        let mut o = Vec::new();
        put_int(&mut o, 0, 5, 10);
        assert_eq!(o, [10]);
        o.clear();
        put_int(&mut o, 0, 5, 1337);
        assert_eq!(o, [31, 154, 10]);
        assert_eq!(get_int(&o, 5), Ok(Some((1337, 3))));
        assert_eq!(get_int(&[31, 154], 5), Ok(None));
        assert!(get_int(&[31, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f], 5).is_err());
    }

    #[test]
    fn static_roundtrip() {
        let f = h(&[(":method", "GET"), (":path", "/x"), (":authority", "a.example"), ("x-custom", "v"), ("accept", "*/*")]);
        let enc = Encoder.encode(&f);
        let mut d = Decoder::new(0, 0, 32 * 1024);
        assert_eq!(d.decode(0, &enc).unwrap(), Decoded::Done(f));
    }

    #[test]
    fn rfc9204_b2_dynamic_table() {
        // 付録 B.2: encoder stream で 2 項目を挿入し、それを参照する field section
        let mut d = Decoder::new(220, 1, 32 * 1024);
        // stream 4 の field section は先に届く → blocked
        let section = [0x03, 0x81, 0x10, 0x11];
        assert_eq!(d.decode(4, &section).unwrap(), Decoded::Blocked);
        let enc = [
            0x3f, 0xbd, 0x01, // Set Dynamic Table Capacity=220
            0xc0, 0x0f, 0x77, 0x77, 0x77, 0x2e, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e, 0x63, 0x6f, 0x6d,
            0xc1, 0x0c, 0x2f, 0x73, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2f, 0x70, 0x61, 0x74, 0x68,
        ];
        d.on_encoder_stream(&enc).unwrap();
        assert_eq!(d.unblocked(), vec![4]);
        let got = d.decode(4, &section).unwrap();
        assert_eq!(
            got,
            Decoded::Done(h(&[(":authority", "www.example.com"), (":path", "/sample/path")]))
        );
        // Section Acknowledgment (stream 4)
        assert_eq!(d.take_instructions(), vec![0x84]);
    }

    #[test]
    fn rejects_bad_input() {
        let mut d = Decoder::new(0, 0, 64);
        // 動的テーブルなしで RIC > 0
        assert!(d.decode(0, &[0x02, 0x00]).is_err());
        // 静的 index の範囲外
        assert!(d.decode(0, &[0x00, 0x00, 0xff, 0x30]).is_err());
        // 大きすぎる
        let big = h(&[("x", &"a".repeat(100))]);
        assert_eq!(d.decode(0, &Encoder.encode(&big)), Err(QpackError::TooLarge));
        // 容量の上限を超える設定
        let mut d = Decoder::new(100, 1, 1000);
        assert!(d.on_encoder_stream(&[0x3f, 0xbd, 0x01]).is_err());
    }
}
