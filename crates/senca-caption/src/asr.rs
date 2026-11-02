//! 音声認識（ASR）の差し替え口。要求を作り、応答を読むだけ（sans-io）。
//!
//! - [`OpenAiTranscription`] — OpenAI 互換の `POST {base}/audio/transcriptions`（multipart/form-data、
//!   `response_format=verbose_json`、`timestamp_granularities[]=segment`）。OpenAI のほか、同じ API を出す
//!   whisper.cpp の server（`--inference-path` を合わせたもの）・faster-whisper の server（speaches 等）でも使える。
//! - [`WhisperCppServer`] — whisper.cpp の server の `POST {base}/inference`。
//!   **注意**: whisper.cpp の server は既定では WAV しか受けない。こちらは復号しない（ADR-0004）ので、
//!   server を `--convert`（server 側の ffmpeg で変換）付きで起動すること。
//!
//! 応答の時刻はチャンクの中の相対時刻（µs）。チャンクの長さに切り詰める。

use crate::chunk::{AudioChunk, ChunkInfo};
use crate::error::{Error, Result, clip_message};
use crate::http::{Endpoint, HttpCall, HttpReply, Multipart};
use crate::secret::Secret;
use serde_json::Value;

/// 認識結果の 1 区切り（チャンクの先頭からの相対時刻）。
#[derive(Debug, Clone, PartialEq)]
pub struct AsrSegment {
    pub start_us: i64,
    pub end_us: i64,
    pub text: String,
    /// Whisper の no_speech_prob（あれば）。
    pub no_speech_prob: Option<f32>,
    /// Whisper の avg_logprob（あれば）。
    pub avg_logprob: Option<f32>,
}

/// 1 チャンクの認識結果。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Transcript {
    /// 相手が判定した言語（あれば）。
    pub language: Option<String>,
    pub segments: Vec<AsrSegment>,
}

impl Transcript {
    /// 全体の文字列（区切りを空白なしでつなぐ。次の要求の `prompt` に使う）。
    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join("")
    }
}

/// 音声認識の差し替え口。
pub trait AsrAdapter {
    /// 表示用の名前（ログ・指標）。
    fn name(&self) -> &'static str;
    /// 送り先（送る前に表示・確認するため）。
    fn endpoint(&self) -> &Endpoint;
    /// チャンクを送る要求。`prompt` は直前の字幕（認識の続きの手がかり。送らないなら `None`）。
    fn build_request(&self, chunk: &AudioChunk, prompt: Option<&str>) -> Result<HttpCall>;
    /// 応答を読む。
    fn parse_response(&self, chunk: &ChunkInfo, reply: &HttpReply) -> Result<Transcript>;
}

/// 応答の読み方の上限と、無音の幻聴を捨てるしきい値。
#[derive(Debug, Clone, PartialEq)]
pub struct AsrLimits {
    /// 応答の body の上限（バイト）。
    pub max_response_bytes: usize,
    /// 区切りの数の上限。
    pub max_segments: usize,
    /// 1 区切りの文字数の上限（超えた分は切る）。
    pub max_segment_chars: usize,
    /// no_speech_prob がこれより大きく、avg_logprob が −1 より小さい区切りは捨てる（Whisper の判定と同じ考え方）。
    pub no_speech_threshold: Option<f32>,
    /// `prompt` に入れる文字数の上限（後ろから）。
    pub max_prompt_chars: usize,
    /// ノードが使う時間切れ（ms）。
    pub timeout_ms: u64,
}

impl Default for AsrLimits {
    fn default() -> Self {
        AsrLimits {
            max_response_bytes: 1 << 20,
            max_segments: 2000,
            max_segment_chars: 2000,
            no_speech_threshold: Some(0.6),
            max_prompt_chars: 200,
            timeout_ms: 30_000,
        }
    }
}

/// OpenAI 互換の `/audio/transcriptions`。
#[derive(Debug, Clone)]
pub struct OpenAiTranscription {
    /// 例: `https://api.openai.com/v1`（[`OPENAI_API_URL`]）、`http://gpu-host:8000/v1`。
    pub endpoint: Endpoint,
    /// `Authorization: Bearer …`（社内の server なら `None`）。
    pub api_key: Option<Secret>,
    /// 例: `whisper-1`、`large-v3`。
    pub model: String,
    /// ISO 639-1 の言語の指定（例: `ja`）。`None` なら相手が判定する。
    pub language: Option<String>,
    pub temperature: Option<f32>,
    pub limits: AsrLimits,
}

/// OpenAI の API の URL（使うかどうかは設定する人が決める。既定では使わない）。
pub const OPENAI_API_URL: &str = "https://api.openai.com/v1";

impl OpenAiTranscription {
    pub fn new(endpoint: Endpoint, model: impl Into<String>) -> Self {
        OpenAiTranscription {
            endpoint,
            api_key: None,
            model: model.into(),
            language: Some("ja".into()),
            temperature: Some(0.0),
            limits: AsrLimits::default(),
        }
    }
}

fn check_language(l: &str) -> Result<()> {
    if l.is_empty() || l.len() > 16 || !l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
        return Err(Error::InvalidConfig("language"));
    }
    Ok(())
}

fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

impl AsrAdapter for OpenAiTranscription {
    fn name(&self) -> &'static str {
        "openai-transcriptions"
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn build_request(&self, chunk: &AudioChunk, prompt: Option<&str>) -> Result<HttpCall> {
        if self.model.is_empty() || self.model.len() > 128 {
            return Err(Error::InvalidConfig("model"));
        }
        let mut m = Multipart::new(chunk.info.seq);
        m.file("file", &chunk.file_name(), chunk.mime(), chunk.data.clone())?;
        m.text("model", &self.model)?;
        m.text("response_format", "verbose_json")?;
        m.text("timestamp_granularities[]", "segment")?;
        if let Some(l) = &self.language {
            check_language(l)?;
            m.text("language", l)?;
        }
        if let Some(t) = self.temperature {
            m.text("temperature", &format!("{t}"))?;
        }
        if let Some(p) = prompt.map(|p| tail_chars(p, self.limits.max_prompt_chars)).filter(|p| !p.is_empty()) {
            m.text("prompt", &p)?;
        }
        let (ct, body) = m.finish()?;
        let mut call = HttpCall::post(&self.endpoint, "/audio/transcriptions", &ct, body).header("Accept", "application/json");
        call.max_response_bytes = self.limits.max_response_bytes;
        call.timeout_ms = self.limits.timeout_ms;
        if let Some(k) = &self.api_key {
            call = call.secret_header("Authorization", Secret::new(format!("Bearer {}", k.expose())));
        }
        Ok(call)
    }

    fn parse_response(&self, chunk: &ChunkInfo, reply: &HttpReply) -> Result<Transcript> {
        parse_transcript(chunk, reply, &self.limits, self.api_key.as_ref())
    }
}

/// whisper.cpp の server の `/inference`。
#[derive(Debug, Clone)]
pub struct WhisperCppServer {
    /// 例: `http://gpu-host:8080`。
    pub endpoint: Endpoint,
    pub language: Option<String>,
    pub temperature: Option<f32>,
    pub limits: AsrLimits,
}

impl WhisperCppServer {
    pub fn new(endpoint: Endpoint) -> Self {
        WhisperCppServer { endpoint, language: Some("ja".into()), temperature: Some(0.0), limits: AsrLimits::default() }
    }
}

impl AsrAdapter for WhisperCppServer {
    fn name(&self) -> &'static str {
        "whisper.cpp"
    }

    fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    fn build_request(&self, chunk: &AudioChunk, prompt: Option<&str>) -> Result<HttpCall> {
        let mut m = Multipart::new(chunk.info.seq);
        m.file("file", &chunk.file_name(), chunk.mime(), chunk.data.clone())?;
        m.text("response_format", "verbose_json")?;
        if let Some(l) = &self.language {
            check_language(l)?;
            m.text("language", l)?;
        }
        if let Some(t) = self.temperature {
            m.text("temperature", &format!("{t}"))?;
        }
        if let Some(p) = prompt.map(|p| tail_chars(p, self.limits.max_prompt_chars)).filter(|p| !p.is_empty()) {
            m.text("prompt", &p)?;
        }
        let (ct, body) = m.finish()?;
        let mut call = HttpCall::post(&self.endpoint, "/inference", &ct, body).header("Accept", "application/json");
        call.max_response_bytes = self.limits.max_response_bytes;
        call.timeout_ms = self.limits.timeout_ms;
        Ok(call)
    }

    fn parse_response(&self, chunk: &ChunkInfo, reply: &HttpReply) -> Result<Transcript> {
        parse_transcript(chunk, reply, &self.limits, None)
    }
}

/// 相手の誤りの文を取り出す（OpenAI: `{"error":{"message":…}}`、whisper.cpp: `{"error":"…"}`）。
pub(crate) fn error_message(body: &[u8], secret: Option<&Secret>) -> String {
    let msg = match serde_json::from_slice::<Value>(body) {
        Ok(v) => v
            .get("error")
            .and_then(|e| e.get("message").and_then(Value::as_str).or_else(|| e.as_str()))
            .or_else(|| v.get("message").and_then(Value::as_str))
            .or_else(|| v.get("detail").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_default(),
        Err(_) => String::from_utf8_lossy(&body[..body.len().min(512)]).into_owned(),
    };
    let msg = match secret {
        Some(s) => s.redact_in(&msg),
        None => msg,
    };
    clip_message(&msg, 200)
}

fn num(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64)
}

/// (開始秒, 終了秒, 文字列, no_speech_prob, avg_logprob)。
type RawSegment = (f64, f64, String, Option<f32>, Option<f32>);

fn seconds_to_us(s: f64) -> Result<i64> {
    if !s.is_finite() || !(-1.0..=86_400.0).contains(&s) {
        return Err(Error::Malformed("segment time out of range".into()));
    }
    Ok((s * 1e6).round() as i64)
}

/// verbose_json（OpenAI・whisper.cpp）と whisper.cpp の `-oj` 形式（`transcription[].offsets`）を読む。
fn parse_transcript(chunk: &ChunkInfo, reply: &HttpReply, limits: &AsrLimits, secret: Option<&Secret>) -> Result<Transcript> {
    if reply.body.len() > limits.max_response_bytes {
        return Err(Error::TooLarge("ASR response"));
    }
    if !reply.is_success() {
        return Err(Error::Status { status: reply.status, message: error_message(&reply.body, secret) });
    }
    let v: Value = serde_json::from_slice(&reply.body).map_err(|e| Error::Malformed(format!("ASR JSON: {e}")))?;
    let obj = v.as_object().ok_or(Error::Malformed("ASR response is not an object".into()))?;
    let dur = chunk.duration_us as i64;
    let mut out = Transcript { language: obj.get("language").and_then(Value::as_str).map(|s| clip_message(s, 32)), segments: Vec::new() };
    let mut raw: Vec<RawSegment> = Vec::new();
    if let Some(segs) = obj.get("segments") {
        let segs = segs.as_array().ok_or(Error::Malformed("segments is not an array".into()))?;
        if segs.len() > limits.max_segments {
            return Err(Error::TooLarge("ASR segments"));
        }
        for s in segs {
            let (Some(start), Some(end)) = (num(s, "start"), num(s, "end")) else {
                return Err(Error::Malformed("segment without start/end".into()));
            };
            let text = s.get("text").and_then(Value::as_str).ok_or(Error::Malformed("segment without text".into()))?;
            raw.push((start, end, text.to_string(), num(s, "no_speech_prob").map(|x| x as f32), num(s, "avg_logprob").map(|x| x as f32)));
        }
    } else if let Some(segs) = obj.get("transcription") {
        let segs = segs.as_array().ok_or(Error::Malformed("transcription is not an array".into()))?;
        if segs.len() > limits.max_segments {
            return Err(Error::TooLarge("ASR segments"));
        }
        for s in segs {
            let off = s.get("offsets").ok_or(Error::Malformed("transcription without offsets".into()))?;
            let (Some(from), Some(to)) = (num(off, "from"), num(off, "to")) else {
                return Err(Error::Malformed("offsets without from/to".into()));
            };
            let text = s.get("text").and_then(Value::as_str).ok_or(Error::Malformed("segment without text".into()))?;
            raw.push((from / 1000.0, to / 1000.0, text.to_string(), None, None));
        }
    } else if let Some(text) = obj.get("text").and_then(Value::as_str) {
        // 時刻のない応答（response_format=json の相手）: チャンク全体を 1 区切りとする
        raw.push((0.0, dur as f64 / 1e6, text.to_string(), None, None));
    } else {
        return Err(Error::Malformed("no segments or text in ASR response".into()));
    }
    for (start, end, text, nsp, alp) in raw {
        let s = seconds_to_us(start)?.clamp(0, dur);
        let e = seconds_to_us(end)?.clamp(0, dur);
        if e < s {
            return Err(Error::Malformed("segment ends before it starts".into()));
        }
        if let (Some(th), Some(n)) = (limits.no_speech_threshold, nsp)
            && n > th
            && alp.is_some_and(|a| a < -1.0)
        {
            continue;
        }
        let text = crate::text::normalize_spaces(&text);
        let text: String = text.chars().take(limits.max_segment_chars).collect();
        if text.is_empty() {
            continue;
        }
        out.segments.push(AsrSegment { start_us: s, end_us: e, text, no_speech_prob: nsp, avg_logprob: alp });
    }
    out.segments.sort_by_key(|s| (s.start_us, s.end_us));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::ChunkFormat;
    use crate::http::parse_multipart;
    use bytes::Bytes;

    fn chunk() -> AudioChunk {
        AudioChunk {
            info: ChunkInfo { seq: 3, start_us: 12_000_000, duration_us: 5_000_000, dropped_before: 0, discontinuity: false },
            format: ChunkFormat::M4a,
            data: Bytes::from_static(b"\x00\x00\x00\x20ftypisom"),
        }
    }

    #[test]
    fn openai_request_shape() {
        let mut a = OpenAiTranscription::new(Endpoint::parse("https://api.openai.com/v1").unwrap(), "whisper-1");
        a.api_key = Some(Secret::new("sk-test-abcdef"));
        let call = a.build_request(&chunk(), Some("前の字幕")).unwrap();
        assert_eq!(call.target, "/v1/audio/transcriptions");
        let ct = call.plain_header("content-type").unwrap().to_string();
        let parts = parse_multipart(&ct, &call.body).unwrap();
        let get = |n: &str| parts.iter().find(|p| p.0 == n).map(|p| String::from_utf8(p.3.clone()).unwrap());
        assert_eq!(get("model").as_deref(), Some("whisper-1"));
        assert_eq!(get("response_format").as_deref(), Some("verbose_json"));
        assert_eq!(get("timestamp_granularities[]").as_deref(), Some("segment"));
        assert_eq!(get("language").as_deref(), Some("ja"));
        assert_eq!(get("prompt").as_deref(), Some("前の字幕"));
        let file = parts.iter().find(|p| p.0 == "file").unwrap();
        assert_eq!(file.1.as_deref(), Some("chunk-3.m4a"));
        assert_eq!(file.2.as_deref(), Some("audio/mp4"));
        let head = String::from_utf8(call.encode_head().unwrap().to_vec()).unwrap();
        assert!(head.contains("Authorization: Bearer sk-test-abcdef\r\n"));
        assert!(head.contains("Host: api.openai.com\r\n"));
        assert!(!format!("{call:?}").contains("sk-test"));
        assert!(!format!("{a:?}").contains("sk-test"));
    }

    #[test]
    fn whisper_cpp_request_shape() {
        let a = WhisperCppServer::new(Endpoint::parse("http://10.0.0.5:8080").unwrap());
        let call = a.build_request(&chunk(), None).unwrap();
        assert_eq!(call.target, "/inference");
        let parts = parse_multipart(call.plain_header("content-type").unwrap(), &call.body).unwrap();
        assert!(parts.iter().any(|p| p.0 == "response_format" && p.3 == b"verbose_json"));
        assert!(!parts.iter().any(|p| p.0 == "prompt"));
        assert!(call.encode_head().is_ok());
    }

    #[test]
    fn parses_verbose_json() {
        let a = OpenAiTranscription::new(Endpoint::parse("http://h/v1").unwrap(), "m");
        let body = r#"{"task":"transcribe","language":"japanese","duration":5.0,"text":"こんにちは。今日は","segments":[
            {"id":0,"seek":0,"start":0.0,"end":2.5,"text":" こんにちは。","no_speech_prob":0.01,"avg_logprob":-0.2},
            {"id":1,"seek":0,"start":2.5,"end":9.0,"text":" 今日は","no_speech_prob":0.1,"avg_logprob":-0.3},
            {"id":2,"seek":0,"start":4.0,"end":4.5,"text":"ご視聴ありがとうございました","no_speech_prob":0.9,"avg_logprob":-1.5}]}"#;
        let t = a.parse_response(&chunk().info, &HttpReply::new(200, body)).unwrap();
        assert_eq!(t.language.as_deref(), Some("japanese"));
        assert_eq!(t.segments.len(), 2); // 幻聴を捨てる
        assert_eq!(t.segments[0].text, "こんにちは。");
        assert_eq!((t.segments[0].start_us, t.segments[0].end_us), (0, 2_500_000));
        assert_eq!(t.segments[1].end_us, 5_000_000); // チャンクの長さに切り詰め
        assert_eq!(t.text(), "こんにちは。今日は");
    }

    #[test]
    fn parses_whisper_cpp_offsets_and_text_only() {
        let a = WhisperCppServer::new(Endpoint::parse("http://h").unwrap());
        let body = r#"{"transcription":[{"timestamps":{"from":"00:00:00,000","to":"00:00:01,500"},"offsets":{"from":0,"to":1500},"text":" はい"}]}"#;
        let t = a.parse_response(&chunk().info, &HttpReply::new(200, body)).unwrap();
        assert_eq!(t.segments[0].end_us, 1_500_000);
        let t = a.parse_response(&chunk().info, &HttpReply::new(200, r#"{"text":"全部"}"#)).unwrap();
        assert_eq!((t.segments[0].start_us, t.segments[0].end_us), (0, 5_000_000));
    }

    #[test]
    fn rejects_malformed_and_errors() {
        let mut a = OpenAiTranscription::new(Endpoint::parse("http://h").unwrap(), "m");
        a.api_key = Some(Secret::new("sk-leaky-key"));
        let info = chunk().info;
        for bad in [
            "not json",
            "[]",
            "{}",
            r#"{"segments":{}}"#,
            r#"{"segments":[{"start":0}]}"#,
            r#"{"segments":[{"start":"a","end":1,"text":"x"}]}"#,
            r#"{"segments":[{"start":3,"end":1,"text":"x"}]}"#,
            r#"{"segments":[{"start":1e300,"end":1e301,"text":"x"}]}"#,
        ] {
            assert!(a.parse_response(&info, &HttpReply::new(200, bad)).is_err(), "{bad}");
        }
        let e = a
            .parse_response(&info, &HttpReply::new(401, r#"{"error":{"message":"Incorrect API key provided: sk-leaky-key"}}"#))
            .unwrap_err();
        match e {
            Error::Status { status, message } => {
                assert_eq!(status, 401);
                assert!(!message.contains("sk-leaky-key"), "{message}");
            }
            e => panic!("{e:?}"),
        }
        let mut small = a.clone();
        small.limits.max_response_bytes = 10;
        assert!(matches!(small.parse_response(&info, &HttpReply::new(200, "x".repeat(11))), Err(Error::TooLarge(_))));
        small.limits.max_response_bytes = 1 << 20;
        small.limits.max_segments = 1;
        let two = r#"{"segments":[{"start":0,"end":1,"text":"a"},{"start":1,"end":2,"text":"b"}]}"#;
        assert!(matches!(small.parse_response(&info, &HttpReply::new(200, two)), Err(Error::TooLarge(_))));
    }

    #[test]
    fn bad_config_rejected() {
        let mut a = OpenAiTranscription::new(Endpoint::parse("http://h").unwrap(), "");
        assert!(a.build_request(&chunk(), None).is_err());
        a.model = "m".into();
        a.language = Some("ja\"; x".into());
        assert!(a.build_request(&chunk(), None).is_err());
    }
}
