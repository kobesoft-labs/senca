//! codec worker との窓口（ADR-0004、M4）。外部コーデックはノードに読み込まず、worker プロセスで動かす。

use serde_json::{Value, json};

pub struct CodecService {}

impl CodecService {
    pub fn capabilities(&self) -> Value {
        json!([])
    }
    pub fn devices(&self) -> Value {
        json!([])
    }
    pub fn can_transcode_hw(&self) -> bool {
        false
    }
}
