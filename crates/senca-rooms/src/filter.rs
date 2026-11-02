//! 転送フィルター（forwarding filter）。
//!
//! `rules` は「AND の組」の OR。どれかの組の規則がすべて成り立てば一致。`action` が `block` なら一致したものを
//! 配らず、`allow` なら一致したものだけ配る。規則は**送り手**（トラックの持ち主）の属性を見る。
//!
//! ```json
//! {"action":"block","rules":[[{"field":"kind","operator":"is_in","values":["video"]},
//!                             {"field":"client_id","operator":"is_not_in","values":["teacher"]}]]}
//! ```
//! （先生以外の映像を受けない）

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{FieldError, Kind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterAction {
    Block,
    Allow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    IsIn,
    IsNotIn,
}

/// 規則 1 つ。`field` は `connection_id`・`client_id`・`bundle_id`・`kind`・`metadata.<key>`（文字列・数・真偽の値）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub field: String,
    pub operator: Operator,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardingFilter {
    pub action: FilterAction,
    pub rules: Vec<Vec<Rule>>,
}

/// 規則が見る送り手の属性。
#[derive(Debug, Clone, Copy)]
pub struct Subject<'a> {
    pub connection_id: &'a str,
    pub client_id: &'a str,
    pub bundle_id: &'a str,
    pub kind: Kind,
    pub metadata: &'a Value,
}

impl Rule {
    fn field_value(&self, s: &Subject<'_>) -> Option<String> {
        match self.field.as_str() {
            "connection_id" => Some(s.connection_id.to_string()),
            "client_id" => Some(s.client_id.to_string()),
            "bundle_id" => Some(s.bundle_id.to_string()),
            "kind" => Some(s.kind.as_str().to_string()),
            f => {
                let key = f.strip_prefix("metadata.")?;
                match s.metadata.get(key)? {
                    Value::String(v) => Some(v.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                }
            }
        }
    }

    fn matches(&self, s: &Subject<'_>) -> bool {
        let v = self.field_value(s);
        let inside = v.as_ref().is_some_and(|v| self.values.iter().any(|x| x == v));
        match self.operator {
            Operator::IsIn => inside,
            Operator::IsNotIn => !inside,
        }
    }
}

impl ForwardingFilter {
    /// この送り手のトラックを配ってよいか。
    pub fn allows(&self, s: &Subject<'_>) -> bool {
        let hit = self.rules.iter().any(|g| !g.is_empty() && g.iter().all(|r| r.matches(s)));
        match self.action {
            FilterAction::Block => !hit,
            FilterAction::Allow => hit,
        }
    }

    pub fn validate(&self) -> Result<(), Vec<FieldError>> {
        let mut e = Vec::new();
        if self.rules.is_empty() || self.rules.len() > 10 {
            e.push(FieldError::new("rules", "1〜10 組"));
        }
        for (i, g) in self.rules.iter().enumerate() {
            if g.is_empty() || g.len() > 10 {
                e.push(FieldError::new(&format!("rules[{i}]"), "1〜10 個の規則"));
            }
            for (j, r) in g.iter().enumerate() {
                let f = r.field.as_str();
                let ok = matches!(f, "connection_id" | "client_id" | "bundle_id" | "kind")
                    || f.strip_prefix("metadata.").is_some_and(|k| !k.is_empty() && k.len() <= 64);
                if !ok {
                    e.push(FieldError::new(&format!("rules[{i}][{j}].field"), "未知のフィールド"));
                }
                if r.values.is_empty() || r.values.len() > 100 || r.values.iter().any(|v| v.len() > 255) {
                    e.push(FieldError::new(&format!("rules[{i}][{j}].values"), "1〜100 個、各 255 byte まで"));
                }
                if f == "kind" && r.values.iter().any(|v| v != "audio" && v != "video") {
                    e.push(FieldError::new(&format!("rules[{i}][{j}].values"), "audio か video"));
                }
            }
        }
        if e.is_empty() { Ok(()) } else { Err(e) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn subj<'a>(client: &'a str, kind: Kind, md: &'a Value) -> Subject<'a> {
        Subject {
            connection_id: "c1",
            client_id: client,
            bundle_id: "b1",
            kind,
            metadata: md,
        }
    }

    #[test]
    fn block_video_except_teacher() {
        let f: ForwardingFilter = serde_json::from_value(json!({
            "action": "block",
            "rules": [[
                {"field": "kind", "operator": "is_in", "values": ["video"]},
                {"field": "client_id", "operator": "is_not_in", "values": ["teacher"]}
            ]]
        }))
        .unwrap();
        f.validate().unwrap();
        let md = json!({});
        assert!(f.allows(&subj("teacher", Kind::Video, &md)));
        assert!(!f.allows(&subj("student", Kind::Video, &md)));
        assert!(f.allows(&subj("student", Kind::Audio, &md)));
    }

    #[test]
    fn allow_by_metadata_or_group() {
        let f: ForwardingFilter = serde_json::from_value(json!({
            "action": "allow",
            "rules": [
                [{"field": "metadata.team", "operator": "is_in", "values": ["red", "1"]}],
                [{"field": "connection_id", "operator": "is_in", "values": ["c1"]}, {"field": "kind", "operator": "is_in", "values": ["audio"]}]
            ]
        }))
        .unwrap();
        let red = json!({"team": "red"});
        let one = json!({"team": 1});
        let blue = json!({"team": "blue"});
        assert!(f.allows(&subj("x", Kind::Video, &red)));
        assert!(f.allows(&subj("x", Kind::Video, &one)));
        assert!(!f.allows(&subj("x", Kind::Video, &blue)));
        assert!(f.allows(&subj("x", Kind::Audio, &blue)));
        // 無い metadata は is_in に一致しない、is_not_in に一致する
        let none = json!(null);
        assert!(!f.allows(&subj("x", Kind::Video, &none)));
    }

    #[test]
    fn validation() {
        assert!(serde_json::from_value::<ForwardingFilter>(json!({"action":"block","rules":[],"x":1})).is_err());
        let bad: ForwardingFilter = serde_json::from_value(json!({
            "action": "block",
            "rules": [[{"field": "ip", "operator": "is_in", "values": []}, {"field": "kind", "operator": "is_in", "values": ["data"]}]]
        }))
        .unwrap();
        let errs = bad.validate().unwrap_err();
        assert_eq!(errs.len(), 3);
        let empty = ForwardingFilter {
            action: FilterAction::Block,
            rules: vec![],
        };
        assert!(empty.validate().is_err());
    }
}
