//! AMF0（Action Message Format 0）の符号化と復号。I/O を持たない。
//!
//! 対応する型: number・boolean・string・long string・object・null・undefined・
//! ECMA array・strict array・date・object end。
//! reference・typed object・movieclip・recordset・XML document・unsupported・
//! AMF3 への切り替え（avmplus-object）は [`Error::UnsupportedMarker`] で拒否する。
//!
//! 壊れた入力・巨大な入力に対しては [`Limits`] で入れ子の深さ・要素数・総量を制限する（仕様 9.4）。
//! 要素数の宣言値（ECMA array・strict array の個数）は信用せず、確保量の根拠にしない。

use std::fmt;

/// AMF0 の値。object と ECMA array はキーの順序を保つ。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(f64),
    Boolean(bool),
    /// string と long string はどちらもこれになる。符号化では長さで選ぶ。
    String(String),
    Object(Vec<(String, Value)>),
    Null,
    Undefined,
    EcmaArray(Vec<(String, Value)>),
    StrictArray(Vec<Value>),
    /// UNIX 時刻（ミリ秒）と時差（分。仕様上は 0 を書くことになっている）。
    Date {
        millis: f64,
        timezone: i16,
    },
}

/// 型マーカー。
pub mod marker {
    pub const NUMBER: u8 = 0x00;
    pub const BOOLEAN: u8 = 0x01;
    pub const STRING: u8 = 0x02;
    pub const OBJECT: u8 = 0x03;
    pub const MOVIECLIP: u8 = 0x04;
    pub const NULL: u8 = 0x05;
    pub const UNDEFINED: u8 = 0x06;
    pub const REFERENCE: u8 = 0x07;
    pub const ECMA_ARRAY: u8 = 0x08;
    pub const OBJECT_END: u8 = 0x09;
    pub const STRICT_ARRAY: u8 = 0x0A;
    pub const DATE: u8 = 0x0B;
    pub const LONG_STRING: u8 = 0x0C;
    pub const UNSUPPORTED: u8 = 0x0D;
    pub const RECORDSET: u8 = 0x0E;
    pub const XML_DOCUMENT: u8 = 0x0F;
    pub const TYPED_OBJECT: u8 = 0x10;
    pub const AVMPLUS_OBJECT: u8 = 0x11;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// 入力が途中で終わった。
    Truncated,
    /// 対応しない、または未定義の型マーカー。
    UnsupportedMarker(u8),
    /// object end が値の位置に現れた、またはキーのあとに値がない。
    UnexpectedObjectEnd,
    /// 文字列が UTF-8 でない。
    InvalidUtf8,
    /// 入れ子が [`Limits::max_depth`] を超えた。
    DepthExceeded,
    /// 要素数が [`Limits::max_elements`] を超えた。
    TooManyElements,
    /// 入力が [`Limits::max_size`] を超えた。
    TooLarge,
    /// 符号化できない（キーが 65535 バイトを超える、要素数が u32 を超えるなど）。
    Unencodable,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => f.write_str("amf0: truncated input"),
            Error::UnsupportedMarker(m) => write!(f, "amf0: unsupported marker 0x{m:02x}"),
            Error::UnexpectedObjectEnd => f.write_str("amf0: unexpected object end"),
            Error::InvalidUtf8 => f.write_str("amf0: invalid utf-8 string"),
            Error::DepthExceeded => f.write_str("amf0: nesting too deep"),
            Error::TooManyElements => f.write_str("amf0: too many elements"),
            Error::TooLarge => f.write_str("amf0: input too large"),
            Error::Unencodable => f.write_str("amf0: value cannot be encoded"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// 復号の上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// object・array の入れ子の深さ（最上位の値が 1）。
    pub max_depth: usize,
    /// 1 回の復号（[`Decoder`] 1 つ）で読む値の総数（入れ子の中も数える）。
    pub max_elements: usize,
    /// 入力の総バイト数。
    pub max_size: usize,
}

impl Default for Limits {
    /// 仕様 9.4 の初期値（入れ子 32、RTMP message 16 MiB）。要素数は 65536。
    fn default() -> Self {
        Limits {
            max_depth: 32,
            max_elements: 65_536,
            max_size: 16 * 1024 * 1024,
        }
    }
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// object または ECMA array の中身。
    pub fn as_properties(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(p) | Value::EcmaArray(p) => Some(p),
            _ => None,
        }
    }

    /// object または ECMA array から、キーが一致する最初の値を探す。
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_properties()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    pub fn is_null_or_undefined(&self) -> bool {
        matches!(self, Value::Null | Value::Undefined)
    }

    /// `(キー, 値)` の並びから object を作る。
    pub fn object<K: Into<String>>(props: impl IntoIterator<Item = (K, Value)>) -> Value {
        Value::Object(props.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn string(s: impl Into<String>) -> Value {
        Value::String(s.into())
    }

    /// 符号化したときのバイト数。
    pub fn encoded_len(&self) -> usize {
        fn props_len(p: &[(String, Value)]) -> usize {
            p.iter()
                .map(|(k, v)| 2 + k.len() + v.encoded_len())
                .sum::<usize>()
                + 3
        }
        match self {
            Value::Number(_) => 9,
            Value::Boolean(_) => 2,
            Value::String(s) if s.len() <= 0xFFFF => 3 + s.len(),
            Value::String(s) => 5 + s.len(),
            Value::Object(p) => 1 + props_len(p),
            Value::Null | Value::Undefined => 1,
            Value::EcmaArray(p) => 5 + props_len(p),
            Value::StrictArray(v) => 5 + v.iter().map(Value::encoded_len).sum::<usize>(),
            Value::Date { .. } => 11,
        }
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Number(v)
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Boolean(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::String(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::String(v)
    }
}

/// 連続した AMF0 値を先頭から順に読む。
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
    limits: Limits,
    elements: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(buf: &'a [u8]) -> Result<Self> {
        Self::with_limits(buf, Limits::default())
    }

    pub fn with_limits(buf: &'a [u8], limits: Limits) -> Result<Self> {
        if buf.len() > limits.max_size {
            return Err(Error::TooLarge);
        }
        Ok(Decoder {
            buf,
            pos: 0,
            limits,
            elements: 0,
        })
    }

    /// 読んだバイト数。
    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// 次の値。入力の終わりなら `None`。
    pub fn next_value(&mut self) -> Result<Option<Value>> {
        if self.is_empty() {
            return Ok(None);
        }
        self.read_value(1).map(Some)
    }

    /// 残りをすべて読む。
    pub fn read_all(&mut self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        while let Some(v) = self.next_value()? {
            out.push(v);
        }
        Ok(out)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(Error::Truncated)?;
        if end > self.buf.len() {
            return Err(Error::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn f64(&mut self) -> Result<f64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(f64::from_be_bytes(a))
    }

    fn str_of_len(&mut self, n: usize) -> Result<String> {
        let b = self.take(n)?;
        std::str::from_utf8(b)
            .map(str::to_owned)
            .map_err(|_| Error::InvalidUtf8)
    }

    fn count(&mut self) -> Result<()> {
        self.elements += 1;
        if self.elements > self.limits.max_elements {
            return Err(Error::TooManyElements);
        }
        Ok(())
    }

    fn read_value(&mut self, depth: usize) -> Result<Value> {
        if depth > self.limits.max_depth {
            return Err(Error::DepthExceeded);
        }
        self.count()?;
        let m = self.u8()?;
        match m {
            marker::NUMBER => Ok(Value::Number(self.f64()?)),
            marker::BOOLEAN => Ok(Value::Boolean(self.u8()? != 0)),
            marker::STRING => {
                let n = self.u16()? as usize;
                Ok(Value::String(self.str_of_len(n)?))
            }
            marker::LONG_STRING => {
                let n = self.u32()? as usize;
                Ok(Value::String(self.str_of_len(n)?))
            }
            marker::OBJECT => Ok(Value::Object(self.read_props(depth)?)),
            marker::NULL => Ok(Value::Null),
            marker::UNDEFINED => Ok(Value::Undefined),
            marker::ECMA_ARRAY => {
                // 宣言された個数は信用しない（実装によって 0 や不正確な値が入る）。
                let _declared = self.u32()?;
                Ok(Value::EcmaArray(self.read_props(depth)?))
            }
            marker::STRICT_ARRAY => {
                let n = self.u32()? as usize;
                // 各要素は最低 1 バイトなので、残りより多い個数はありえない。
                if n > self.buf.len() - self.pos {
                    return Err(Error::Truncated);
                }
                if n > self.limits.max_elements {
                    return Err(Error::TooManyElements);
                }
                let mut v = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    v.push(self.read_value(depth + 1)?);
                }
                Ok(Value::StrictArray(v))
            }
            marker::DATE => {
                let millis = self.f64()?;
                let timezone = self.u16()? as i16;
                Ok(Value::Date { millis, timezone })
            }
            marker::OBJECT_END => Err(Error::UnexpectedObjectEnd),
            other => Err(Error::UnsupportedMarker(other)),
        }
    }

    fn read_props(&mut self, depth: usize) -> Result<Vec<(String, Value)>> {
        let mut props = Vec::new();
        loop {
            let n = self.u16()? as usize;
            if n == 0 {
                // 空キーの直後が object end なら終わり。空キーに値が続くのは許さない。
                if self.u8()? == marker::OBJECT_END {
                    return Ok(props);
                }
                return Err(Error::UnexpectedObjectEnd);
            }
            let key = self.str_of_len(n)?;
            let v = self.read_value(depth + 1)?;
            props.push((key, v));
        }
    }
}

/// `buf` の値をすべて既定の上限で復号する。
pub fn decode_all(buf: &[u8]) -> Result<Vec<Value>> {
    Decoder::new(buf)?.read_all()
}

/// 値を 1 つ符号化して `out` に追記する。
pub fn encode(v: &Value, out: &mut Vec<u8>) -> Result<()> {
    encode_depth(v, out, 1)
}

/// 値を順に符号化する。
pub fn encode_all<'a>(values: impl IntoIterator<Item = &'a Value>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for v in values {
        encode(v, &mut out)?;
    }
    Ok(out)
}

fn encode_depth(v: &Value, out: &mut Vec<u8>, depth: usize) -> Result<()> {
    if depth > Limits::default().max_depth {
        return Err(Error::DepthExceeded);
    }
    match v {
        Value::Number(n) => {
            out.push(marker::NUMBER);
            out.extend_from_slice(&n.to_be_bytes());
        }
        Value::Boolean(b) => {
            out.push(marker::BOOLEAN);
            out.push(*b as u8);
        }
        Value::String(s) => {
            if s.len() <= 0xFFFF {
                out.push(marker::STRING);
                out.extend_from_slice(&(s.len() as u16).to_be_bytes());
            } else {
                let n = u32::try_from(s.len()).map_err(|_| Error::Unencodable)?;
                out.push(marker::LONG_STRING);
                out.extend_from_slice(&n.to_be_bytes());
            }
            out.extend_from_slice(s.as_bytes());
        }
        Value::Object(p) => {
            out.push(marker::OBJECT);
            encode_props(p, out, depth)?;
        }
        Value::Null => out.push(marker::NULL),
        Value::Undefined => out.push(marker::UNDEFINED),
        Value::EcmaArray(p) => {
            out.push(marker::ECMA_ARRAY);
            let n = u32::try_from(p.len()).map_err(|_| Error::Unencodable)?;
            out.extend_from_slice(&n.to_be_bytes());
            encode_props(p, out, depth)?;
        }
        Value::StrictArray(items) => {
            out.push(marker::STRICT_ARRAY);
            let n = u32::try_from(items.len()).map_err(|_| Error::Unencodable)?;
            out.extend_from_slice(&n.to_be_bytes());
            for it in items {
                encode_depth(it, out, depth + 1)?;
            }
        }
        Value::Date { millis, timezone } => {
            out.push(marker::DATE);
            out.extend_from_slice(&millis.to_be_bytes());
            out.extend_from_slice(&timezone.to_be_bytes());
        }
    }
    Ok(())
}

fn encode_props(p: &[(String, Value)], out: &mut Vec<u8>, depth: usize) -> Result<()> {
    for (k, v) in p {
        // 空キーは object end と区別できないので書けない。
        if k.is_empty() || k.len() > 0xFFFF {
            return Err(Error::Unencodable);
        }
        out.extend_from_slice(&(k.len() as u16).to_be_bytes());
        out.extend_from_slice(k.as_bytes());
        encode_depth(v, out, depth + 1)?;
    }
    out.extend_from_slice(&[0, 0, marker::OBJECT_END]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: Value) {
        let mut b = Vec::new();
        encode(&v, &mut b).unwrap();
        assert_eq!(b.len(), v.encoded_len());
        let d = decode_all(&b).unwrap();
        assert_eq!(d, vec![v]);
    }

    #[test]
    fn roundtrips() {
        roundtrip(Value::Number(1.5));
        roundtrip(Value::Boolean(true));
        roundtrip(Value::string("hello"));
        roundtrip(Value::string("あ".repeat(30000))); // 90000 バイト → long string
        roundtrip(Value::Null);
        roundtrip(Value::Undefined);
        roundtrip(Value::Date {
            millis: 1.7e12,
            timezone: 0,
        });
        roundtrip(Value::object([
            ("app", Value::string("live")),
            ("n", Value::Number(3.0)),
            ("inner", Value::object([("x", Value::Null)])),
        ]));
        roundtrip(Value::EcmaArray(vec![(
            "duration".into(),
            Value::Number(0.0),
        )]));
        roundtrip(Value::StrictArray(vec![
            Value::Number(1.0),
            Value::string("a"),
        ]));
    }

    #[test]
    fn decodes_connect_command() {
        // "connect", 1.0, {app: "live"}
        let mut b = Vec::new();
        for v in [
            Value::string("connect"),
            Value::Number(1.0),
            Value::object([("app", Value::string("live"))]),
        ] {
            encode(&v, &mut b).unwrap();
        }
        let vals = decode_all(&b).unwrap();
        assert_eq!(vals[0].as_str(), Some("connect"));
        assert_eq!(vals[1].as_f64(), Some(1.0));
        assert_eq!(vals[2].get_str("app"), Some("live"));
    }

    #[test]
    fn ecma_array_count_is_not_trusted() {
        // 宣言 0xFFFFFFFF 個だが実際は 1 個。
        let mut b = vec![marker::ECMA_ARRAY, 0xFF, 0xFF, 0xFF, 0xFF];
        b.extend_from_slice(&[0, 1, b'a', marker::NULL, 0, 0, marker::OBJECT_END]);
        let v = decode_all(&b).unwrap();
        assert_eq!(v[0].get("a"), Some(&Value::Null));
    }

    #[test]
    fn rejects_unsupported() {
        for m in [
            marker::REFERENCE,
            marker::UNSUPPORTED,
            marker::AVMPLUS_OBJECT,
            marker::TYPED_OBJECT,
            marker::MOVIECLIP,
            marker::RECORDSET,
            marker::XML_DOCUMENT,
            0x12,
            0xFF,
        ] {
            assert_eq!(decode_all(&[m, 0, 0]), Err(Error::UnsupportedMarker(m)));
        }
        assert_eq!(
            decode_all(&[marker::OBJECT_END]),
            Err(Error::UnexpectedObjectEnd)
        );
    }

    #[test]
    fn rejects_deep_nesting() {
        let mut b = Vec::new();
        for _ in 0..40 {
            b.push(marker::STRICT_ARRAY);
            b.extend_from_slice(&1u32.to_be_bytes());
        }
        b.push(marker::NULL);
        assert_eq!(decode_all(&b), Err(Error::DepthExceeded));

        let mut b = Vec::new();
        for _ in 0..40 {
            b.extend_from_slice(&[marker::OBJECT, 0, 1, b'k']);
        }
        assert_eq!(decode_all(&b), Err(Error::DepthExceeded));
    }

    #[test]
    fn rejects_too_many_elements() {
        let limits = Limits {
            max_elements: 10,
            ..Limits::default()
        };
        let b = vec![marker::NULL; 11];
        let mut d = Decoder::with_limits(&b, limits).unwrap();
        assert_eq!(d.read_all(), Err(Error::TooManyElements));
        // strict array の宣言数が大きすぎる
        let mut b = vec![marker::STRICT_ARRAY, 0, 0, 0, 20];
        b.extend(std::iter::repeat_n(marker::NULL, 20));
        let mut d = Decoder::with_limits(&b, limits).unwrap();
        assert_eq!(d.read_all(), Err(Error::TooManyElements));
    }

    #[test]
    fn rejects_too_large() {
        let limits = Limits {
            max_size: 4,
            ..Limits::default()
        };
        assert!(matches!(
            Decoder::with_limits(&[0; 5], limits),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn truncated_and_garbage_never_panic() {
        let mut b = Vec::new();
        encode(
            &Value::object([
                ("a", Value::StrictArray(vec![Value::Number(1.0); 3])),
                ("b", Value::string("xyz")),
                (
                    "c",
                    Value::EcmaArray(vec![("d".into(), Value::Boolean(false))]),
                ),
            ]),
            &mut b,
        )
        .unwrap();
        for i in 1..b.len() {
            assert!(decode_all(&b[..i]).is_err());
        }
        // 疑似乱数の入力
        let mut x: u32 = 0x1234_5678;
        for _ in 0..2000 {
            let mut buf = Vec::new();
            for _ in 0..64 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                buf.push((x % 20) as u8);
            }
            let _ = decode_all(&buf);
        }
        // 巨大な strict array 宣言
        assert_eq!(
            decode_all(&[marker::STRICT_ARRAY, 0xFF, 0xFF, 0xFF, 0xFF]),
            Err(Error::Truncated)
        );
        assert_eq!(
            decode_all(&[marker::LONG_STRING, 0xFF, 0xFF, 0xFF, 0xFF]),
            Err(Error::Truncated)
        );
        assert_eq!(
            decode_all(&[marker::STRING, 0, 2, 0xFF, 0xFE]),
            Err(Error::InvalidUtf8)
        );
    }

    #[test]
    fn encode_rejects_empty_key() {
        let mut b = Vec::new();
        assert_eq!(
            encode(&Value::object([("", Value::Null)]), &mut b),
            Err(Error::Unencodable)
        );
    }
}
