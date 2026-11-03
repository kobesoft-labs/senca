# 7. パイプライン

[English](../en/07-pipelines.md)

パイプラインは、stream を入力にして復号・板書の補正・拡大縮小・切り抜き・合成・音声のミックス・符号化を行い、結果を別の stream に入れる
処理のグラフ（有向非巡回グラフ）。ピクチャー・イン・ピクチャー、シーンの切り替え（スイッチャー）、トランスコードなどに使う。

- 設計の理由: [ADR-0004 コーデックと特許](../../dev/adr/0004-codecs-patents.md)、[ADR-0013 グリッド](../../dev/adr/0013-grid.md)（配置と縮退）
- 関係するページ: [4. 取り込みと再生](04-ingest-playback.md)、[9. クラスタとグリッド](09-cluster-grid.md)、[14. コーデックと特許](14-codecs-and-patents.md)、[15. API と SDK](15-api-and-sdks.md)
- 実装: `crates/senca-pipeline`（`model.rs` 定義、`validate.rs` 検証、`control.rs` 操作）、`crates/senca-node/src/pipelines.rs`・`api/pipelines.rs`、`crates/senca-control/src/workloads.rs`（縮退）
- 端から端までの試験: `tools/interop/node.sh` の場面 J

例の `API`・`TOKEN`・`H` は [4. 取り込みと再生](04-ingest-playback.md) と同じ。

## 7.1 しくみ

- グラフは codec worker（別プロセス、サンドボックス付き）の中で動く。生のフレームは worker の外に出ない。
  codec worker が無いノードではパイプラインは動かない（`GET /capabilities` の `pipelines` が `false`）。
- `source` は SENCA の stream の AU を読み、`sink` は結果の AU を出力先の stream に**送り手として**入れる。出力先の stream は
  普通の stream と同じく HLS・録画・出力・字幕に乗る。出力先の stream に別の送り手がつながっていると、パイプラインは出せない。
- パイプラインはグリッドの仕事（`pipeline:{id}`）で、入力の stream を受けているノードを好んで置かれる（[9. クラスタとグリッド](09-cluster-grid.md)）。
- worker が落ちる・失敗すると `observed_state: error` になり、1・2・4…32 秒の間を置いて作り直す。

## 7.2 API

| 操作 | 要求 | 権限 |
|---|---|---|
| 作る | `POST /pipelines` `{name, graph, desired_state?, placement?}` | `stream:write` |
| 一覧 | `GET /pipelines?limit&cursor` | `stream:read` |
| 取得 | `GET /pipelines/{id}` | `stream:read` |
| 変更 | `PATCH /pipelines/{id}` `{name?, graph?, desired_state?, placement?}`（`If-Match` 必須） | `stream:write` |
| 削除 | `DELETE /pipelines/{id}` | `stream:write` |
| 検証 | `POST /pipelines/{id}/validate`（本文に別のグラフを渡せる。省くと保存済みのグラフ） | `stream:read` |
| 操作 | `POST /pipelines/{id}/control` | `stream:write` |

- `name` は stream の名前と同じ規則で、テナント内で一意。テナントのパイプラインは 100 個まで。
- **`desired_state` の既定は `stopped`**。動かすには `"running"` を指定する。
- 作成・変更のときにグラフを検証する。`source`・`sink` の stream はテナントのもので、存在していなければならない。
- `graph` を変えるとパイプラインを止めて新しいグラフで起動し直す（出力は途切れる）。
- 応答: `observed_state`（`starting`・`running`・`error`・`stopped`）、`last_error`、`plan`（検証の見積もり）、`status`・`meters`
  （worker の状態とメーター。既定で約 10 Hz に更新）、`placement`・`placement_status`。

検証の失敗は problem+json で、`details.issues` に `[{code, node, message}]` が入る。装置が足りない（`CAPACITY_UNAVAILABLE` を含む）ときは
503 `CAPACITY_UNAVAILABLE`（`retryable: false`）、それ以外は 400 `INVALID_CONFIG`。

| `issues[].code` | 意味 |
|---|---|
| `MALFORMED` | JSON として読めない、知らない項目、`version` が 1 でない、16 MiB を超える |
| `INVALID_PARAM` | 値が範囲外・形が不正（id の形を含む） |
| `DUPLICATE_ID` | 同じ `id` の部品が 2 つ |
| `UNKNOWN_REFERENCE` | 知らない部品・ポート・シーン・画像を参照 |
| `TYPE_MISMATCH` | ポートの型が合わない（符号化済みを合成に入れる、映像を Opus で符号化する、sink に生のフレームを入れるなど） |
| `CYCLE` | 循環がある |
| `UNCONSUMED_OUTPUT` | 終端（`sink`・`snapshot_on_change`）以外の出力がどこにも使われていない（`source` は宣言したトラックごと、`compose` は `program`） |
| `POLICY_REJECTED` | ロイヤリティフリー限定で H.264・H.265・AAC を使う（素通しも含む） |
| `UNSUPPORTED_CODEC` | AAC の復号・符号化、知らないコーデック |
| `CAPACITY_UNAVAILABLE` | H.264/H.265 のハードウェアが無い、解像度に対応する提供者が無い、ハードウェアの同時セッション数を超える |
| `LIMIT_EXCEEDED` | 部品の数・解像度・画像・シーン・入力の数の上限を超える |

`source` か `sink` の stream のどれかが `codec_profile: royalty_free` なら、グラフ全体をロイヤリティフリー限定で検証する。

## 7.3 グラフの形

```json
{"version": 1, "options": {...}, "nodes": [ {"type": "...", "id": "...", ...}, ... ]}
```

- `version` は 1。知らない項目は拒否する。
- `id` は英数字・`_`・`-` の 1〜64 文字。部品は 256 個まで。
- 入力の参照は `"部品"` か `"部品.ポート"`。`source` はポートを省けない（`cam.video`・`cam.audio`）。`compose` のポートは
  `program`（既定）と `preview`。他の部品のポートは `out` で、省ける。
- 解像度は各辺 2〜7680×4320、fps は 240 まで（合成のキャンバスは 120 まで）。

ポートの型:

| 型 | 出す部品 |
|---|---|
| 符号化済みの映像・音声（コーデック付き） | `source`（`video`・`audio`）、`encode` |
| 生の映像 | `decode`（映像）・`board`・`scale`・`crop`・`compose` |
| 生の音声（48 kHz） | `decode`（音声）・`audio_mix`（48 kHz ステレオ） |

### `options`

| 項目 | 既定 | 範囲 | 意味 |
|---|---|---|---|
| `clock` | `realtime` | `realtime`・`input` | `realtime`: 単調時計で出力の刻みを進める（ライブ）。`input`: 入力の時刻で進める（全入力がそろったら出す。ファイル・試験向け） |
| `overflow` | `drop` | `drop`・`block` | キューが一杯のとき。`drop`: 古いものを捨てる（映像は次のキーフレームまで）。`block`: 上流を待たせる（`block_timeout_ms` を超えたら捨てる） |
| `latency_ms` | 100 | 0〜10000 | 入力の揺らぎを吸収する遅延。合成・ミキサーはこの分遅れた時刻を描く |
| `missing_timeout_ms` | 2000 | 100〜60000 | 入力が来ないとき「切れた」とみなすまで |
| `block_timeout_ms` | 2000 | 10〜60000 | `block` で待つ上限 |
| `input_queue_aus` | 256 | 8〜100000 | 映像の AU の入力キューの長さ |
| `frame_queue` | 8 | 1〜256 | 生のフレームのキュー（部品の入力ごと） |
| `status_interval_ms` | 100 | 0 か 20〜60000 | 状態・メーターの通知の間隔（0 で出さない） |
| `commit_timeout_ms` | 10000 | 100〜600000 | 新しい世代への切り替えを待つ上限（ノードの API からは使わない） |

## 7.4 部品

### `source`（入力）

| 項目 | 必須 | 意味 |
|---|---|---|
| `stream` | はい | SENCA の stream の ID |
| `video`・`audio` | どちらか | 使うトラック。`{codec, track?, width?, height?, fps?, sample_rate?, channels?}` |

`codec` は `h264`・`h265`・`av1`・`vp8`・`vp9`・`opus`・`aac` など（入力が実際に送るコーデック）。`track` を省くと種類の最初のトラック。
`width`・`height`・`fps`・`sample_rate`（8000〜192000）・`channels`（1〜8）は検証と見積もりに使う。

### `decode`（復号）

`{input, max_width?, max_height?, threads?}`。符号化済みの入力を生のフレーム・音声にする。`max_width`・`max_height` は受け付ける解像度の上限
（既定 3840×2160）、`threads` はソフトウェアのコーデックのスレッド数（0 = 自動、64 まで）。AAC は復号しない（`UNSUPPORTED_CODEC`）。

### `board`（板書の補正）

`{input, params}`。ホワイトボード・黒板を正面から見た形に直し、見やすくする。

| `params` の項目 | 既定 | 意味 |
|---|---|---|
| `mode` | `whiteboard` | `whiteboard`・`blackboard` |
| `corners` | なし | 板の 4 隅（左上・右上・右下・左下、入力の画素）`[[x,y],…]` |
| `auto_detect` | `false` | 4 隅を自動で検出する（見つかるまで毎フレーム試す） |
| `redetect_frames` | 0 | 検出をやり直す間隔（フレーム、0 = やり直さない） |
| `output_width`・`output_height` | 1280・720 | 出力の大きさ（16 以上）。`corners` も `auto_detect` も無ければ入力の大きさのまま |
| `enhance` | `true` | 照明むら・反射の補正と強調 |
| `stroke_gain`・`chroma_gain` | なし | 0〜10 |
| `glare_suppression` | なし | |
| `invert_to_white` | `false` | 黒板を白地・暗い字にする |
| `clahe` | `false` | 輝度の CLAHE |

### `scale`・`crop`

- `scale`: `{input, width, height, filter?}`。`filter` は `nearest`・`bilinear`（既定）・`bicubic`・`lanczos3`・`area`。
- `crop`: `{input, x, y, width, height}`（入力の画素）。大きさは偶数に切り下げる。入力の大きさが分かっていれば、はみ出しは `INVALID_PARAM`。

### `compose`（合成）

| 項目 | 既定 | 意味 |
|---|---|---|
| `inputs` | （必須） | シーンから使う名前 → 上流の参照（生の映像）。32 個まで |
| `canvas` | （必須） | `{width, height, fps_num, fps_den?(1), background?("#000000")}`。幅・高さは偶数 |
| `images` | `{}` | 静止画（ロゴなど）。名前 → `{png_base64}` か `{jpeg_base64}`。32 枚まで、1 枚 8 MiB（base64）まで。検証で実際に復号する |
| `scenes` | （必須） | シーン名 → `{layers: [...]}`。1〜64 個 |
| `program` | （必須） | 出力するシーン |
| `preview` | なし | `preview` ポートに描くシーン |
| `transition` | `{kind: cut, duration_ms: 0}` | `take` の既定のトランジション。`kind` は `cut`・`fade`・`wipe_left`・`wipe_right`、`duration_ms` は 60000 まで |
| `missing` | `{hold_ms: 2000, slate_color: "#000000"}` | 入力が来ないとき。`hold_ms`（600000 まで）だけ最後の画を保ち、その後はスレート（`slate_image` は `images` の名前で、色より優先） |
| `threads` | 0 | 描画の並列度（0 = CPU の数、64 まで） |

名前（入力・画像・シーン）は英数字・`_`・`-` の 1〜64 文字。

レイヤー（`layers[]`）は `source`（`inputs` の名前）・`image`（`images` の名前）・`color`（`#RRGGBB`）の**どれか 1 つ**を持つ:

| 項目 | 既定 | 意味 |
|---|---|---|
| `x`・`y`・`width`・`height` | （必須） | キャンバス上の位置と大きさ |
| `crop` | 0 | `{left, top, right, bottom}`。各辺から割合で削る |
| `fit` | `contain` | `stretch`・`contain`・`cover` |
| `z` | 0 | 重なり（大きいほど上） |
| `opacity` | 1.0 | 0〜1 |
| `corner_radius` | 0 | 4096 まで |
| `border` | なし | `{width (512 まで), color}` |
| `visible` | `true` | |
| `filter` | `bicubic` | 拡大縮小の核 |

### `audio_mix`（音声のミックス）

| 項目 | 既定 | 意味 |
|---|---|---|
| `inputs` | （必須） | 名前 → `{from, gain_db?(0), mute?, solo?, pan?(0), delay_ms?(0)}`。`from` は生の音声。1〜32 個 |
| `master_gain_db` | 0 | -120〜24 |
| `limiter` | `true` | |

`gain_db` は -120〜24、`pan` は -1〜1、`delay_ms` は 0〜5000。出力は 48 kHz ステレオ。

### `encode`（符号化）

| 項目 | 既定 | 意味 |
|---|---|---|
| `input` | （必須） | 生の映像か生の音声 |
| `codec` | （必須） | 映像: `av1`・`vp8`・`vp9`・`h264`・`h265`。音声: `opus` |
| `bitrate_kbps` | （必須） | 1〜800000 |
| `gop` | 2 秒分 | キーフレームの間隔（フレーム、100000 まで） |
| `low_latency` | `true` | |
| `rate_control` | `cbr` | `cbr`・`vbr` |
| `width`・`height` | 入力の大きさ | 2 つそろえて書く。違う大きさのフレームは拡大縮小する |
| `frame_ms` | 20 | Opus の 1 パケットの長さ。2.5・5・10・20・40・60 |
| `threads` | 0 | 64 まで |

`h264`・`h265` は GPU（NVENC・oneVPL・AMF）が無いと `CAPACITY_UNAVAILABLE`。`av1` は SVT-AV1（またはハードウェア）、`vp8`・`vp9` は libvpx、
`opus` は libopus。AAC の符号化は無い（[14. コーデックと特許](14-codecs-and-patents.md)）。

### `snapshot_on_change`（板書の変化で静止画）

`{input, params}`。生の映像を解析し、内容が変わって落ち着いたら JPEG を保存する。保存先は**パイプラインごとの場所**
（`{data-dir}/pipeline-snapshots/{pipeline_id}/`）で、`sink` の無いグラフでも使える。部品ごとに `params.keep` 枚まで残し、超えたら古いものから消す。
パイプラインを消すと、静止画も消す（パイプラインを動かしていたノードの分）。

| 操作 | API | 権限 | 結果 |
|---|---|---|---|
| 一覧 | `GET /pipelines/{id}/snapshots` | `stream:read` | `{"items":[{id, taken_at, reason: "board_change", node, width, height, bytes, changed_fraction}]}`（古い順。`node` は部品の ID） |
| 1 枚の取得 | `GET /pipelines/{id}/snapshots/{snapshot_id}` | `stream:read` | JPEG |
| 削除 | `DELETE /pipelines/{id}/snapshots/{snapshot_id}` | `stream:write` | 204 |

静止画は、パイプラインを動かしているノードのディスクに置く。クラスターでは、そのノード（`placement_status.node_name`）の API で取得する。

| `params` の項目 | 既定 | 範囲 |
|---|---|---|
| `mode` | `whiteboard` | `whiteboard`・`blackboard` |
| `enabled` | `true` | 偽なら操作 `snapshot` で始める |
| `analysis_fps` | 2.0 | 0〜120（0 = 全フレーム） |
| `stable_frames` | 3 | 1〜10000 |
| `change_fraction` | 0.004 | 0 より大きく 1 より小さい |
| `jpeg_quality` | 85 | 1〜100 |
| `keep` | 1000 | 1〜10000（残す枚数） |

### `sink`（出力）

`{inputs, stream}`。`inputs` は符号化済みの映像 1 つ・音声 1 つまで（1〜2 個）で、並びの順にトラック番号 0、1 を付ける。
`source` のトラックをそのまま入れると素通し（再符号化しない）になる。`stream` は出力先の stream の ID。

出力先の stream の HLS の入れ物にコーデックが入るようにする（AV1・VP9 は `hls.container: fmp4`。[4.9](04-ingest-playback.md)）。

## 7.5 例: ピクチャー・イン・ピクチャー

`tools/interop/node.sh` の場面 J と同じ構成。板書のカメラ（`jb`）を全面に、講師（`jt`）を右下に小さく重ね、講師の音声を Opus にして
AV1 で出す。3 つの stream はどれも `codec_profile: royalty_free`・`hls.container: fmp4`。

```sh
for n in jt jb jout; do
  curl -s -X POST "$API/streams" "${H[@]}" -d '{"name":"'$n'","codec_profile":"royalty_free","hls":{"container":"fmp4"}}' > $n.json
done
id() { python3 -c "import json;print(json.load(open('$1.json'))['id'])"; }

cat > graph.json <<EOF
{"version":1,"options":{"clock":"realtime"},"nodes":[
 {"type":"source","id":"t","stream":"$(id jt)","video":{"codec":"vp9"},"audio":{"codec":"opus"}},
 {"type":"source","id":"b","stream":"$(id jb)","video":{"codec":"vp9"}},
 {"type":"decode","id":"dt","input":"t.video"},
 {"type":"decode","id":"db","input":"b.video"},
 {"type":"decode","id":"at","input":"t.audio"},
 {"type":"compose","id":"prod","inputs":{"board":"db","teacher":"dt"},
  "canvas":{"width":640,"height":360,"fps_num":30},
  "scenes":{
    "pinp":{"layers":[{"source":"board","x":0,"y":0,"width":640,"height":360},
                      {"source":"teacher","x":440,"y":250,"width":192,"height":108,"z":1}]},
    "teacher":{"layers":[{"source":"teacher","x":0,"y":0,"width":640,"height":360}]}},
  "program":"pinp","preview":"teacher","transition":{"kind":"fade","duration_ms":300}},
 {"type":"audio_mix","id":"mix","inputs":{"mic":{"from":"at","gain_db":0}}},
 {"type":"encode","id":"venc","input":"prod","codec":"av1","bitrate_kbps":800,"gop":30},
 {"type":"encode","id":"aenc","input":"mix","codec":"opus","bitrate_kbps":96},
 {"type":"sink","id":"out","inputs":["venc","aenc"],"stream":"$(id jout)"}]}
EOF

python3 -c "import json;print(json.dumps({'name':'pinp','graph':json.load(open('graph.json')),'desired_state':'running'}))" > body.json
curl -s -X POST "$API/pipelines" "${H[@]}" --data-binary @body.json
```

`jt`・`jb` に VP9（と Opus）で送ると、`jout` が配信中になる。`jout` の再生 URL で合成の結果を見る。

この構成には VP9 の復号（libvpx）・AV1 の符号化（SVT-AV1）・Opus の符号化（libopus）が要る。入れ方は [14. コーデックと特許](14-codecs-and-patents.md)。

## 7.6 オペレーターの操作（control）

`POST /pipelines/{id}/control` に操作の JSON（`"op"` で種類を選ぶ。知らない項目は 400）を送る。合成・ミキサーへの操作は次の出力のフレームの境目で反映する。

```sh
# プレビュー（teacher）をプログラムへ（既定のトランジション = fade 300 ms）。プレビューには元のプログラムが入る
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"take","node":"prod"}'
# トランジションを指定してシーンを切り替える
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"switch_program","node":"prod","scene":"pinp","transition":{"kind":"wipe_left","duration_ms":500}}'
# マイクのフェーダー
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"set_audio","node":"mix","input":"mic","gain_db":-6}'
```

| `op` | 項目 | 意味 |
|---|---|---|
| `switch_program` | `node`、`scene`、`transition?` | プログラムのシーンを切り替える（省くとカット） |
| `set_preview` | `node`、`scene` | プレビューのシーンを決める |
| `take` | `node`、`transition?` | プレビューをプログラムへ（既定は合成の `transition`）。プレビューには元のプログラムが入る |
| `update_layer` | `node`、`scene`、`layer`（番号）、`patch` | レイヤーを部分的に変える。`patch` は `x`・`y`・`width`・`height`・`z`・`opacity`・`visible`・`source` |
| `set_scene` | `node`、`scene`、`layers` | シーンを作る・置き換える |
| `set_audio` | `node`（`audio_mix`）、`input`、`gain_db?`・`mute?`・`solo?`・`pan?`・`delay_ms?` | 音声の入力を変える（書いた項目だけ） |
| `set_master` | `node`、`gain_db` | マスターの音量 |
| `snapshot` | `node`（`snapshot_on_change`）、`enabled` | 変化の検出を始める・止める |
| `request_keyframe` | `node?` | キーフレームを求める（省くとすべての映像の符号化） |

- 応答は 200 `{"ok": true, "reply": {...}}`。パイプラインが動いていなければ 409、操作の中身が誤り（知らない部品・シーンなど）は 400（`details.reason`）。
- `set_scene`・`update_layer` などの操作は動いている間だけ効き、保存しているグラフは変えない（起動し直すとグラフの内容に戻る）。
- `prepare`・`commit`・`rollback`（世代の操作）はこの API では使えない（400）。グラフは `PATCH` で変える。

## 7.7 縮退（degrade）

装置・ノードの資源が足りないとき、グリッドはパイプラインを止める代わりに、あらかじめ決めた**縮退の段**で一部の部品を外して動かせる。

```sh
curl -s -X PATCH "$API/pipelines/$PID" "${H[@]}" -H "If-Match: $ETAG" -d '{
  "placement": {"priority": "high", "degrade": [
    {"label": "no-board", "disable": ["b"]}
  ]}
}'
```

- `placement.degrade` はよい順の段の並び（8 段まで）。各段は `{label, disable}`。`label` は重複不可で、`full`（段 0 の名前）は使えない。
  `disable` は空にできず、グラフにある部品の ID だけ。
- 段は**積み重なる**: 段 2 では段 1 と段 2 の `disable` の両方を外す。
- 外し方（`prune_graph`）: 指定した部品を外し、入力がすべて外れた下流の部品も外す。入力の一部だけが外れる部品（複数入力の `sink`・`compose`・
  `audio_mix`）は、外れた入力を除いて残す。`compose` は外れた入力を使うレイヤーを各シーンから除く。
- 外した後のグラフも検証される。上流を残して下流だけ外すと、上流の出力が `UNCONSUMED_OUTPUT` になって動かないので、**上流（`source` など）から外す**。
  上の例は板書の `source`（`b`）を外し、`decode`（`db`）が連鎖して外れ、合成から板書のレイヤーが消える（講師だけの画になる）。
- 段が変わるとパイプラインを新しいグラフで起動し直す。結果は `placement_status` に出る（`state: degraded`、`level`、`level_label`）。
- 段ごとの必要な資源（CPU・メモリ・GPU のセッション）は外した後のグラフから見積もる。

段の選び方・優先度・装置の止め方は [9. クラスタとグリッド](09-cluster-grid.md)。
