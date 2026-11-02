//! Stream ID の解釈（SRT Access Control の `#!::key=value,...` 形式と平文）。

/// 解釈した Stream ID。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct StreamId {
    /// 受け取った文字列そのまま。
    pub raw: String,
    /// `r`（資源名）。平文の Stream ID はそのまま資源名とみなす。
    pub resource: Option<String>,
    /// `m`（request / publish / bidirectional）。
    pub mode: Option<String>,
    /// `u`（利用者名）。
    pub user: Option<String>,
    /// `h`（ホスト名）。
    pub host: Option<String>,
    /// `s`（セッション ID）。
    pub session: Option<String>,
    /// `t`（種別）。
    pub kind: Option<String>,
    /// その他のキー。
    pub other: Vec<(String, String)>,
}

/// 解釈の失敗。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamIdError {
    /// `#!::` 形式なのに `key=value` になっていない、または同じキーが重なる。
    Syntax,
}

impl StreamId {
    /// 解釈する。`#!::` で始まらなければ平文として `resource` に入れる。
    pub fn parse(s: &str) -> Result<Self, StreamIdError> {
        let mut out = StreamId { raw: s.to_string(), ..Default::default() };
        let Some(rest) = s.strip_prefix("#!::") else {
            if !s.is_empty() {
                out.resource = Some(s.to_string());
            }
            return Ok(out);
        };
        for item in rest.split(',') {
            if item.is_empty() {
                continue;
            }
            let (k, v) = item.split_once('=').ok_or(StreamIdError::Syntax)?;
            let slot = match k {
                "r" => &mut out.resource,
                "m" => &mut out.mode,
                "u" => &mut out.user,
                "h" => &mut out.host,
                "s" => &mut out.session,
                "t" => &mut out.kind,
                _ => {
                    if out.other.len() >= 32 || out.other.iter().any(|(ok, _)| ok == k) {
                        return Err(StreamIdError::Syntax);
                    }
                    out.other.push((k.to_string(), v.to_string()));
                    continue;
                }
            };
            if slot.is_some() {
                return Err(StreamIdError::Syntax);
            }
            *slot = Some(v.to_string());
        }
        Ok(out)
    }

    /// `m=publish` か。
    pub fn is_publish(&self) -> bool {
        self.mode.as_deref() == Some("publish")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forms() {
        let s = StreamId::parse("#!::r=live/cam1,m=publish,u=alice,x=1").unwrap();
        assert_eq!(s.resource.as_deref(), Some("live/cam1"));
        assert!(s.is_publish());
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert_eq!(s.other, vec![("x".into(), "1".into())]);
        let p = StreamId::parse("live/cam2").unwrap();
        assert_eq!(p.resource.as_deref(), Some("live/cam2"));
        assert_eq!(p.mode, None);
        assert!(StreamId::parse("#!::r=a,r=b").is_err());
        assert!(StreamId::parse("#!::rabc").is_err());
        assert_eq!(StreamId::parse("").unwrap().resource, None);
    }
}
