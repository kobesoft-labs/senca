//! 字幕（音声認識）と字幕の翻訳の部品（REQ-006 R6-4）。I/O・スレッド・時計を持たない（sans-io）。
//!
//! 流れ:
//!
//! ```text
//! 音声 AU ──▶ chunk::AudioChunker ──▶ AudioChunk（.m4a / .aac / .ogg の自己完結したファイル）
//!                                        │  asr::AsrAdapter::build_request → HttpCall（ノードが送る）
//!                                        ▼  asr::AsrAdapter::parse_response ← HttpReply
//!                                     Transcript（チャンク内の相対時刻の断片）
//!                                        │  merge::CueMerger::push
//!                                        ▼
//!                                     Cue（媒体時刻・ID 付き・確定済み）
//!                     ┌──────────────────┼──────────────────────┐
//!                     ▼                  ▼                      ▼
//!   translate::TranslationBatcher   hls::SubtitleSegmenter   vtt::VttDocument
//!   → Translator（HttpCall）         （ライブの .vtt と         （録画用の .vtt 全体）
//!   → 言語ごとの Cue                   字幕プレイリスト）
//! ```
//!
//! - [`chunk`] — 復号しない音声の切り出し（AAC → ADTS / 音声だけの MP4、Opus → Ogg Opus）。
//!   固定窓＋重なり、AU 境界、チャンク先頭の媒体時刻、上限付きのメモリー、遅れたら古いチャンクを捨てる。
//! - [`ogg`] — 最小限の Ogg ページ書き出し（RFC 3533、CRC）と Ogg Opus（RFC 7845）。
//! - [`asr`] — 音声認識の差し替え口（OpenAI 互換 `/v1/audio/transcriptions`、whisper.cpp の `/inference`）。
//! - [`merge`] — 重なったチャンクの結果を重複なく 1 本の字幕にまとめ、長さ・行で分ける。
//! - [`text`] — 日本語の表示幅、禁則を考えた改行、文字列の類似度。
//! - [`translate`] — 翻訳の差し替え口（Anthropic Messages API、DeepL v2、OpenAI 互換 chat completions）、
//!   まとめ送り、直前の文脈、再試行の判断。
//! - [`vtt`] — WebVTT の書き出し・検査（`X-TIMESTAMP-MAP` で TS/fMP4 の 90 kHz の時間軸に合わせる）。
//! - [`hls`] — ライブの HLS 字幕セグメント・字幕の media playlist・`EXT-X-MEDIA TYPE=SUBTITLES`。
//! - [`latency`] — 「字幕は遅れてよい」か「映像を遅らせて合わせる」かの遅延の見積もり。
//! - [`http`] — 送る要求（[`HttpCall`]）・受けた応答（[`HttpReply`]）・multipart/form-data。
//! - [`secret`] — API キーを表示に出さない型。
//!
//! # コーデックの方針（ADR-0004）
//!
//! 音声は**復号も符号化もしない**。AU（AAC の raw_data_block、Opus パケット）を別の入れ物に詰め替えるだけ。
//! 認識の相手（OpenAI・whisper.cpp など）が自分で復号する。
//!
//! # 外部に出るデータ（REQ-005 の外部処理・REQ-006 R6-4）
//!
//! この crate は外部の音声認識・翻訳サービスに送る**要求を組み立てるだけ**で、送るのはノード。
//! 送る先と中身は次のとおり。
//!
//! | 部品 | 送る先 | ノードの外に出るもの |
//! |---|---|---|
//! | [`asr::OpenAiTranscription`] | 設定した URL の `…/audio/transcriptions` | 音声チャンク（既定 5 秒、重なり 1 秒）、言語の指定、直前の字幕の文字列（`prompt`、設定で切れる）、API キー |
//! | [`asr::WhisperCppServer`] | 設定した URL の `…/inference` | 音声チャンク、言語の指定、直前の字幕の文字列 |
//! | [`translate::AnthropicTranslator`] | 設定した URL の `…/v1/messages` | 字幕の文字列（まとめた数件＋直前の数件の文脈）、言語の指定、API キー |
//! | [`translate::DeepLTranslator`] | 設定した URL の `…/v2/translate` | 字幕の文字列＋文脈、言語、API キー |
//! | [`translate::OpenAiChatTranslator`] | 設定した URL の `…/chat/completions` | 字幕の文字列＋文脈、言語、API キー |
//!
//! - **既定で外へ送る設定はない。** どの部品も送り先（[`http::Endpoint`]）を引数で必ず受け取る。
//!   よく使う送り先の URL は定数（[`translate::ANTHROPIC_API_URL`] など）として置くが、使うかどうかは設定する人が決める。
//! - 映像・stream key・視聴者の情報は送らない。音声チャンクには媒体時刻を入れない（ファイルの中の時刻は 0 から）。
//! - 送り先が音声・文字列を保存するか（学習に使うか）は送り先の契約で決まる。運用の設定画面では送り先と
//!   保存の有無を明示し、同意の記録を残すこと（REQ-006 R6-4）。社内の GPU サーバー（whisper.cpp・faster-whisper）
//!   に向ければ外部へは出ない。
//! - API キーは [`Secret`] に包み、`Debug`・`Display`・誤りの文に出さない。
//!
//! # ノードへのつなぎ方
//!
//! 1. 音声トラックの [`CodecConfig`](senca_core::CodecConfig) が来たら [`chunk::AudioChunker::set_config`]、
//!    AU ごとに [`chunk::AudioChunker::push`]。
//! 2. 認識の要求が出ていないとき（同時に 1 本。ライブでは並列にしない）に
//!    [`chunk::AudioChunker::take_chunk`] でチャンクを取り、[`asr::AsrAdapter::build_request`] で [`HttpCall`] を作り、
//!    ノードの HTTP クライアント（TLS は `senca-tls`）で送る。応答は [`http::ReplyReader`] で集めて
//!    [`asr::AsrAdapter::parse_response`] に渡す。失敗したら [`translate::RetryPolicy::decide`] で再試行を決める。
//! 3. 結果を**チャンクの順に** [`merge::CueMerger::push`] へ。返った確定済みの [`merge::Cue`] を
//!    字幕の言語（例: `ja`）の [`hls::SubtitleSegmenter::push_cue`] と [`vtt::VttDocument::push`]、
//!    翻訳する言語があれば [`translate::TranslationBatcher::push`] へ。
//! 4. [`translate::TranslationBatcher::poll`] が返したまとめを翻訳の部品で送り、
//!    [`translate::translated_cues`] で言語ごとの Cue にして、その言語の `SubtitleSegmenter` と `VttDocument` へ。
//! 5. 映像の HLS セグメントが閉じるたびに、同じ media sequence 番号と媒体時刻で
//!    [`hls::SubtitleSegmenter::close_segment`] を呼び、返った `.vtt` を
//!    `{stream}/{track}/sub_{lang}_{msn}.vtt`、[`hls::SubtitleSegmenter::playlist`] を `sub_{lang}.m3u8`
//!    （映像の media playlist と同じ場所）に置く。multivariant playlist には
//!    [`hls::SubtitleSegmenter::rendition`] を足し、各 `EXT-X-STREAM-INF` に `SUBTITLES="<group>"` を付ける。
//! 6. 録画の終わりに [`vtt::VttDocument::to_vtt`] を録画と同じ場所に `captions_{lang}.vtt` として保存する。
//! 7. 遅延の方針は [`latency::plan`] で数字を出し、`DelayVideo` なら映像のプレイリストの公開をその分遅らせる。

pub mod asr;
pub mod chunk;
pub mod error;
pub mod hls;
pub mod http;
pub mod latency;
pub mod merge;
pub mod ogg;
pub mod secret;
pub mod text;
pub mod translate;
pub mod vtt;

pub use asr::{AsrAdapter, AsrSegment, OpenAiTranscription, Transcript, WhisperCppServer};
pub use chunk::{AudioChunk, AudioChunker, ChunkFormat, ChunkInfo, ChunkerConfig};
pub use error::{Error, Result};
pub use hls::{SubtitleSegmenter, SubtitleTrackConfig, VttSegment};
pub use http::{Endpoint, HttpCall, HttpReply, ReplyReader};
pub use latency::{LatencyInputs, LatencyMode, LatencyReport};
pub use merge::{Cue, CueMerger, MergeConfig};
pub use secret::Secret;
pub use translate::{
    AnthropicTranslator, DeepLTranslator, OpenAiChatTranslator, RetryDecision, RetryPolicy, TranslationBatch,
    TranslationBatcher, Translator,
};
pub use vtt::{TimestampMap, VttCue, VttDocument};
