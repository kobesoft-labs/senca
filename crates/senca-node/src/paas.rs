//! クラウドのメディア PaaS との連携（REQ-001、M13）。外部の資源の作成・削除を SENCA の出力と結び付ける。

use crate::app::App;
use senca_control::model::Output;

pub const PROVIDERS: &[&str] = &["cloudflare_stream", "aws_ivs", "aws_medialive"];

/// SENCA が外部に作った資源を片付ける（M13 で実装）。
pub fn cleanup(app: &App, o: &Output) {
    if o.provider_resource.is_some() {
        app.log(&format!("paas: output {} の外部資源の後片付けはまだ実装されていません", o.id));
    }
}
