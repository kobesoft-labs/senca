# 11. メディアパイプライン

[English](../en/11-media-pipelines.md)

メディアパイプラインは、部屋（[10. リアルタイムの部屋](10-rooms.md)）の参加者の映像と音声を SENCA のストリームに書き出す機能です。
録画、合成（composite）、外部への配信（egress）、文字起こしを、1 つのリソースでまとめて指定できます。
AWS の Chime SDK Media Pipelines に相当します。

- 設計の背景: [ADR-0016](../../dev/adr/0016-media-pipelines.md)
- API: `/api/v1/media-pipelines`

## 1. AWS との対応

| AWS（Chime SDK Media Pipelines） | SENCA |
|---|---|
| Media stream pipeline（Kinesis Video Streams へ送る） | 参加者ごとのストリーム（`artifacts.participant_streams`）。作成後は通常のストリームと同じく、HLS・録画・出力・パイプラインで使えます |
| Media capture pipeline（会議の録画） | `recording`。参加者ごとのストリームと合成のストリームを自動で録画します |
| Media concatenation pipeline | 通常の録画の API。`GET /recordings/{id}/export.mp4` で 1 本の MP4 にまとめます |
| Media live connector pipeline（会議を RTMP へ送る） | `egress`。合成のストリームを外部へ配信します（RTMP・SRT・PaaS）。`composite` が必要です |
| Media insights pipeline（文字起こし） | `captions`。参加者ごとのストリームに字幕と翻訳を付けます（[8. 字幕](08-captions.md)） |

## 2. しくみ

- メディアパイプラインの定義は制御状態に保存されるリソースです。クラスターのどのノードの API でも作成・変更できます。
- **実際の処理は、部屋が配置されているノードで行います**（配置先は `GET /rooms/{id}` の `placement_status.node`）。
  そのノードが **1 秒ごと**に部屋の参加者を確認し、ストリーム・合成・外部への配信を必要な状態にそろえます。
  部屋が別のノードへ移ると、移った先のノードが処理を引き継ぎます（[9. クラスターとグリッド](09-cluster-grid.md)）。
- 処理するのは `desired_state: running` のメディアパイプラインだけです。
- メディアパイプラインが作るリソース（ストリーム、合成のパイプライン、外部への配信の出力）の ID は、メディアパイプラインの ID から決まります。
  何度作り直しても同じ ID になります。これらの ID は応答の `artifacts` に入ります。
  合成のパイプラインと出力は、`GET /pipelines/{id}`・`GET /outputs/{id}` でも参照できます。

### 2.1 参加者ごとのストリーム（再エンコードなし）

- 参加者 1 人につきストリームを 1 本作ります。参加者は `client_id` で識別します（`client_id` が無ければ `connection_id`）。
- `participants` が空なら、全員を書き出します。`client_id` を列挙すると、その参加者だけを書き出します（`client_id` の無い参加者は対象外です）。
- 参加者が送信している映像の**最も品質の高いレイヤー**（simulcast の rid は低い順に並ぶため、最後の rid）と音声を、
  **再エンコードせずに**そのままストリームへ送ります。映像は最初のキーフレームから書き出します。
- 書き出している間、サーバーは参加者に **2 秒ごと**にキーフレームを要求します（途中から視聴する HLS や録画がすぐ始められるようにするためです）。
  書き出しが追いつかずにデータを取りこぼしたときもキーフレームを要求し、次のキーフレームから再開します。
- 映像も音声も送信していない参加者は書き出しません。映像トラックと音声トラックは、それぞれ最初の 1 本だけを使います。
- 送信するトラックやレイヤーが変わると、その参加者の書き出しを作り直します。参加者が退出すると書き出しを止めます（ストリームは残ります）。
- カスケード（[10. リアルタイムの部屋](10-rooms.md)）の部屋では、別のノード（edge）に接続している参加者も書き出します。
  トラックは、参加者のいるノードからノード間の中継で受け取ります。
- E2EE の部屋から書き出した stream には `metadata.e2ee: "true"` が付きます。中身は暗号化されたままなので、静止画・字幕・
  パイプラインの入力などには使えません（422。[10.11](10-rooms.md#1011-e2eeパススルー)）。

作成されるストリームの属性は次のとおりです。

| 項目 | 値 |
|---|---|
| 名前 | `{name}-{参加者}-{ストリーム ID の先頭 6 文字}`（英数字と `-_.` 以外は `-` に置き換え、前半は 56 文字まで） |
| `metadata` | `media_pipeline`（メディアパイプラインの ID）、`room`、`participant` |
| `codec_profile` | 部屋の `video_codec` が `h264`・`h265` なら `any`、それ以外（既定は `vp8`）なら `royalty_free` |
| `hls.container` | `fmp4` |
| 配信者 | 部屋からの書き出しだけです。publish 用の stream key では配信できません（使えない鍵のハッシュが設定されています） |

`export_participants: false` にしても、合成の入力として参加者ごとのストリームは作られます。
ただし、そのストリームを録画（`recording.participants`）や文字起こし（`captions`）の対象にはできなくなります。

### 2.2 合成（composite）

合成は、参加者の映像を 1 つの画面に並べ、1 本のストリームにまとめる機能です。
映像を送信している参加者を並べ、音声はすべての参加者（映像の無い参加者を含む）を 0 dB でミックスします。
内部では、このためのパイプライン（[7. パイプライン](07-pipelines.md)）が作られます。

- 参加者のストリームにデータが流れ始めてから、その参加者を合成に加えます（入力を待つ間に合成が止まらないようにするためです）。
  参加者が変わると、合成の構成を更新します。
- 映像を送信している参加者がいないときは、合成のパイプラインは `stopped` になります。
- 合成のパイプラインは部屋の配置先のノードに固定され（`placement.node`）、優先度は部屋と同じです。
  部屋が移ると、元のノードが合成を止め、移った先のノードが合成を配置し直します。
- 合成のストリームの名前は `{name}-composite-{ID の先頭 6 文字}`、パイプラインの名前は `{name}-compose-{ID の先頭 6 文字}` です。
- 合成のストリームの `codec_profile` は、合成の `video_codec` と部屋の `video_codec` がどちらも H.264・H.265 でなければ `royalty_free`、
  どちらかが H.264・H.265 なら `any` です。

レイアウト:

| `layout` | 並べ方 |
|---|---|
| `grid` | 映像を送信している人数を n とすると、列数 = ⌈√n⌉、行数 = ⌈n / 列数⌉ の格子に並べます。並び順は参加者の識別子（`client_id` など）の順です |
| `speaker` | 今話している人（主な話者）を上部に大きく（高さの 3/4）表示し、残りの参加者を下部の帯（高さの 1/4）に等分して並べます。映像が 1 人だけなら `grid` と同じです |

`speaker` の話者の選び方:

- 部屋の話者の検出（音声の大きさ。スポットライトと同じもの）を使い、合成に入っている映像の参加者の中から 1 人を選びます。
- 話している人がいなければ、直前の話者のままです（最初は識別子の順で最初の参加者）。
- 表示が頻繁に切り替わらないよう、今の話者が話している間と、切り替えてから 2 秒の間は切り替えません。0.2 秒続いた声を「話している」とみなし、
  1.5 秒声が途切れると「話していない」とみなします。
- 切り替えは合成のシーンの切り替えで行い（合成のパイプラインは作り直しません）、切り替えた画面はキーフレームから始まります。
- 今の話者は `status.active_speaker` で確認できます。

エンコード:

| 項目 | 値 |
|---|---|
| 映像 | `composite.video_codec`、`composite.bitrate_kbps`、GOP = `fps × 2`（2 秒ごとにキーフレーム） |
| 音声 | Opus 96 kbps |
| クロック | `realtime` |

合成では、codec worker がデコード・合成・エンコードを行います。ノードは codec worker を有効にして起動してください
（`--codec-workers`・`--codec-lib-dir`。[7. パイプライン](07-pipelines.md)）。
H.264・H.265 のエンコードには GPU が必要です。GPU の有無は作成時には検査しません。GPU が無い場合は合成のパイプラインの配置や検証が失敗し、
そのパイプラインの `placement_status` に理由が表示されます。

### 2.3 録画（capture）と結合（concatenation）

- `recording.participants: true` にすると参加者ごとのストリームに、`recording.composite: true` にすると合成のストリームに、
  自動録画の設定（`recording.enabled: true`、`retention_days`）を適用します。
  ストリームにデータが流れると録画が始まり、配信者（部屋からの書き出し）が切断すると録画が確定します（[5. 録画](05-recording.md)）。
- 録画は通常の録画リソースです。`GET /recordings?stream={ストリーム ID}` で探し、`GET /recordings/{id}/export.mp4` で 1 本の MP4 に書き出せます。
  - セグメントをつなぎ、先頭に `moov` を置いた MP4 を作ります。再エンコードはしません。途中の不連続（入力の切れ目）はタイムスタンプをずらしてつなぎます。
  - 録画中は 409 `CONFLICT` を返します。4 GiB を超える録画は 413 `PAYLOAD_TOO_LARGE` です。必要な scope は `recording:read` です。
- 録画の保存先は、ノードのデータディレクトリ（`--data-dir`）です。

### 2.4 外部への配信（live connector）

- `egress` の各要素は、合成のストリームの出力になります（出力リソースと同じ形式で、同じ検査を行います。[6. 入力と出力](06-inputs-outputs.md)）。
- 使える `protocol` は `rtmp_push`（`rtmp://`・`rtmps://`）、`srt_push`（`srt://`）、`paas`（`provider` は `cloudflare_stream`・`aws_ivs`・`aws_medialive`）です。
- URL に資格情報を含めることはできません（`user@host` の形は 400）。資格情報は `credential_ref: "secret:{名前}"` で秘密値を参照します。
- 接続先の IP アドレスは接続時に検査します。ループバックやプライベートアドレスなどへは、管理者が `--egress-allow CIDR` で許可した場合だけ配信できます
  （[12. セキュリティ](12-security.md)）。

### 2.5 文字起こし（insights）

- `captions` は、ストリームの `captions` と同じ形式（`CaptionSettings`。[8. 字幕](08-captions.md)）です。参加者ごとのストリームそれぞれに設定されます。
- 合成のストリームには設定されません。`export_participants: true` が必要です。

## 3. リソース

### 3.1 フィールド

| フィールド | 型 | 既定 | 意味 |
|---|---|---|---|
| `name` | string | （必須） | 1〜40 文字の英数字と `-_.`。`.` で始めることはできません。テナント内で一意です（作成するストリームの名前に使うため、ストリーム名の上限 64 文字より短くしています） |
| `room` | string | （必須） | 部屋の ID。テナントの部屋でなければ 400 です。作成後は変更できません |
| `participants` | string[] | `[]`（全員） | 書き出す参加者の `client_id`。100 人まで、各 1〜255 バイト |
| `export_participants` | bool | `true` | 参加者ごとのストリームを成果物として扱います。`recording.participants`・`captions` を使うときに必要です |
| `composite` | object | なし | 合成の設定（下の表） |
| `recording` | object | なし | 録画の設定（下の表） |
| `egress` | object[] | `[]` | 外部への配信。8 個まで。`composite` が必要です |
| `captions` | object | なし | 文字起こしと翻訳（`CaptionSettings`） |
| `desired_state` | string | `running` | `running` または `stopped` |

`composite`:

| フィールド | 既定 | 範囲 |
|---|---|---|
| `layout` | `grid` | `grid`・`speaker` |
| `width` | 1280 | 64〜3840 の偶数 |
| `height` | 720 | 64〜2160 の偶数 |
| `fps` | 30 | 1〜60 |
| `video_codec` | `av1` | `av1`・`vp9`・`vp8`・`h264`・`h265`（H.264・H.265 には GPU が必要） |
| `bitrate_kbps` | 2500 | 100〜50000 |

`recording`:

| フィールド | 既定 | 意味 |
|---|---|---|
| `participants` | `true` | 参加者ごとのストリームを録画します。`export_participants: true` が必要です |
| `composite` | `true` | 合成のストリームを録画します。`composite` が必要です |
| `retention_days` | なし | 1〜36500 |

`recording` の `participants`・`composite` は、**省略すると true** になります。
`composite` を設定していないメディアパイプラインで `recording` を指定するときは、`"composite": false` を明示してください（明示しないと 400）。

`egress[]`:

| フィールド | 意味 |
|---|---|
| `protocol` | `rtmp_push`・`srt_push`・`paas` |
| `url` | `paas` 以外で必須。2048 文字まで。空白・制御文字・資格情報を含めることはできません |
| `credential_ref` | `secret:{名前}`。テナントに該当する秘密値が無ければ 400 です。`paas` では必須です（API トークン） |
| `provider` | `paas` で必須 |
| `options` | 16 個まで。キーは 1〜64 文字、値は 256 文字まで |

未知のフィールドがあると 400（`INVALID_REQUEST`）です。`Content-Type` が `application/json` でなければ 415 です。

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
    "composite_members": 2,
    "active_speaker": "alice"
  },
  "managed": false
}
```

- `participants`・`egress` は空のとき、`composite`・`recording`・`captions` は設定が無いとき、応答から省かれます。
- `artifacts.participant_streams`: このメディアパイプラインが作った参加者ごとのストリームです（制御状態から取得するため、どのノードに問い合わせても同じです）。
- `artifacts.composite`: `composite` を設定したときだけ含まれます。合成のストリームとパイプラインの ID、および `egress` の順に並んだ出力の ID です。
- `status`: **応答したノードでの実行状況**です。部屋の配置先以外のノードが応答したときや、停止しているときは `null` です。
  `composite_members` は、現在の合成の入力数です（音声だけの参加者も数えます）。合成の構成が無ければ `null` です。
  `active_speaker` は、`speaker` の並びで今大きく表示している参加者の識別子です（`grid` のときと、決まる前は `null`）。
- `managed`: 部屋の設定 `recording` から自動で作られたものなら `true` です（ID は `room-rec.{部屋の ID}`）。API から変更・削除はできません（409）。
  部屋の設定を変えると合わせて更新され、`none` にすると削除されます（[10. リアルタイムの部屋](10-rooms.md)）。
- 外部への配信の秘密値は応答に含めません（`credential_ref` の名前だけを返します）。

### 3.3 API

| メソッドとパス | scope | 内容 |
|---|---|---|
| `POST /api/v1/media-pipelines` | `stream:write` | 作成します。201、`Location`・`ETag` を返します |
| `GET /api/v1/media-pipelines` | `stream:read` | 一覧（ID の昇順）。クエリは `room`・`limit`（1〜1000、既定 100）・`cursor`。応答は `{items, next_cursor}` |
| `GET /api/v1/media-pipelines/{id}` | `stream:read` | 定義・成果物・実行状況を返します。`ETag` 付き |
| `PATCH /api/v1/media-pipelines/{id}` | `stream:write` | 指定したフィールドだけを置き換えます。`If-Match` が必須です |
| `DELETE /api/v1/media-pipelines/{id}` | `stream:write` | 削除します。204 |

作成時の検査の順序と、返すエラー:

1. 本文の形式（400 `INVALID_REQUEST`、415）
2. `room` がテナントの部屋か（400 `INVALID_CONFIG`）
3. 内容の検査（400 `INVALID_CONFIG`。3.1 の範囲と依存関係）
4. 名前の重複（409 `CONFLICT`）
5. 1 つの部屋あたり 8 個までの上限（403 `QUOTA_EXCEEDED`）

`PATCH` の検査の順序: `If-Match` が無い（428）→ 本文の形式（400・415）→ 存在しない（404）→ 世代が一致しない（412）→
作成時と同じ内容の検査（400）→ 名前を変更して重複した（409）。
`room` は変更できません（未知のフィールドとして 400 になります）。`composite`・`recording`・`captions` は `null` を指定すると設定が外れます。

`POST` では `Idempotency-Key` を使えます（[15. 管理 API と SDK](15-api-and-sdks.md)）。

## 4. 後片付け

| 操作 | 合成のパイプライン・外部への配信の出力 | 参加者ごとのストリーム・合成のストリーム・録画 |
|---|---|---|
| `DELETE /media-pipelines/{id}` | 削除します（PaaS の出力は、配信先のリソースも片付けます） | 残します |
| `PATCH` で `desired_state: stopped` | 削除します | 残します |
| `PATCH` で `composite` を外す、`egress` を変える | 前の定義の分を削除します（実行中なら、次の 1 秒の確認で作り直します） | 残します |
| `PATCH` で `participants` を減らす | — | 対象から外れた参加者のストリームも残します |
| `DELETE /rooms/{id}` | 部屋のメディアパイプラインを削除し、それが作ったリソースも削除します | 残します |

- 書き出したストリームと録画は成果物として**残ります**。不要なら `DELETE /streams/{id}`・`DELETE /recordings/{id}` で削除してください。
- 停止すると、参加者の書き出しも止まります（ストリームの配信者が切断した扱いになり、録画が確定します）。
- `DELETE /rooms/{id}` で削除されたメディアパイプラインについては、`media_pipeline.deleted` イベントは送られません（`room.deleted` だけです）。
- 参加トークンの発行時に自動で作られた部屋（`auto: true`）は、参加者がいなくなってから `empty_timeout_secs`（既定 60 秒）が過ぎると削除され、
  その部屋のメディアパイプラインも削除されます。このとき合成のパイプラインは停止しますが、合成のパイプラインと外部への配信の出力のリソースは残ります
  （§8）。メディアパイプラインを使う部屋は、`POST /rooms` で明示的に作成してください。

## 5. イベント

`GET /api/v1/events`（SSE。[15. 管理 API と SDK](15-api-and-sdks.md)）に、次のイベントが送られます。

| type | `data` | 送られるとき |
|---|---|---|
| `media_pipeline.updated` | `{room, desired_state}` | 作成・更新 |
| `media_pipeline.deleted` | `{}` | 削除 |

メディアパイプラインが作ったリソースは、それぞれのイベント（`stream.*`・`pipeline.updated`・`output.*`・録画のイベント）を送ります。

## 6. 例

前提: `$API=http://127.0.0.1:8080/api/v1`、`$TOKEN` は `stream:write`・`stream:read`・`subscribe`・`recording:read` を持つトークンです
（この例では最初の管理者トークンを使います）。
合成には codec worker を有効にしたノードが必要です。外部への配信先を同じマシンにする場合は `--egress-allow 127.0.0.0/8` も必要です。

```sh
api() { m=$1; p=$2; shift 2; curl -s -X "$m" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' "$API$p" "$@"; }

# 部屋を作る（明示的に作成する。auto の部屋は空になると削除される）
api POST /rooms -d '{"id":"meet","settings":{"max_participants":8,"video_codec":"vp8"}}'

# メディアパイプライン: 参加者ごとのストリーム、2 人を並べた AV1 の合成、両方の録画、合成を RTMP へ配信
api POST /media-pipelines -d '{
  "name": "mtg", "room": "meet",
  "composite": {"layout":"grid","width":640,"height":360,"fps":30,"video_codec":"av1","bitrate_kbps":800},
  "recording": {"participants":true,"composite":true},
  "egress": [{"protocol":"rtmp_push","url":"rtmp://live.example.com/app/STREAMKEY"}]
}' | tee mp.json
MP=$(jq -r .id mp.json); CSID=$(jq -r .artifacts.composite.stream mp.json)

# 参加者のトークン（client_id で参加者を識別する）
api POST /rooms/meet/tokens -d '{"role":"sendrecv","client_id":"alice"}'

# 参加中: 成果物と実行状況
api GET /media-pipelines/$MP | jq '.artifacts, .status'

# 合成のストリームを HLS で視聴する
api POST /streams/$CSID/playback-url | jq -r .playback.hls

# 退出後: 合成の録画を 1 本の MP4 に書き出す
RID=$(api GET "/recordings?stream=$CSID" | jq -r '.items[0].id')
curl -s -H "Authorization: Bearer $TOKEN" -o composite.mp4 "$API/recordings/$RID/export.mp4"

# 停止する（If-Match に現在の世代を指定）
G=$(api GET /media-pipelines/$MP -D - -o /dev/null | sed -n 's/^[Ee][Tt]ag: *//p' | tr -d '\r')
api PATCH /media-pipelines/$MP -H "If-Match: $G" -d '{"desired_state":"stopped"}'

# 削除する（合成のパイプラインと外部への配信は削除され、ストリームと録画は残る）
api DELETE /media-pipelines/$MP
```

そのほかの設定例:

```jsonc
// 録画だけ（合成なし）。composite: false を明示する
{"name":"rec","room":"meet","recording":{"participants":true,"composite":false,"retention_days":30}}

// 特定の参加者だけを書き出して文字起こしする
{"name":"cap","room":"meet","participants":["teacher"],
 "captions":{"asr_provider":"whisper_cpp","asr_endpoint":"http://asr.internal:8080","language":"ja","targets":["en"],
             "translate_provider":"deepl","translate_endpoint":"https://api.deepl.com","translate_credential_ref":"secret:deepl"}}

// 話者を大きく表示し、H.264 の合成を PaaS へ配信する（GPU が必要）
{"name":"show","room":"meet",
 "composite":{"layout":"speaker","video_codec":"h264","bitrate_kbps":4000},
 "egress":[{"protocol":"paas","provider":"cloudflare_stream","credential_ref":"secret:cf-token"}]}
```

## 7. グリッドでのリソース見積もり

メディアパイプライン自体は、グリッドのワークロードではありません。グリッドは次のように見積もります。

- **参加者の書き出し**: 部屋と同じノードで動くため、部屋のワークロードの見積もりに加算します。実行中のメディアパイプライン 1 つにつき、
  部屋の `max_participants`（既定 50）を m として、CPU `20 × m` ミリコア、メモリ `8 × m` MB です。
- **合成**: 別のワークロード（パイプライン）として、グラフの構成から見積もります。

| グラフの処理ノード | CPU（ミリコア） | メモリ（MB） | GPU |
|---|---|---|---|
| 基本 | 200 | 128 | — |
| デコード（参加者 1 人につき映像・音声の 2 つ） | 400（H.264・H.265 は 100） | 128 | H.264・H.265 はデコード 1 |
| `compose` | 300 | 64 | — |
| `audio_mix` | 50 | — | — |
| 映像のエンコード（AV1・VP8・VP9） | 1500 | 128 | — |
| 映像のエンコード（H.264・H.265） | 100 | 128 | エンコード 1 |
| 音声のエンコード（Opus） | 50 | 128 | — |

ネットワークは送信・受信とも 10 Mbit/s です。例: VP8 の部屋で 2 人を AV1 で合成すると、CPU は 200 + 400 × 4 + 300 + 50 + 1500 + 50 = 3700 ミリコア、
メモリは 128 + 128 × 4 + 64 + 128 × 2 = 960 MB です。

## 8. 注意点

- メディアパイプラインは、部屋の配置先のノード（home）で動きます。カスケードの部屋で別のノード（edge）に接続している参加者は、home がその edge から受け取って書き出します。
- `status` は、部屋の配置先のノードが応答したときだけ値を持ちます。ロードバランサーの背後では、別のノードが応答して `null` になることがあります。
- 自動で作られた部屋が空になって削除されると、メディアパイプラインは削除されますが、合成のパイプライン（停止状態）と外部への配信の出力のリソースは残ります。
  `GET /pipelines`・`GET /outputs` で探して削除してください。
- 録画の保存先はノードのデータディレクトリです。S3 互換ストレージへの保存と、WHIP での外部への配信には対応していません。

## 9. 管理画面で使う

管理画面（`/console/`）の「メディアパイプライン」では、やりたいことから選んで作成できます。

- **作成**: 「ルームを録画する」「まとめて配信する」「参加者をストリームにする」「文字起こしと字幕」から選ぶと、その用途に必要な設定だけが表示されます。
  右側には、作られるもの（ストリーム・録画・配信先）とその行き先が入力に合わせて表示され、足りない設定があれば作成の前に知らせます。
- **配信先の鍵**: YouTube・Twitch・RTMP・SRT・クラウドのサービスから選びます。ストリームキーはその場で入れると秘密値として保存され、`credential_ref` で参照されます（URL には入りません）。既存の秘密値を選ぶこともできます。
- **一覧と詳細**: 状態（書き出し中・参加者を待っています・準備中・停止）と、作られるものが表示されます。詳細では、書き出し中の参加者、合成している人数、作ったストリーム・録画・合成のパイプライン・配信先への出力へのリンク、
  合成や配信先で起きている問題（配置できない、接続できないなど）が表示されます。停止・削除の前には、残るものと消えるものを確認できます。
- ルームの設定 `recording` から作られたもの（`managed: true`）には「自動」と表示され、ここでは変更・停止・削除できません。
- ルームの詳細の「録画・配信」から開くと、そのルームが選ばれた状態で始まります。
