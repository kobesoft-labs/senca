//! 変換計画（仕様 7 章）。実行前に passthrough / remux / transcode / unsupported と理由を返す。副作用なし。

use crate::app::App;
use senca_control::model::{CodecProfile, Stream};
use senca_core::{Codec, MediaKind};
use serde_json::{Value, json};

pub fn plan(app: &App, s: &Stream) -> Value {
    let configs: Vec<senca_core::CodecConfig> = app
        .hubs
        .lock()
        .unwrap()
        .get(&s.id)
        .map(|h| h.shared.configs.lock().unwrap().values().cloned().collect())
        .unwrap_or_default();
    if configs.is_empty() {
        return json!({ "state": "provisional", "reason": "入力のコーデックがまだ分かりません（publish 前）", "outputs": [] });
    }
    let hw = app.codec.as_ref().map(|c| c.can_transcode_hw()).unwrap_or(false);
    let mut outputs = Vec::new();
    for c in &configs {
        let track = match c.codec.kind() {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
            _ => "data",
        };
        // HLS
        let hls = if s.codec_profile == CodecProfile::RoyaltyFree && c.codec.is_patent_pooled() {
            if c.codec.kind() == MediaKind::Video && hw {
                json!({ "output": "hls", "track": track, "mode": "transcode", "to": "av1", "reason": "royalty_free: H.264/H.265 をハードウェアで復号し AV1 へ", "added_latency_ms": 200 })
            } else {
                json!({ "output": "hls", "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC",
                        "reason": format!("royalty_free プロファイルでは {} を扱えません（変換に必要な装置がありません）", c.codec) })
            }
        } else {
            let container_ok = matches!(
                (s.hls.container.as_str(), c.codec),
                ("ts", Codec::H264 | Codec::H265 | Codec::Aac | Codec::Opus)
                    | ("fmp4", Codec::H264 | Codec::H265 | Codec::Av1 | Codec::Vp9 | Codec::Aac | Codec::Opus)
            );
            if container_ok {
                json!({ "output": "hls", "track": track, "mode": "remux", "container": s.hls.container, "added_latency_ms": s.hls.segment_ms })
            } else {
                json!({ "output": "hls", "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC",
                        "reason": format!("{} は HLS の {} に入れられません", c.codec, s.hls.container) })
            }
        };
        outputs.push(hls);
        // WebRTC
        let rtc = match c.codec {
            Codec::H264 | Codec::Vp8 | Codec::Vp9 | Codec::Av1 | Codec::Opus | Codec::Pcma | Codec::Pcmu => {
                json!({ "output": "webrtc", "track": track, "mode": "repacketize" })
            }
            Codec::Aac => {
                json!({ "output": "webrtc", "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC", "reason": "AAC は WebRTC で運べません。AAC の復号は特許の方針で行いません（送り手で Opus にしてください）" })
            }
            Codec::H265 if hw => json!({ "output": "webrtc", "track": track, "mode": "transcode", "to": "h264", "reason": "ハードウェアで H.265 → H.264" }),
            _ => json!({ "output": "webrtc", "track": track, "mode": "unsupported", "code": "UNSUPPORTED_CODEC" }),
        };
        outputs.push(rtc);
        outputs.push(json!({ "output": "recording", "track": track, "mode": "passthrough" }));
        outputs.push(json!({ "output": "rtmp", "track": track, "mode": "remux" }));
    }
    json!({ "state": "ready", "outputs": outputs })
}
