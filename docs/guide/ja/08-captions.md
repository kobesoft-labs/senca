# 8. 字幕と翻訳

[English](../en/08-captions.md)

配信中のストリームの音声を音声認識サービスへ送って字幕を作り、必要に応じて翻訳します。
字幕は言語ごとに WebVTT の HLS 字幕として配信します。

- 設計の背景: [ADR-0011 字幕と翻訳](../../dev/adr/0011-captions.md)、[ADR-0004 コーデックと特許](../../dev/adr/0004-codecs-patents.md)（音声をデコードしない理由）
- 関連するページ: [4. 取り込みと再生](04-ingest-playback.md)、[6. 入力と出力](06-inputs-outputs.md)（シークレット・外向きの接続の制限）、[11. メディアパイプライン](11-media-pipelines.md)（部屋の参加者の字幕）、[12. セキュリティ](12-security.md)

例で使う `API`・`TOKEN`・`H` は [4. 取り込みと再生](04-ingest-playback.md) と同じです。

## 8.1 しくみ

```
音声の AU ─▶ 5 秒の窓（1 秒ずつ重ねる）に詰め替え ─▶ 音声認識 ─▶ 重なりを統合した字幕（cue）
                （デコードしない: AAC → m4a、Opus → Ogg）                     │
                                                         ┌───────────────┴──────────────┐
                                                         ▼                              ▼
                                               元の言語の WebVTT             まとめて翻訳 → 言語ごとの WebVTT
                                                         └──────────── HLS 字幕（sub_{lang}.m3u8）＋ master.m3u8
```

- **設定が無ければ、外部へは何も送信しません。** ストリームに `captions` を設定したときだけ、設定した送信先へ音声とテキストを送ります。
- 音声は**デコードしません**。AAC は音声だけの MP4（`.m4a`）に、Opus は Ogg Opus（`.ogg`）にそのまま詰め替えて送り、送信先でデコードします。
  それ以外の音声（G.711 など）は字幕にできません。
- 字幕処理は、配信者が接続したときに始まり、配信が終わったとき（再接続の猶予が切れたとき）に止まります。
  配信中に `captions` を変更しても、反映されるのは次の配信からです。
- クラスタでは、配信を受信したノード（ストリームの所有者）で字幕処理が動きます。中継で作られるミラーは字幕を作りません。
  字幕のファイルは所有者のノードのメモリにだけあるため、所有者でないノードがミラーから配信している再生 URL では、
  字幕のファイル（`master.m3u8` など）は 404 になります。

## 8.2 設定（ストリームの `captions`）

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
| `asr_provider` | （必須） | `openai`・`whisper_cpp` | 音声認識サービスの種類 |
| `asr_endpoint` | （必須） | `http://` か `https://` の URL | 送信先のベース URL（パスを含めてもかまいません） |
| `asr_credential_ref` | なし | `secret:{name}`、テナントのシークレット | API キー（`openai` で使用） |
| `asr_model` | `whisper-1` | | `openai` のモデル名 |
| `language` | `ja` | 1〜8 文字の英数字と `-` | 話されている言語（ISO 639-1）。音声認識に渡し、元の言語の字幕の言語にもなります |
| `translate_provider` | なし | `anthropic`・`deepl`・`openai` | 指定すると翻訳します |
| `translate_endpoint` | なし | `translate_provider` があれば必須 | 翻訳の送信先のベース URL |
| `translate_credential_ref` | なし | `secret:{name}` | API キー（`anthropic`・`deepl` では必須） |
| `translate_model` | `anthropic`: `claude-haiku-5-5`、`openai`: `gpt-4o-mini` | | `deepl` では使いません |
| `targets` | `[]` | 8 個まで。形式は `language` と同じ | 翻訳先の言語 |

- `captions: null` を指定すると、字幕を止めます。
- 参照するシークレットが存在しないと 400 になります。字幕から参照されているシークレットは削除できません（409）。
  シークレットの作成には `tenant:admin` の権限が必要です（[6. 入力と出力](06-inputs-outputs.md)）。
- `anthropic`・`deepl` で `translate_credential_ref` が無い、シークレットを復号できない、などの設定の誤りは API では検出されません。
  配信の開始時に字幕処理がログに理由を書いて停止します（配信そのものは続きます）。

## 8.3 送信するリクエスト

`asr_endpoint`・`translate_endpoint` の後ろに次のパスを付けて POST します。

| 種類 | パス | 認証 | 本文 |
|---|---|---|---|
| `openai`（音声認識） | `{asr_endpoint}/audio/transcriptions` | `Authorization: Bearer {key}`（キーがあれば） | multipart: `file`、`model`、`response_format=verbose_json`、`timestamp_granularities[]=segment`、`language`、`prompt`（直前の認識結果の末尾 200 文字） |
| `whisper_cpp`（音声認識） | `{asr_endpoint}/inference` | なし | multipart: `file`、`response_format=verbose_json`、`language`、`prompt` |
| `anthropic`（翻訳） | `{translate_endpoint}/v1/messages` | `x-api-key`、`anthropic-version: 2023-06-01` | JSON |
| `deepl`（翻訳） | `{translate_endpoint}/v2/translate` | `Authorization: DeepL-Auth-Key {key}` | JSON |
| `openai`（翻訳） | `{translate_endpoint}/chat/completions` | `Authorization: Bearer {key}`（キーがあれば） | JSON |

したがって、エンドポイントは次のように指定します。

- OpenAI の音声認識: `https://api.openai.com/v1`（OpenAI 互換のサーバーなら、その `/v1` まで）
- whisper.cpp の server: `http://whisper.internal:8080`
- Anthropic: `https://api.anthropic.com`
- DeepL: `https://api.deepl.com`（Free プランは `https://api-free.deepl.com`）
- OpenAI の翻訳: `https://api.openai.com/v1`

1 回のリクエストのタイムアウトは 60 秒です。API キーはログやエラーメッセージに出力しません。

**外向きの接続の制限**: 送信先も入力・出力と同じく、名前解決した後の IP アドレスで検査します。社内の whisper.cpp など、
プライベートアドレスのサーバーを使うときは `--egress-allow` で範囲を許可してください（[6. 入力と出力](06-inputs-outputs.md)）。

例（社内の whisper.cpp で音声認識し、DeepL で英語に翻訳する）:

```sh
curl -s -X POST "$API/secrets" "${H[@]}" -d '{"name":"deepl","value":"xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx:fx"}'
curl -s -X POST "$API/streams" "${H[@]}" -d '{
  "name": "lecture",
  "captions": {"asr_provider": "whisper_cpp", "asr_endpoint": "http://10.0.0.5:8080", "language": "ja",
               "translate_provider": "deepl", "translate_endpoint": "https://api-free.deepl.com",
               "translate_credential_ref": "secret:deepl", "targets": ["en"]}
}'
# ノードの起動時に --egress-allow 10.0.0.5/32 が必要
```

## 8.4 音声の切り出しと認識

- 5 秒の窓を、1 秒ずつ重ねながらずらして切り出します。窓の境目は AU の境目に合わせます。各チャンクは先頭のメディア時刻を持ち、
  認識結果の時刻をストリームの時刻に変換するのに使います。
- 欠落（不連続やタイムスタンプの飛び）があれば、そこで切り、次から新しい窓を始めます。
- 音声認識は 1 チャンクずつ、受信とは別のタスクで行います。認識が実時間より遅れて待ち行列（2 チャンク）があふれた場合は、
  **古いチャンクから捨てます**（字幕が欠けるだけで、配信は止まりません）。
- 重なった部分の認識結果は重複を除いて 1 本の字幕にまとめます。1 つの字幕（cue）は 0.7〜7 秒、1 行 64 桁（日本語の表示幅を考慮）・2 行までに分割します。
- 翻訳では、確定した字幕を最大 6 個・1500 文字・1.5 秒ごとにまとめて送り、直前の 3 個を文脈として添えます。
  翻訳に失敗するとログに記録され、その分の訳が欠けます。

## 8.5 HLS 字幕

字幕は**ライブの HLS** で配信します。ライブの再生 URL と同じ署名付きのパスの下に、次のファイルが作られます。

| ファイル | 内容 |
|---|---|
| `master.m3u8` | multivariant playlist。言語ごとの `EXT-X-MEDIA:TYPE=SUBTITLES`（グループ `subs`。`language` の言語が `DEFAULT`）と、映像の `index.m3u8` を指す `EXT-X-STREAM-INF:BANDWIDTH=3000000,SUBTITLES="subs"` を含みます |
| `sub_{lang}.m3u8` | 言語ごとの字幕の media playlist |
| `sub_{lang}_{n}.vtt` | 字幕のセグメント。番号と区切りは映像のセグメントと同じです。`X-TIMESTAMP-MAP` で TS・fMP4 の時間軸に合わせます |

- 字幕付きで再生するには、再生 URL の `index.m3u8` を `master.m3u8` に置き換えて開きます（署名はファイル名を含まないので、そのまま使えます）。
- `master.m3u8` は、字幕処理が始まってから最初の映像セグメントが確定したときに作られます。それまでは 404 です。
- 字幕のセグメントは、メモリに最大 256 個まで保持します。

```sh
HLS=$(curl -s -X POST "$API/streams/$SID/playback-url" "${H[@]}" | python3 -c "import json,sys;print(json.load(sys.stdin)['playback']['hls'])")
MASTER=${HLS%index.m3u8}master.m3u8
curl -s "$MASTER"
curl -s "${HLS%index.m3u8}sub_en.m3u8"
```

字幕が付かないもの:

- 録画（VOD）と MP4 の書き出し。録画には字幕を保存しません。
- WebRTC（WHEP）、RTMP/SRT/RTSP の play。

## 8.6 遅延の扱い

SENCA は「**字幕は遅れて表示されてもよい**」という方針で、映像は遅らせません。

- 字幕は、音声が窓にたまり（最大 5 秒）、音声認識（と翻訳）の結果が返ってから表示されます。認識には通常数秒かかるため、
  字幕は話した時点から数秒〜十数秒遅れます。
- 字幕が届いた時点で、その時刻の映像セグメントがすでに確定している場合は、字幕の開始時刻を、まだ確定していないセグメントの時刻へずらして出力します
  （表示時間は、元の長さと 0.5 秒の長い方です）。プレイヤーは、現在のセグメントで字幕を表示します。
- 映像を遅らせて字幕と同期させる方式には、現在は対応していません。

## 8.7 うまく動かないとき

ノードのログに `captions {stream}: ...` という行が出力されます。

| ログ | 原因 |
|---|---|
| `asr failed: ...` | 音声認識の送信先の誤り、認証の失敗、タイムアウト。その分の字幕が欠けます |
| `translation failed: ...` | 翻訳の失敗 |
| `... は許されていない宛先です` | 送信先がプライベートアドレス（`--egress-allow` が必要） |
| `翻訳の資格情報がありません` など | `anthropic`・`deepl` で API キーが無い。字幕処理は始まりません |
| 何も出力されない | 音声が AAC・Opus ではない、または配信の途中で `captions` を設定した（次の配信から反映） |
