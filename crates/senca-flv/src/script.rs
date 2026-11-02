//! script data（FLV の script タグ・RTMP の AMF0 data message）。
//!
//! RTMP の送り手は `@setDataFrame`, `onMetaData`, {…} を送る。FLV ファイルに書くときは
//! 先頭の `@setDataFrame` を外して `onMetaData`, {…} にする（[`ScriptData::strip_set_data_frame`]）。

use crate::Error;
use senca_amf0::{Decoder, Value};

/// script data の中身（名前と引数）。
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptData {
    pub name: String,
    pub values: Vec<Value>,
}

impl ScriptData {
    /// AMF0 の列を読む。先頭は文字列でなければならない。
    pub fn parse(body: &[u8]) -> Result<ScriptData, Error> {
        let mut d = Decoder::new(body)?;
        let name = match d.next_value()? {
            Some(Value::String(s)) => s,
            _ => return Err(Error::Malformed("script data must start with a string")),
        };
        Ok(ScriptData {
            name,
            values: d.read_all()?,
        })
    }

    /// `@setDataFrame` で始まっていれば、それを外して次の名前を使う。
    pub fn strip_set_data_frame(self) -> Result<ScriptData, Error> {
        if self.name != "@setDataFrame" {
            return Ok(self);
        }
        let mut it = self.values.into_iter();
        match it.next() {
            Some(Value::String(name)) => Ok(ScriptData {
                name,
                values: it.collect(),
            }),
            _ => Err(Error::Malformed("@setDataFrame without a name")),
        }
    }

    /// onMetaData の object（または ECMA array）。
    pub fn metadata(&self) -> Option<&Value> {
        (self.name == "onMetaData")
            .then(|| self.values.first())
            .flatten()
            .filter(|v| v.as_properties().is_some())
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        senca_amf0::encode(&Value::String(self.name.clone()), &mut out)?;
        for v in &self.values {
            senca_amf0::encode(v, &mut out)?;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_data_frame() {
        let s = ScriptData {
            name: "@setDataFrame".into(),
            values: vec![
                Value::string("onMetaData"),
                Value::EcmaArray(vec![("width".into(), Value::Number(320.0))]),
            ],
        };
        let b = s.encode().unwrap();
        let p = ScriptData::parse(&b)
            .unwrap()
            .strip_set_data_frame()
            .unwrap();
        assert_eq!(p.name, "onMetaData");
        assert_eq!(p.metadata().unwrap().get_f64("width"), Some(320.0));
        assert!(ScriptData::parse(&[0x05]).is_err());
    }
}
