//! 手元の模擬 ASR・翻訳 HTTP server（std::net）に、本物の HTTP で要求を送って部品を端から端まで確かめる。
//! 外部の API には送らない。クライアント側の応答の解析は senca-http（ReplyReader）。

use bytes::Bytes;
use senca_caption::asr::{AsrAdapter, OpenAiTranscription, WhisperCppServer};
use senca_caption::chunk::{AudioChunker, ChunkerConfig};
use senca_caption::hls::{SubtitleSegmenter, SubtitleTrackConfig};
use senca_caption::http::{Endpoint, HttpCall, HttpReply, parse_multipart};
use senca_caption::merge::{Cue, CueMerger, MergeConfig};
use senca_caption::translate::{
    AnthropicTranslator, BatcherConfig, DeepLTranslator, Failure, OpenAiChatTranslator, RetryDecision, RetryPolicy,
    TranslationBatcher, Translator, translated_cues,
};
use senca_caption::vtt::{VttDocument, validate};
use senca_caption::{Error, Secret};
use senca_core::time::Timebase;
use senca_core::{AccessUnit, Codec, TrackId};
use senca_http::{Event, Limits, RequestHead, RequestParser};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// 台本: (開始秒, 終了秒, 文)。模擬 ASR はチャンクの窓に入る分を返す。
const SCRIPT: &[(f64, f64, &str)] = &[
    (0.3, 2.1, "皆さんこんにちは。"),
    (2.4, 4.6, "今日は微分の話をします。"),
    (5.0, 7.8, "まず関数の変化の割合を考えます。"),
    (8.2, 9.4, "これが微分係数です。"),
    (10.1, 12.6, "次に具体的な例を見てみましょう。"),
    (13.0, 15.0, "二次関数を例にとります。"),
];
const STEP_S: f64 = 4.0;
const WINDOW_S: f64 = 5.0;
const API_KEY: &str = "sk-mock-secret-0123456789";

#[derive(Default)]
struct Log {
    paths: Vec<String>,
    bodies: Vec<Vec<u8>>,
}

/// 1 接続 1 要求の模擬 server。
fn spawn_server(log: Arc<Mutex<Log>>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        for s in l.incoming() {
            let Ok(s) = s else { continue };
            let log = log.clone();
            thread::spawn(move || handle(s, log));
        }
    });
    port
}

fn read_request(s: &mut TcpStream) -> (RequestHead, Vec<u8>) {
    let limits = Limits { max_body: 32 << 20, max_buffer: 33 << 20, ..Limits::DEFAULT };
    let mut p = RequestParser::new(limits);
    let mut head = None;
    let mut body = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        loop {
            match p.poll().unwrap() {
                Event::NeedMore => break,
                Event::Head(h) => head = Some(h),
                Event::Data(d) => body.extend_from_slice(&d),
                Event::End(_) => return (head.unwrap(), body),
                Event::Closed => panic!("closed"),
            }
        }
        let n = s.read(&mut buf).unwrap();
        if n == 0 {
            p.feed_eof();
        } else {
            p.feed(&buf[..n]).unwrap();
        }
    }
}

fn respond(s: &mut TcpStream, status: u16, extra: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n", body.len());
    // 相手が先に閉じても構わない（大きすぎる応答の試験）
    let _ = s.write_all(head.as_bytes()).and_then(|_| s.write_all(body));
}

fn transcript_for(seq: u64) -> Value {
    let ws = seq as f64 * STEP_S;
    let we = ws + WINDOW_S;
    let mut segs = Vec::new();
    for &(s, e, t) in SCRIPT {
        if e <= ws || s >= we {
            continue;
        }
        let (cs, ce) = (s.max(ws), e.min(we));
        // 窓の端で切れた発話は途中までしか認識されない
        let text: String = if cs > s || ce < e {
            let n = t.chars().count();
            let frac = (ce - cs) / (e - s);
            let k = ((n as f64 * frac).floor() as usize).max(1);
            if cs > s { t.chars().skip(n - k).collect() } else { t.chars().take(k).collect() }
        } else {
            t.to_string()
        };
        segs.push(json!({"start": cs - ws, "end": ce - ws, "text": format!(" {text}"), "no_speech_prob": 0.01, "avg_logprob": -0.2}));
    }
    json!({"task": "transcribe", "language": "japanese", "duration": WINDOW_S, "segments": segs})
}

fn handle(mut s: TcpStream, log: Arc<Mutex<Log>>) {
    let (head, body) = read_request(&mut s);
    let path = head.path().to_string();
    {
        let mut l = log.lock().unwrap();
        l.paths.push(path.clone());
        l.bodies.push(body.clone());
    }
    let h = |n: &str| head.headers.get_str(n).map(str::to_string);
    match path.as_str() {
        "/v1/audio/transcriptions" | "/inference" => {
            if path.starts_with("/v1") && h("authorization").as_deref() != Some(&format!("Bearer {API_KEY}")) {
                return respond(&mut s, 401, "", br#"{"error":{"message":"bad key sk-mock-secret-0123456789"}}"#);
            }
            let parts = parse_multipart(&h("content-type").unwrap(), &body).unwrap();
            let file = parts.iter().find(|p| p.0 == "file").expect("file part");
            assert!(file.3.len() > 100);
            assert!(&file.3[4..8] == b"ftyp" || file.3.starts_with(b"OggS"));
            let name = file.1.clone().unwrap();
            let seq: u64 = name.trim_start_matches("chunk-").split('.').next().unwrap().parse().unwrap();
            let body = serde_json::to_vec(&transcript_for(seq)).unwrap();
            respond(&mut s, 200, "", &body);
        }
        "/v1/messages" => {
            assert_eq!(h("x-api-key").as_deref(), Some(API_KEY));
            assert_eq!(h("anthropic-version").as_deref(), Some("2023-06-01"));
            let req: Value = serde_json::from_slice(&body).unwrap();
            let user: Value = serde_json::from_str(req["messages"][0]["content"].as_str().unwrap()).unwrap();
            let tr: Vec<Value> = user["items"]
                .as_array()
                .unwrap()
                .iter()
                .rev() // 順番を変えて返す（番号で戻せること）
                .map(|it| json!({"n": it["n"], "text": format!("EN[{}]", it["text"].as_str().unwrap())}))
                .collect();
            let text = format!("Sure:\n```json\n{}\n```", json!({"translations": tr}));
            let resp = json!({"id":"msg_mock","type":"message","role":"assistant","model": req["model"],
                "content":[{"type":"thinking","thinking":"","signature":"s"},{"type":"text","text": text}],
                "stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}});
            respond(&mut s, 200, "", &serde_json::to_vec(&resp).unwrap());
        }
        "/v2/translate" => {
            assert_eq!(h("authorization").as_deref(), Some(&*format!("DeepL-Auth-Key {API_KEY}")));
            let req: Value = serde_json::from_slice(&body).unwrap();
            let tr: Vec<Value> =
                req["text"].as_array().unwrap().iter().map(|t| json!({"detected_source_language":"JA","text": format!("DE[{}]", t.as_str().unwrap())})).collect();
            respond(&mut s, 200, "", &serde_json::to_vec(&json!({"translations": tr})).unwrap());
        }
        "/v1/chat/completions" => {
            let req: Value = serde_json::from_slice(&body).unwrap();
            let user: Value = serde_json::from_str(req["messages"][1]["content"].as_str().unwrap()).unwrap();
            let lines: Vec<String> = user["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|it| format!("{}. ZH[{}]", it["n"], it["text"].as_str().unwrap()))
                .collect();
            // JSON ではなく番号付きの行で返す相手
            let resp = json!({"choices":[{"index":0,"message":{"role":"assistant","content": lines.join("\n")},"finish_reason":"stop"}]});
            respond(&mut s, 200, "", &serde_json::to_vec(&resp).unwrap());
        }
        "/busy/v1/messages" => respond(&mut s, 429, "Retry-After: 2\r\n", br#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#),
        "/huge/v2/translate" => respond(&mut s, 200, "", &vec![b' '; 4 << 20]),
        _ => respond(&mut s, 404, "", b"{}"),
    }
}

/// ノードの代わりの最小のクライアント（TCP + senca-http）。
fn send(call: &HttpCall) -> Result<HttpReply, Error> {
    let mut s = TcpStream::connect((call.endpoint.host.as_str(), call.endpoint.port)).unwrap();
    s.write_all(&call.encode()?).unwrap();
    let mut r = call.reader();
    let mut buf = [0u8; 8192];
    loop {
        let n = s.read(&mut buf).unwrap_or(0);
        if n == 0 {
            return r.feed_eof();
        }
        if let Some(reply) = r.feed(&buf[..n])? {
            return Ok(reply);
        }
    }
}

fn aac_aus(seconds: f64) -> Vec<AccessUnit> {
    let tb = Timebase::new(1, 48_000);
    let n = (seconds * 48_000.0 / 1024.0) as i64;
    (0..n)
        .map(|i| AccessUnit {
            track: TrackId(1),
            codec: Codec::Aac,
            epoch: 0,
            sequence: i as u64,
            config_generation: 0,
            pts: i * 1024,
            dts: i * 1024,
            duration: 1024,
            timebase: tb,
            keyframe: true,
            discontinuity: false,
            capture_time_ns: None,
            payload: Bytes::from(vec![0x21u8; 120]),
        })
        .collect()
}

fn run_pipeline(asr: &dyn AsrAdapter) -> Vec<Cue> {
    let cfg = ChunkerConfig { max_queued: 64, ..ChunkerConfig::default() };
    let mut chunker = AudioChunker::new(cfg).unwrap();
    chunker.set_config(&senca_aac::codec_config(&[0x11, 0x90], 0).unwrap()).unwrap();
    for au in aac_aus(17.0) {
        chunker.push(&au).unwrap();
    }
    chunker.flush();
    let mut merger = CueMerger::new(MergeConfig::default()).unwrap();
    let mut cues = Vec::new();
    let mut prompt = String::new();
    while let Some(chunk) = chunker.take_chunk().unwrap() {
        let call = asr.build_request(&chunk, Some(&prompt)).unwrap();
        let reply = send(&call).unwrap();
        let t = asr.parse_response(&chunk.info, &reply).unwrap();
        prompt = t.text();
        cues.extend(merger.push(&chunk.info, &t).unwrap());
    }
    cues.extend(merger.flush());
    cues
}

#[test]
fn asr_openai_and_whisper_cpp_end_to_end() {
    let log = Arc::new(Mutex::new(Log::default()));
    let port = spawn_server(log.clone());
    let want: Vec<&str> = SCRIPT.iter().map(|s| s.2).collect();

    let mut openai = OpenAiTranscription::new(Endpoint::parse(&format!("http://127.0.0.1:{port}/v1")).unwrap(), "whisper-1");
    openai.api_key = Some(Secret::new(API_KEY));
    let cues = run_pipeline(&openai);
    let texts: Vec<String> = cues.iter().map(|c| c.text.replace('\n', "")).collect();
    assert_eq!(texts, want);
    for w in cues.windows(2) {
        assert!(w[0].end_us <= w[1].start_us);
    }
    // 媒体時刻に戻っている（台本の時刻 ±0.1 秒）
    for (c, s) in cues.iter().zip(SCRIPT) {
        assert!((c.start_us as f64 / 1e6 - s.0).abs() < 0.1, "{c:?}");
    }

    let whisper = WhisperCppServer::new(Endpoint::parse(&format!("http://127.0.0.1:{port}")).unwrap());
    let cues2 = run_pipeline(&whisper);
    assert_eq!(cues2.iter().map(|c| c.text.replace('\n', "")).collect::<Vec<_>>(), want);

    // 鍵が違えば 401。誤りの文に鍵は出ない
    let mut bad = openai.clone();
    bad.api_key = Some(Secret::new("sk-wrong-key"));
    let mut chunker = AudioChunker::new(ChunkerConfig::default()).unwrap();
    chunker.set_config(&senca_aac::codec_config(&[0x11, 0x90], 0).unwrap()).unwrap();
    for au in aac_aus(6.0) {
        chunker.push(&au).unwrap();
    }
    let chunk = chunker.take_chunk().unwrap().unwrap();
    let reply = send(&bad.build_request(&chunk, None).unwrap()).unwrap();
    match bad.parse_response(&chunk.info, &reply) {
        Err(Error::Status { status: 401, message }) => assert!(!message.contains("sk-wrong-key")),
        r => panic!("{r:?}"),
    }
    let l = log.lock().unwrap();
    assert!(l.paths.iter().any(|p| p == "/inference"));
    // 2 本目以降の要求には直前の字幕が prompt として付く
    let with_prompt = l.bodies.iter().filter(|b| b.windows(13).any(|w| w == b"name=\"prompt\"")).count();
    assert!(with_prompt >= 3);
}

#[test]
fn translators_end_to_end() {
    let log = Arc::new(Mutex::new(Log::default()));
    let port = spawn_server(log);
    let base = format!("http://127.0.0.1:{port}");
    let cues: Vec<Cue> = SCRIPT
        .iter()
        .enumerate()
        .map(|(i, s)| Cue { id: i as u64 + 1, start_us: (s.0 * 1e6) as i64, end_us: (s.1 * 1e6) as i64, text: s.2.into() })
        .collect();
    let mut batcher = TranslationBatcher::new(BatcherConfig { max_items: 4, ..BatcherConfig::default() }, "ja", &["en", "de", "zh"]).unwrap();
    for c in &cues {
        batcher.push(c.clone(), 0);
    }
    let mut batches = batcher.poll(0, false);
    batches.extend(batcher.poll(0, true));
    assert_eq!(batches.len(), 6); // 4 件 + 2 件、3 言語

    let anthropic = AnthropicTranslator::new(Endpoint::parse(&base).unwrap(), Secret::new(API_KEY));
    let deepl = DeepLTranslator::new(Endpoint::parse(&base).unwrap(), Secret::new(API_KEY));
    let mut chat = OpenAiChatTranslator::new(Endpoint::parse(&format!("{base}/v1")).unwrap(), "local-llm");
    chat.json_mode = false;
    let mut per_lang: std::collections::HashMap<String, Vec<Cue>> = Default::default();
    let mut segmenters: std::collections::HashMap<String, SubtitleSegmenter> = Default::default();
    for b in &batches {
        let t: &dyn Translator = match b.target_lang.as_str() {
            "en" => &anthropic,
            "de" => &deepl,
            _ => &chat,
        };
        let reply = send(&t.build_request(b).unwrap()).unwrap();
        let res = t.parse_response(b, &reply).unwrap();
        assert!(res.iter().all(Option::is_some), "{}: {res:?}", t.name());
        per_lang.entry(b.target_lang.clone()).or_default().extend(translated_cues(&cues, b, &res, 42));
    }
    for (lang, prefix) in [("en", "EN["), ("de", "DE["), ("zh", "ZH[")] {
        let v = &per_lang[lang];
        assert_eq!(v.len(), cues.len());
        for (t, c) in v.iter().zip(&cues) {
            assert_eq!((t.id, t.start_us, t.end_us), (c.id, c.start_us, c.end_us));
            assert_eq!(t.text.replace('\n', " "), format!("{prefix}{}]", c.text), "{lang}");
        }
        // 言語ごとの字幕セグメント
        let mut seg = SubtitleSegmenter::new(SubtitleTrackConfig::new(lang, lang)).unwrap();
        for c in v {
            seg.push_cue(c.clone());
        }
        for i in 0..4u64 {
            let s = seg.close_segment(i, i as i64 * 4_000_000, 4_000_000, false).unwrap();
            validate(&s.body).unwrap();
        }
        segmenters.insert(lang.into(), seg);
        let mut doc = VttDocument::new(0);
        v.iter().for_each(|c| doc.push(c));
        assert_eq!(validate(&doc.to_vtt().unwrap()).unwrap().cues.len(), cues.len());
    }

    // 429 → Retry-After を尊重して再試行の判断
    let busy = AnthropicTranslator::new(Endpoint::parse(&format!("{base}/busy")).unwrap(), Secret::new(API_KEY));
    let reply = send(&busy.build_request(&batches[0]).unwrap()).unwrap();
    assert_eq!(reply.retry_after_s, Some(2));
    let e = busy.parse_response(&batches[0], &reply).unwrap_err();
    let d = RetryPolicy::default().decide(1, Failure::from_error(&e, reply.retry_after_s), 0, 0.3);
    assert_eq!(d, RetryDecision::RetryAfter(2000));

    // 大きすぎる応答は読む段階で拒否
    let huge = DeepLTranslator::new(Endpoint::parse(&format!("{base}/huge")).unwrap(), Secret::new(API_KEY));
    assert!(matches!(send(&huge.build_request(&batches[1]).unwrap()), Err(Error::TooLarge(_))));
}
