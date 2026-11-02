//! FLV ファイルの逐次読み込み（FlvReader）と、タグ本体の取り込み（FlvIngest・script・タグの解析）。
//! 入力は FLV ファイルそのもの。読み込みを不規則な大きさに切って与える。
#![no_main]
use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use senca_flv::{FlvIngest, FlvReader, IngestOptions, TagType};

fuzz_target!(|data: &[u8]| {
    let mut r = FlvReader::new();
    let mut ingest = FlvIngest::new(IngestOptions::default());
    let mut events = Vec::new();
    // 区切りの大きさは先頭のバイトから決める（境界の扱いを揺らす）
    let step = 1 + data.first().copied().unwrap_or(0) as usize * 7;
    for chunk in data.chunks(step) {
        r.feed(chunk);
        loop {
            match r.next_tag() {
                Ok(Some(tag)) => {
                    match tag.tag_type {
                        TagType::Script => {
                            if let Ok(sd) = senca_flv::script::ScriptData::parse(&tag.body) {
                                let _ = sd.metadata();
                                let _ = sd.strip_set_data_frame();
                            }
                        }
                        TagType::Video => {
                            let _ = senca_flv::tag::parse_video(&tag.body);
                        }
                        TagType::Audio => {
                            let _ = senca_flv::tag::parse_audio(&tag.body);
                        }
                    }
                    let _ = ingest.push(tag.tag_type, tag.timestamp, &tag.body, &mut events);
                    events.clear();
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
    // 生のタグ本体としても取り込む（RTMP の media message と同じ経路）
    let body = Bytes::copy_from_slice(data);
    let mut ingest = FlvIngest::new(IngestOptions::default());
    let _ = ingest.push_video(0, &body, &mut events);
    let _ = ingest.push_audio(0, &body, &mut events);
});
