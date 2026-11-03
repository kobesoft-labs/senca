# 8. 字幕と翻訳

[English](../en/08-captions.md)

配信中の stream の音声を音声認識のサービスへ送って字幕にし、必要なら翻訳して、言語ごとの WebVTT の HLS 字幕として出す。

- 設計の理由: [ADR-0011 字幕と翻訳](../../dev/adr/0011-captions.md)、[ADR-0004 コーデックと特許](../../dev/adr/0004-codecs-patents.md)（音声を復号しない）
- 関係するページ: [4. 取り込みと再生](04-ingest-playback.md)、[6. 入力と出力](06-inputs-outputs.md)（秘密値・外向きの接続の制限）、[11. メディアパイプライン](11-media-pipelines.md)（部屋の参加者の字幕）、[12. セキュリティ](12-security.md)
- 実装: `crates/senca-node/src/captions.rs`、`crates/senca-caption`（`chunk.rs`・`asr.rs`・`merge.rs`・`translate.rs`・`hls.rs`・`vtt.rs`）

例の `API`・`TOKEN`・`H` は [4. 取り込みと再生](04-ingest-playback.md) と同じ。

## 8.1 しくみ

```
音声の AU ─▶ 5 秒の窓（1 秒重ねる）に詰め替え ─▶ 音声認識 ─▶ 重なりを統合した字幕（cue）
                （復号しない: AAC → m4a、Opus → Ogg）                     │
                                                         ┌───────────────┴──────────────┐
                                                         ▼                              ▼
                                               元の言語の WebVTT             まとめて翻訳 → 言語ごとの WebVTT
                                                         └──────────── HLS 字幕（sub_{lang}.m3u8）＋ master.m3u8
```

- **設定が無ければ外部へは何も送らない。** stream の `captions` を設定したときだけ、その送り先へ音声とテキストを送る。
- 音声は**復号しない**。AAC は音声だけの MP4（`.m4a`）、Opus は Ogg Opus（`.ogg`）にそのまま詰め替えて送り、送り先が復号する。
  それ以外の音声（G.711 など）は字幕にできない。
- 字幕の係は送り手がつながったときに始まり、配信が終わる（再接続の猶予が切れる）と止まる。配信中に `captions` を変えても、
  効くのは次の配信から。
- クラスタでは、送り手を受けた（所有者の）ノードで動く。中継の写しは字幕を作らない。字幕のファイルは所有者のノードのメモリにあり、
  所有者でないノードで写しから配っている再生 URL では、そのノードが字幕のファイル（`master.m3u8`・`sub_{lang}.m3u8`・`sub_{lang}_{n}.vtt`）を
  所有者のノードから取って返す（プレイリストは 1 秒、字幕のセグメントは 30 秒の間、使い回す）。どのノードの再生 URL でも字幕を見られる。

## 8.2 設定（stream の `captions`）

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"openai","value":"sk-..."}'
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"anthropic","value":"sk-ant-..."}'

curl -s -X PATCH "$API/streams/$SID" "${H[@]}" -H "If-Match: $ETAG" -d '{
  "captions": {
    "asr_provider": "openai",
    "asr_endpoint": "https://api.openai.com/v1",
    "asr_credential_ref": "secret:openai",
    "language": "ja",
    "translate_provider": "anthropic",
    "translate_endpoint": "https://api.anthropic.com",
    "translate_credential_ref": "secret:anthropic",
    "targets": ["en", "zh"]
  }
}'
```

| フィールド | 既定 | 検査 | 意味 |
|---|---|---|---|
| `asr_provider` | （必須） | `openai`・`whisper_cpp` | 音声認識の種類 |
| `asr_endpoint` | （必須） | `http://` か `https://` の URL | 送り先の基の URL（パスを含めてよい） |
| `asr_credential_ref` | なし | `secret:{name}`、テナントの秘密値 | API キー（`openai` で使う） |
| `asr_model` | `whisper-1` | | `openai` のモデル名 |
| `language` | `ja` | 1〜8 文字の英数字と `-` | 話している言語（ISO 639-1）。認識に渡し、元の言語の字幕の言語になる |
| `translate_provider` | なし | `anthropic`・`deepl`・`openai` | 書けば翻訳する |
| `translate_endpoint` | なし | `translate_provider` があれば必須 | 翻訳の送り先の基の URL |
| `translate_credential_ref` | なし | `secret:{name}` | API キー（`anthropic`・`deepl` では必須） |
| `translate_model` | `anthropic`: `claude-haiku-5-5`、`openai`: `gpt-4o-mini` | | `deepl` では使わない |
| `targets` | `[]` | 8 個まで、形は `language` と同じ | 翻訳先の言語 |

- `captions: null` で字幕をやめる。
- 参照する秘密値が無いと 400。字幕が参照している秘密値は消せない（409）。秘密値の作成には `tenant:admin` が要る（[6. 入力と出力](06-inputs-outputs.md)）。
- `anthropic`・`deepl` で `translate_credential_ref` が無い、秘密値を開けないなどの設定の誤りは API では検出されず、配信の開始時に字幕の係が
  ログに理由を書いて止まる（配信は続く）。

## 8.3 送る要求

`asr_endpoint`・`translate_endpoint` の後ろに次のパスを付けて POST する。

| 種類 | パス | 認証 | 本文 |
|---|---|---|---|
| `openai`（音声認識） | `{asr_endpoint}/audio/transcriptions` | `Authorization: Bearer {key}`（鍵があれば） | multipart: `file`、`model`、`response_format=verbose_json`、`timestamp_granularities[]=segment`、`language`、`prompt`（直前の認識結果の末尾 200 文字） |
| `whisper_cpp`（音声認識） | `{asr_endpoint}/inference` | なし | multipart: `file`、`response_format=verbose_json`、`language`、`prompt` |
| `anthropic`（翻訳） | `{translate_endpoint}/v1/messages` | `x-api-key`、`anthropic-version: 2023-06-01` | JSON |
| `deepl`（翻訳） | `{translate_endpoint}/v2/translate` | `Authorization: DeepL-Auth-Key {key}` | JSON |
| `openai`（翻訳） | `{translate_endpoint}/chat/completions` | `Authorization: Bearer {key}`（鍵があれば） | JSON |

したがって `asr_endpoint` は `https://api.openai.com/v1`（OpenAI 互換のサーバーならその `/v1` まで）、whisper.cpp の server なら
`http://whisper.internal:8080`、Anthropic は `https://api.anthropic.com`、DeepL は `https://api.deepl.com`（Free は `https://api-free.deepl.com`）、
OpenAI の翻訳は `https://api.openai.com/v1` のように書く。

1 回の要求の時間切れは 60 秒。API キーはログ・エラーに出さない。

**外向きの接続の制限**: 送り先も入力・出力と同じく、解決した IP で検査する。社内の whisper.cpp など私設アドレスのサーバーを使うときは
`--egress-allow` で範囲を許す（[6. 入力と出力](06-inputs-outputs.md)）。

例（社内の whisper.cpp で認識し、DeepL で英語に訳す）:

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"deepl","value":"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx:fx"}'
curl -s -X POST "$API/streams" "${H[@]}" -d '{
  "name": "lecture",
  "captions": {"asr_provider": "whisper_cpp", "asr_endpoint": "http://10.0.0.5:8080", "language": "ja",
               "translate_provider": "deepl", "translate_endpoint": "https://api-free.deepl.com",
               "translate_credential_ref": "secret:deepl", "targets": ["en"]}
}'
# ノードの起動に --egress-allow 10.0.0.5/32 が要る
```

## 8.4 音声の切り出しと認識

- 5 秒の窓を 1 秒ずつ重ねてずらす。境目は AU の境目に合わせ、各チャンクは先頭の媒体時刻を持つ（認識結果の時刻をストリームの時刻に直す）。
- 欠落（不連続・時刻の飛び）があればそこで切り、次から新しい窓を始める。
- 認識は 1 本ずつ、配信の受信とは別のタスクで行う。認識が実時間より遅れて待ち行列（2 チャンク）があふれたら、**古いチャンクから捨てる**
  （字幕が欠けるだけで配信は止めない）。
- 重なった結果は重複を除いて 1 本の字幕にまとめ、1 つの字幕を 0.7〜7 秒、1 行 64 桁（日本語の表示幅を考える）・2 行までに分ける。
- 翻訳は確定した字幕を最大 6 個・1500 文字・1.5 秒ごとにまとめて送り、直前の 3 個を文脈として添える。翻訳の失敗はログに残り、その分の訳が欠ける。

## 8.5 HLS 字幕

字幕は**ライブの HLS** に出る。ライブの再生 URL と同じ署名付きのパスの下に、次のファイルが現れる。

| ファイル | 中身 |
|---|---|
| `master.m3u8` | multivariant playlist。言語ごとの `EXT-X-MEDIA:TYPE=SUBTITLES`（グループ `subs`、`language` の言語が `DEFAULT`）と、映像の `index.m3u8` を指す `EXT-X-STREAM-INF:BANDWIDTH=3000000,SUBTITLES="subs"` |
| `sub_{lang}.m3u8` | 言語ごとの字幕の media playlist |
| `sub_{lang}_{n}.vtt` | 字幕のセグメント。番号と区切りは映像のセグメントと同じ。`X-TIMESTAMP-MAP` で TS・fMP4 の時間軸に合わせる |

- 字幕を見るには、再生 URL の `index.m3u8` を `master.m3u8` に置き換えて開く（署名はファイル名を含まないので同じ URL のまま使える）。
- `master.m3u8` は字幕の係が始まって最初の映像セグメントが閉じたときにでき、それまでは 404。
- 字幕のセグメントはメモリに最大 256 個まで残す。

```sh
HLS=$(curl -s -X POST "$API/streams/$SID/playback-url" "${H[@]}" | python3 -c "import json,sys;print(json.load(sys.stdin)['playback']['hls'])")
MASTER=${HLS%index.m3u8}master.m3u8
curl -s "$MASTER"
curl -s "${HLS%index.m3u8}sub_en.m3u8"
```

字幕が出ない所:

- 録画（VOD）と MP4 の書き出し。録画には字幕を保存しない。
- WebRTC（WHEP）・RTMP/SRT/RTSP play。

## 8.6 遅れの扱い

SENCA は「**字幕は遅れて出てよい**」方式で、映像は遅らせない。

- 字幕は音声が窓にたまり（最大 5 秒）、認識（と翻訳）が返ってから出る。認識に数秒かかるのがふつうなので、字幕は話より数秒〜十数秒遅れる。
- 字幕が届いた時点で、その時刻の映像セグメントがすでに閉じていたら、字幕の始まりを開いているセグメントの時刻へずらして出す
  （長さは元の長さか 0.5 秒の長い方）。プレイヤーは今のセグメントに字幕を表示する。
- 映像を遅らせて字幕と合わせる方式は、見積もりの部品（`senca-caption` の `latency`）はあるが、ノードでは使っていない。

## 8.7 うまく動かないとき

ノードのログに `captions {stream}: ...` の行が出る。

| ログ | 原因 |
|---|---|
| `asr failed: ...` | 認識の送り先の誤り・認証・時間切れ。字幕はその分欠ける |
| `translation failed: ...` | 翻訳の失敗 |
| `... は許されていない宛先です` | 送り先が私設アドレス（`--egress-allow`） |
| `翻訳の資格情報がありません` など | `anthropic`・`deepl` で鍵が無い。字幕の係は始まらない |
| 何も出ない | 音声が AAC・Opus でない、`captions` を配信の途中で設定した（次の配信から） |
