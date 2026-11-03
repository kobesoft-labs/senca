# 11. メディアパイプライン

[English](../en/11-media-pipelines.md)

部屋（[10-rooms.md](10-rooms.md)）の参加者を SENCA の stream に書き出し、録画・合成・送り出し・文字起こしを 1 つの資源で指定する。
AWS Chime SDK Media Pipelines に相当する。設計の理由は [ADR-0016](../../adr/0016-media-pipelines.md)。

- API: `/api/v1/media-pipelines`（`crates/senca-node/src/api/media_pipelines.rs`）
- 動かす係: `crates/senca-node/src/mediapipes.rs`
- 端から端までの試験: `tools/interop/node_media_pipelines.sh`（Chrome 2 ページ）

## 1. 対応表

| AWS（Chime SDK Media Pipelines） | SENCA |
|---|---|
| Media stream pipeline（Kinesis Video Streams へ） | 参加者ごとの stream（`artifacts.participant_streams`）。以後は普通の stream と同じく HLS・録画・出力・パイプラインに乗る |
| Media capture pipeline（会議を録る） | `recording`。参加者ごとの stream と合成の stream を自動録画する |
| Media concatenation pipeline | 普通の録画の API。`GET /recordings/{id}/export.mp4` で 1 本の MP4 にまとめる |
| Media live connector pipeline（会議を RTMP へ） | `egress`。合成の stream の出力（RTMP・SRT・PaaS）。`composite` が要る |
| Media insights pipeline（文字起こし） | `captions`。参加者ごとの stream の字幕・翻訳（[08-captions.md](08-captions.md)） |

## 2. しくみ

- メディアパイプラインの定義は制御状態の資源で、クラスターのどのノードの API でも受ける。
- **動かすのは部屋を置いたノード**（`GET /rooms/{id}` の `placement_status.node`）。そのノードの係が **1 秒ごと**に部屋の参加者を見て、
  stream・合成・送り出しを突き合わせる。部屋が別のノードへ移れば、移った先の係が引き継ぐ（[09-cluster-grid.md](09-cluster-grid.md)）。
- `desired_state: running` のものだけ動かす。
- 子の資源（stream・合成のパイプライン・送り出しの出力）の ID はメディアパイプラインの ID から決まる（何度作り直しても同じ ID）。
  `artifacts` に出る。合成のパイプラインと出力は `GET /pipelines/{id}`・`GET /outputs/{id}` でも読める。

### 2.1 参加者ごとの stream（再符号化しない）

- 参加者ごとに stream を 1 本作る。参加者の鍵は `client_id`（無ければ `connection_id`）。
- `participants` が空なら全員。書けば、その `client_id` の参加者だけ（`client_id` の無い参加者は対象外）。
- 送ってきた映像の**いちばん良い層**（simulcast の rid は低い順に並ぶので、最後の rid）と音声を、**再符号化せずに**そのまま stream に流す。
  映像は最初のキーフレームから流し始める。
- 書き出している間、サーバーは送り手に **2 秒ごと**にキーフレームを求める（途中から見る HLS・録画が早く始められるように）。
  書き出しが追いつかず取りこぼしたときもキーフレームを求め、次のキーフレームからやり直す。
- 映像も音声も無い参加者は書き出さない。映像トラック・音声トラックはそれぞれ最初の 1 本だけ使う。
- 送るトラックや層が変わったら、その参加者の書き出しを作り直す。参加者が去れば書き出しを止める（stream は残る）。
- カスケードで別のノードにいる参加者は書き出さない（ノードではカスケードは有効になっていない。[10-rooms.md](10-rooms.md)）。

stream の属性:

| 項目 | 値 |
|---|---|
| 名前 | `{name}-{参加者}-{stream ID の頭 6 文字}`（英数字と `-_.` 以外は `-` に置き換え、前半は 56 文字まで） |
| `metadata` | `media_pipeline`（メディアパイプラインの ID）、`room`、`participant` |
| `codec_profile` | 部屋の `video_codec` が `h264`・`h265` なら `any`、ほか（既定 `vp8`）は `royalty_free` |
| `hls.container` | `fmp4` |
| 送り手 | 部屋の書き出しだけ。publish 鍵では受けない（使えない鍵のハッシュが入っている） |

`export_participants: false` にしても、合成の入力として参加者ごとの stream は作られる。偽にすると、その stream を録画（`recording.participants`）・
文字起こし（`captions`）の対象にできなくなる。

### 2.2 合成（composite）

映像のある参加者を並べ、音声はすべて（映像の無い参加者も）0 dB で混ぜ、1 本の stream にするパイプライン（[07-pipelines.md](07-pipelines.md)）を作る。

- 参加者の stream が流れ始めてから図に入れる（入力を待つ間に合成が止まらないように）。参加者が変われば図を直す。
- 映像のある参加者がいなければ、合成のパイプラインは `stopped`。
- 合成のパイプラインは部屋を置いたノードに固定し（`placement.node`）、優先度は部屋と同じ。部屋が移ると、元のノードの係が止め、移った先の係が置き直す。
- 合成の stream の名前は `{name}-composite-{ID の頭 6 文字}`、パイプラインは `{name}-compose-{ID の頭 6 文字}`。
- 合成の stream の `codec_profile` は、合成の `video_codec` と部屋の `video_codec` のどちらも H.264・H.265 でなければ `royalty_free`、そうでなければ `any`。

並べ方:

| `layout` | 並べ方 |
|---|---|
| `grid` | 映像の人数を n として、列 = ⌈√n⌉、行 = ⌈n / 列⌉ の格子。参加者の鍵の順 |
| `speaker` | 最初の参加者（参加者の鍵の順）を上に大きく（高さの 3/4）、残りを下の帯（高さの 1/4）に等分して並べる。映像が 1 人なら `grid` と同じ。話者の検出はまだ合成に使っていない |

符号化:

| 項目 | 値 |
|---|---|
| 映像 | `composite.video_codec`、`composite.bitrate_kbps`、GOP = `fps × 2`（2 秒ごとにキーフレーム） |
| 音声 | Opus 96 kbps |
| 時計 | `realtime` |

合成は codec worker で復号・合成・符号化する。node を codec worker 付きで動かす（`--codec-workers`・`--codec-lib-dir`。[07-pipelines.md](07-pipelines.md)）。
H.264・H.265 の符号化には GPU が要る。作成時には検査せず、合成のパイプラインの配置・検証で失敗して、そのパイプラインの `placement_status` に出る。

### 2.3 録画（capture）とまとめ（concatenation）

- `recording.participants: true` で参加者ごとの stream、`recording.composite: true` で合成の stream に、自動録画の方針
  （`recording.enabled: true`、`retention_days`）を写す。stream が流れると録画が始まり、送り手が去ると確定する（[05-recording.md](05-recording.md)）。
- 録画は普通の録画の資源。`GET /recordings?stream={stream ID}` で探し、`GET /recordings/{id}/export.mp4` で 1 本の MP4 に書き出す。
  - 断片をつなぎ、先頭に `moov` を置いた MP4 にする。再符号化しない。途中の不連続（入力の切れ目）は時刻をずらしてつなぐ。
  - 録画中は 409 `CONFLICT`。4 GiB を超える録画は 413 `PAYLOAD_TOO_LARGE`。scope は `recording:read`。
- 置き場所はノードのデータディレクトリ（`--data-dir`）。

### 2.4 送り出し（live connector）

- `egress` の各要素は、合成の stream の出力になる（出力の資源と同じ形と検査。[06-inputs-outputs.md](06-inputs-outputs.md)）。
- 使える `protocol` は `rtmp_push`（`rtmp://`・`rtmps://`）、`srt_push`（`srt://`）、`paas`（`provider` は `cloudflare_stream`・`aws_ivs`・`aws_medialive`）。
- URL に資格情報を入れない（`user@host` は 400）。資格情報は `credential_ref: "secret:{名前}"` で秘密値を指す。
- 宛先の IP は接続のときに検査する。ループバック・私設アドレスなどへは、管理者が `--egress-allow CIDR` で許したときだけ出せる（[12-security.md](12-security.md)）。

### 2.5 文字起こし（insights）

- `captions` は stream の `captions` と同じ形（`CaptionSettings`。[08-captions.md](08-captions.md)）。参加者ごとの stream それぞれに付ける。
- 合成の stream には付けない。`export_participants: true` が要る。

## 3. 資源

### 3.1 フィールド

| フィールド | 型 | 既定 | 意味 |
|---|---|---|---|
| `name` | string | （必須） | 1〜40 文字の英数字と `-_.`、`.` で始まらない。テナント内で一意（子の stream の名前に使うので stream の 64 文字より短い） |
| `room` | string | （必須） | 部屋の ID。テナントの部屋でなければ 400。作った後は変えられない |
| `participants` | string[] | `[]`（全員） | 書き出す参加者の `client_id`。100 人まで、各 1〜255 バイト |
| `export_participants` | bool | `true` | 参加者ごとの stream を成果物として扱う。`recording.participants`・`captions` に要る |
| `composite` | object | 無し | 合成。下の表 |
| `recording` | object | 無し | 録画。下の表 |
| `egress` | object[] | `[]` | 送り出し。8 個まで。`composite` が要る |
| `captions` | object | 無し | 文字起こし・翻訳（`CaptionSettings`） |
| `desired_state` | string | `running` | `running` か `stopped` |

`composite`:

| フィールド | 既定 | 範囲 |
|---|---|---|
| `layout` | `grid` | `grid`・`speaker` |
| `width` | 1280 | 64〜3840 の偶数 |
| `height` | 720 | 64〜2160 の偶数 |
| `fps` | 30 | 1〜60 |
| `video_codec` | `av1` | `av1`・`vp9`・`vp8`・`h264`・`h265`（H.264・H.265 は GPU が要る） |
| `bitrate_kbps` | 2500 | 100〜50000 |

`recording`:

| フィールド | 既定 | 意味 |
|---|---|---|
| `participants` | `true` | 参加者ごとの stream を録る。`export_participants: true` が要る |
| `composite` | `true` | 合成の stream を録る。`composite` が要る |
| `retention_days` | 無し | 1〜36500 |

`recording` の `participants`・`composite` は**省くと true**。`composite` の無いメディアパイプラインで `recording` を書くなら `"composite": false` を明示する（しないと 400）。

`egress[]`:

| フィールド | 意味 |
|---|---|
| `protocol` | `rtmp_push`・`srt_push`・`paas` |
| `url` | `paas` 以外で必須。2048 文字まで、空白・制御文字・資格情報を含まない |
| `credential_ref` | `secret:{名前}`。テナントの秘密値が無ければ 400。`paas` では必須（API トークン） |
| `provider` | `paas` で必須 |
| `options` | 16 個まで、キー 1〜64 文字・値 256 文字まで |

知らないフィールドは 400（`INVALID_REQUEST`）。`Content-Type` が `application/json` でなければ 415。

### 3.2 応答

```json
{
  "id": "…", "name": "mtg", "room": "meet", "generation": 1,
  "participants": [], "export_participants": true,
  "composite": {"layout":"grid","width":640,"height":360,"fps":30,"video_codec":"av1","bitrate_kbps":800},
  "recording": {"participants":true,"composite":true},
  "egress": [{"protocol":"rtmp_push","url":"rtmp://…"}],
  "desired_state": "running",
  "created_at": "…", "updated_at": "…",
  "artifacts": {
    "participant_streams": [{"stream":"…","name":"mtg-alice-1a2b3c","participant":"alice"}],
    "composite": {"stream":"…","pipeline":"…","outputs":["…"]}
  },
  "status": {
    "participants": [{"participant":"alice","stream":"…","video":true}],
    "composite_members": 2
  }
}
```

- `participants`・`egress` は空なら、`composite`・`recording`・`captions` は無ければ応答から省く。
- `artifacts.participant_streams`: このメディアパイプラインが作った参加者ごとの stream（制御状態から。どのノードでも同じ）。
- `artifacts.composite`: `composite` があるときだけ。合成の stream・パイプラインの ID と、`egress` の順の出力の ID。
- `status`: **応答したノードで動いている様子**。部屋を置いたノード以外、また止めているときは `null`。
  `composite_members` は今の合成の図の入力の数（音声だけの参加者も数える）。図が無ければ `null`。
- 送り出しの秘密は出さない（`credential_ref` の名前だけ）。

### 3.3 API

| メソッド・パス | scope | 内容 |
|---|---|---|
| `POST /api/v1/media-pipelines` | `stream:write` | 作る。201、`Location`・`ETag` |
| `GET /api/v1/media-pipelines` | `stream:read` | 一覧（ID の昇順）。`room`・`limit`（1〜1000、既定 100）・`cursor`。応答は `{items, next_cursor}` |
| `GET /api/v1/media-pipelines/{id}` | `stream:read` | 定義・成果物・様子。`ETag` |
| `PATCH /api/v1/media-pipelines/{id}` | `stream:write` | 書いたフィールドだけ置き換える。`If-Match` 必須 |
| `DELETE /api/v1/media-pipelines/{id}` | `stream:write` | 消す。204 |

作成の検査の順と誤り:

1. 本文の形（400 `INVALID_REQUEST`、415）
2. `room` がテナントの部屋か（400 `INVALID_CONFIG`）
3. 中身の検査（400 `INVALID_CONFIG`。3.1 の範囲と依存）
4. 名前の重複（409 `CONFLICT`）
5. 部屋あたり 8 つまで（403 `QUOTA_EXCEEDED`）

`PATCH` の順: `If-Match` が無い（428）→ 本文の形（400・415）→ 無い（404）→ 世代が違う（412）→ 作成と同じ中身の検査（400）→ 名前を変えて重複（409）。
`room` は変えられない（知らないフィールドとして 400）。`composite`・`recording`・`captions` は `null` を書くと外れる。

`POST` は `Idempotency-Key` を受ける（[15-api-and-sdks.md](15-api-and-sdks.md)）。

## 4. 片付け

| 操作 | 合成のパイプライン・送り出しの出力 | 参加者ごとの stream・合成の stream・録画 |
|---|---|---|
| `DELETE /media-pipelines/{id}` | 消す（PaaS の出力は先方の資源も片付ける） | 残す |
| `PATCH` で `desired_state: stopped` | 消す | 残す |
| `PATCH` で `composite` を外す、`egress` を変える | 前の定義の分を消す（動かすなら次の 1 秒で係が作り直す） | 残す |
| `PATCH` で `participants` を減らす | — | 外れた参加者の stream も残す |
| `DELETE /rooms/{id}` | 部屋のメディアパイプラインを消し、その子も消す | 残す |

- 書き出した stream と録画は成果物として**残る**。要らなければ `DELETE /streams/{id}`・`DELETE /recordings/{id}` で消す。
- 止めると、係は参加者の書き出しを止める（stream の送り手が去り、録画が確定する）。
- `DELETE /rooms/{id}` で消えたメディアパイプラインには `media_pipeline.deleted` は流れない（`room.deleted` だけ）。
- 参加トークンの発行で自動に作られた部屋（`auto: true`）は、空になって `empty_timeout_secs`（既定 60 秒）たつと消え、そのメディアパイプラインも消える。
  このときは合成のパイプラインは止まるが、合成のパイプラインと送り出しの出力の資源は残る（下の「注意」）。メディアパイプラインを使う部屋は
  `POST /rooms` で明示して作る。

## 5. イベント

`GET /api/v1/events`（SSE。[15-api-and-sdks.md](15-api-and-sdks.md)）:

| type | `data` | いつ |
|---|---|---|
| `media_pipeline.updated` | `{room, desired_state}` | 作成・更新 |
| `media_pipeline.deleted` | `{}` | 削除 |

子の資源はそれぞれの出来事を流す（`stream.*`・`pipeline.updated`・`output.*`・録画の出来事）。

## 6. 例

前提: `$API=http://127.0.0.1:8080/api/v1`、`$TOKEN` は `stream:write`・`stream:read`・`subscribe`・`recording:read` を持つトークン（例ではブートストラップの管理トークン）。
合成には codec worker 付きのノード、送り出しを同じ機械に向けるなら `--egress-allow 127.0.0.0/8` が要る。

```sh
api() { m=$1; p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$API$p" "$@"; }

# 部屋を作る（明示して作る。auto の部屋は空になると消える）
api POST /rooms -d '{"id":"meet","settings":{"max_participants":8,"video_codec":"vp8"}}'

# メディアパイプライン: 参加者ごとの stream、2 人を並べた AV1 の合成、両方の録画、合成を RTMP へ
api POST /media-pipelines -d '{
  "name": "mtg", "room": "meet",
  "composite": {"layout":"grid","width":640,"height":360,"fps":30,"video_codec":"av1","bitrate_kbps":800},
  "recording": {"participants":true,"composite":true},
  "egress": [{"protocol":"rtmp_push","url":"rtmp://live.example.com/app/STREAMKEY"}]
}' | tee mp.json
MP=$(jq -r .id mp.json); CSID=$(jq -r .artifacts.composite.stream mp.json)

# 参加者のトークン（client_id が参加者の鍵になる）
api POST /rooms/meet/tokens -d '{"role":"sendrecv","client_id":"alice"}'

# 参加している間: 成果物と様子
api GET /media-pipelines/$MP | jq '.artifacts, .status'

# 合成の stream を HLS で見る
api POST /streams/$CSID/playback-url | jq -r .playback.hls

# 去った後: 合成の録画を 1 本の MP4 に
RID=$(api GET "/recordings?stream=$CSID" | jq -r '.items[0].id')
curl -s -H "Authorization: Bearer $TOKEN" -o composite.mp4 "$API/recordings/$RID/export.mp4"

# 止める（If-Match に今の世代）
G=$(api GET /media-pipelines/$MP -D - -o /dev/null | sed -n 's/^[Ee][Tt]ag: *//p' | tr -d '\r')
api PATCH /media-pipelines/$MP -H "If-Match: $G" -d '{"desired_state":"stopped"}'

# 消す（合成のパイプライン・送り出しは消え、stream・録画は残る）
api DELETE /media-pipelines/$MP
```

その他の形:

```jsonc
// 録画だけ（合成なし）。composite: false を明示する
{"name":"rec","room":"meet","recording":{"participants":true,"composite":false,"retention_days":30}}

// 特定の参加者だけ書き出して文字起こし
{"name":"cap","room":"meet","participants":["teacher"],
 "captions":{"asr_provider":"whisper_cpp","asr_endpoint":"http://asr.internal:8080","language":"ja","targets":["en"],
             "translate_provider":"deepl","translate_endpoint":"https://api.deepl.com","translate_credential_ref":"secret:deepl"}}

// 話者を大きく、H.264 の合成を PaaS へ（GPU が要る）
{"name":"show","room":"meet",
 "composite":{"layout":"speaker","video_codec":"h264","bitrate_kbps":4000},
 "egress":[{"protocol":"paas","provider":"cloudflare_stream","credential_ref":"secret:cf-token"}]}
```

`tools/interop/node_media_pipelines.sh` は、Chrome 2 人の部屋で、参加者ごとの stream が再符号化なしで HLS で復号できること、
AV1 640×360 の合成、参加者ごと・合成の録画（3 本）と MP4 への書き出し、RTMP の送り出し、削除で合成のパイプラインが消え stream が残ることを確かめる。

## 7. グリッドでの見積もり

メディアパイプラインそのものはグリッドの仕事ではない。次のように数える（`crates/senca-control/src/workloads.rs`）。

- **参加者の書き出し**: 部屋と同じノードで動くので、部屋の仕事の見積もりに足す。動いているメディアパイプライン 1 つにつき、
  部屋の `max_participants`（既定 50）を m として CPU `20 × m` ミリコア、メモリ `8 × m` MB。
- **合成**: 別の仕事（パイプライン）として、図から見積もる。

| 図の節点 | CPU（ミリコア） | メモリ（MB） | GPU |
|---|---|---|---|
| 土台 | 200 | 128 | — |
| 復号（参加者 1 人につき映像・音声の 2 つ） | 400（H.264・H.265 は 100） | 128 | H.264・H.265 は復号 1 |
| `compose` | 300 | 64 | — |
| `audio_mix` | 50 | — | — |
| 映像の符号化（AV1・VP8・VP9） | 1500 | 128 | — |
| 映像の符号化（H.264・H.265） | 100 | 128 | 符号化 1 |
| 音声の符号化（Opus） | 50 | 128 | — |

ネットワークは上り・下りとも 10 Mbit/s。例: VP8 の部屋で 2 人を AV1 で合成すると、CPU 200 + 400 × 4 + 300 + 50 + 1500 + 50 = 3700 ミリコア、
メモリ 128 + 128 × 4 + 64 + 128 × 2 = 960 MB。

## 8. 注意

- メディアパイプラインの係は、部屋を置いたノードの参加者だけを見る。部屋は 1 つのノードに置かれる（カスケードは無効）。
- `status` は部屋を置いたノードでだけ値を持つ。ロードバランサーの後ろでは別のノードが答えて `null` になることがある。
- 自動に作られた部屋が空で消えると、メディアパイプラインは消えるが、合成のパイプライン（止まる）と送り出しの出力の資源が残る。
  `GET /pipelines`・`GET /outputs` で探して消す。
- 録画の置き場所はノードのデータディレクトリ。ADR-0016 に書いた S3 互換の置き場所、WHIP での送り出しは、まだノードに無い。
