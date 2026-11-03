//! 変換計画（仕様 7 章）。実行前に passthrough / remux / repacketize / unsupported と理由を返す。副作用なし。
//!
//! 計画は、ノードが実際に行うことだけを示す。stream の hub は再エンコードしない（コーデックの変換はパイプライン
//! `POST /pipelines` の `decode`・`encode` で行う）。そのため hub の出力に `transcode` は出ない。
//! - `royalty_free` の stream で届いた H.264・H.265・AAC のトラックは hub が捨てる（すべての出力で `mode: "unsupported"`、理由付き）。
//!   AV1 などへの変換が要るなら、パイプラインで変換した結果を別の stream に出す（ハードウェアの復号が要る）。
//! - WebRTC（WHEP）は、そのまま RTP にできるコーデックだけを送る（H.265・AAC は送らない）。

use crate::app::App;
use senca_control::model::Stream;
use senca_core::{Codec, MediaKind};
use serde_json::{Value, json};

fn track_kind(c: Codec) -> &'static str {
    match c.kind() {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
        _ => "data",
    }
}

pub fn plan(app: &App, s: &Stream) -> Value {
    let (configs, rejected): (Vec<senca_core::CodecConfig>, Vec<Codec>) = app
        .hubs
        .lock()
        .unwrap()
        .get(&s.id)
        .map(|h| {
            (h.shared.configs.lock().unwrap().values().cloned().collect(), h.shared.rejected.lock().unwrap().values().copied().collect())
        })
        .unwrap_or_default();
    if configs.is_empty() && rejected.is_empty() {
        return json!({ "state": "provisional", "reason": "入力のコーデックがまだ分かりません（publish 前）", "outputs": [] });
    }
    // パイプラインで変換できるか（参考。hub の計画には入れない）
    let hw_decode = |c: Codec| app.codec.as_ref().is_some_and(|x| x.can_decode(c));
    let mut outputs = Vec::new();
    // royalty_free で捨てたトラック: どの出力にも出ない
    for c in &rejected {
        let track = track_kind(*c);
        let hint = if c.kind() == MediaKind::Video {
            if hw_decode(*c) {
                "AV1 などへ変換するには、パイプライン（decode → encode）で別の stream に出してください".to_string()
            } else {
                format!("このノードには {c} を復号する装置（GPU）が無いので、パイプラインでも変換できません。送り手で AV1・VP9 にしてください")
            }
        } else {
            "AAC は復号しません（特許の方針）。送り手で Opus にしてください".to_string()
        };
        for out in ["hls", "webrtc", "recording", "rtmp"] {
            outputs.push(json!({
                "output": out, "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC",
                "reason": format!("royalty_free プロファイルでは {c} のトラックを破棄します（変換はしません）。{hint}"),
            }));
        }
    }
    for c in &configs {
        let track = track_kind(c.codec);
        // HLS
        let container_ok = matches!(
            (s.hls.container.as_str(), c.codec),
            ("ts", Codec::H264 | Codec::H265 | Codec::Aac | Codec::Opus)
                | ("fmp4", Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Vp9 | Codec::Aac | Codec::Opus)
        );
        let hls = if container_ok {
            json!({ "output": "hls", "track": track, "mode": "remux", "container": s.hls.container, "added_latency_ms": s.hls.segment_ms })
        } else {
            json!({ "output": "hls", "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC",
                    "reason": format!("{} は HLS の {} に入れられません", c.codec, s.hls.container) })
        };
        outputs.push(hls);
        // WebRTC（WHEP）
        let rtc = match c.codec {
            Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::Opus | Codec::Pcma | Codec::Pcmu => {
                json!({ "output": "webrtc", "track": track, "mode": "repacketize" })
            }
            Codec::Aac => {
                json!({ "output": "webrtc", "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC", "reason": "AAC は WebRTC で運べません。AAC の復号は特許の方針で行いません（送り手で Opus にしてください）" })
            }
            Codec::H265 => {
                json!({ "output": "webrtc", "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC", "reason": "H.265 は WebRTC（WHEP）では送りません（変換はしません）。H.264・VP8・VP9・AV1 で送ってください" })
            }
            _ => json!({ "output": "webrtc", "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC" }),
        };
        outputs.push(rtc);
        outputs.push(json!({ "output": "recording", "track": track, "mode": "passthrough" }));
        outputs.push(json!({ "output": "rtmp", "track": track, "mode": "remux" }));
    }
    json!({ "state": "ready", "outputs": outputs })
}
