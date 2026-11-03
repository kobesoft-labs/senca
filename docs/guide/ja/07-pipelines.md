# 7. パイプライン

[English](../en/07-pipelines.md)

パイプラインは、ストリームを入力として処理し、その結果を別のストリームに出力する機能です。
処理は有向非巡回グラフ（DAG）で定義し、デコード、板書の補正、拡大縮小、切り抜き、合成、音声のミックス、エンコードを組み合わせられます。
ピクチャー・イン・ピクチャー、シーンの切り替え（スイッチャー）、トランスコードなどに使えます。

- 設計の背景: [ADR-0004 コーデックと特許](../../dev/adr/0004-codecs-patents.md)、[ADR-0013 グリッド](../../dev/adr/0013-grid.md)（配置と縮退）
- 関連するページ: [4. 取り込みと再生](04-ingest-playback.md)、[9. クラスターとグリッド](09-cluster-grid.md)、[14. コーデックと特許](14-codecs-and-patents.md)、[15. API と SDK](15-api-and-sdks.md)

例で使う `API`・`TOKEN`・`H` は [4. 取り込みと再生](04-ingest-playback.md) と同じです。

この章では、グラフを構成する処理の単位（JSON の `nodes` の各要素）を**処理ノード**と呼び、SENCA のサーバー（senca-node）と区別します。

## 7.1 しくみ

- グラフは codec worker（サンドボックス化された別プロセス）の中で動きます。デコードした生のフレームが worker の外に出ることはありません。
  codec worker の無いノードではパイプラインは動きません（`GET /capabilities` の `pipelines` が `false` になります）。
- `source` は SENCA のストリームから AU（アクセスユニット）を読み込みます。`sink` は処理結果の AU を、出力先のストリームに**配信者として**送ります。
  出力先のストリームは通常のストリームと同じく、HLS・録画・出力・字幕の対象になります。
  出力先のストリームに別の配信者が接続していると、パイプラインは出力できません。
- パイプラインはグリッドのワークロード（`pipeline:{id}`）です。入力のストリームを受信しているノードに優先して配置されます（[9. クラスターとグリッド](09-cluster-grid.md)）。
- worker が異常終了したり処理に失敗したりすると `observed_state: error` になり、1・2・4…32 秒の間隔を置いて起動し直します。

## 7.2 API

| 操作 | リクエスト | 権限 |
|---|---|---|
| 作成 | `POST /pipelines` `{name, graph, desired_state?, placement?}` | `stream:write` |
| 一覧 | `GET /pipelines?limit&cursor` | `stream:read` |
| 取得 | `GET /pipelines/{id}` | `stream:read` |
| 変更 | `PATCH /pipelines/{id}` `{name?, graph?, desired_state?, placement?}`（`If-Match` 必須） | `stream:write` |
| 削除 | `DELETE /pipelines/{id}` | `stream:write` |
| 検証 | `POST /pipelines/{id}/validate`（本文で別のグラフを渡せます。省略すると保存済みのグラフを検証します） | `stream:read` |
| 操作 | `POST /pipelines/{id}/control` | `stream:write` |

- `name` の規則はストリームの名前と同じで、テナント内で一意です。1 つのテナントで作成できるパイプラインは 100 個までです。
- **`desired_state` の既定は `stopped`** です。動かすには `"running"` を指定してください。
- 作成時と変更時にグラフを検証します。`source`・`sink` のストリームは、同じテナントに存在している必要があります。
- `graph` を変更すると、パイプラインを停止して新しいグラフで起動し直します（出力は一時的に途切れます）。
- 応答には次のフィールドが含まれます: `observed_state`（`starting`・`running`・`error`・`stopped`）、`last_error`、`plan`（検証時の見積もり）、
  `status`・`meters`（worker の状態とメーター。既定では約 10 Hz で更新）、`placement`・`placement_status`。

検証に失敗すると problem+json を返し、`details.issues` に `[{code, node, message}]` が入ります。
デバイスが足りない場合（`issues` に `CAPACITY_UNAVAILABLE` を含む場合）は 503 `CAPACITY_UNAVAILABLE`（`retryable: false`）、それ以外は 400 `INVALID_CONFIG` です。

| `issues[].code` | 意味 |
|---|---|
| `MALFORMED` | JSON として解釈できない、未知のフィールドがある、`version` が 1 でない、16 MiB を超える |
| `INVALID_PARAM` | 値が範囲外、または形式が不正（ID の形式を含む） |
| `DUPLICATE_ID` | 同じ `id` の処理ノードが 2 つある |
| `UNKNOWN_REFERENCE` | 存在しない処理ノード・ポート・シーン・画像を参照している |
| `TYPE_MISMATCH` | ポートの型が合わない（エンコード済みのデータを合成に入れる、映像を Opus でエンコードする、sink に生のフレームを入れる、など） |
| `CYCLE` | 循環がある |
| `UNCONSUMED_OUTPUT` | 終端（`sink`・`snapshot_on_change`）以外の出力が、どこからも使われていない（`source` は宣言したトラックごと、`compose` は `program` について検査） |
| `POLICY_REJECTED` | ロイヤリティフリー限定なのに H.264・H.265・AAC を使っている（パススルーも含む） |
| `UNSUPPORTED_CODEC` | AAC のデコード・エンコード、または未知のコーデック |
| `CAPACITY_UNAVAILABLE` | H.264/H.265 のハードウェアが無い、その解像度に対応するエンコーダー・デコーダーが無い、ハードウェアの同時セッション数を超える |
| `LIMIT_EXCEEDED` | 処理ノードの数・解像度・画像・シーン・入力の数の上限を超える |

`source` か `sink` のストリームのうち 1 つでも `codec_profile: royalty_free` であれば、グラフ全体をロイヤリティフリー限定として検証します。

## 7.3 グラフの形式

```json
{"version": 1, "options": {...}, "nodes": [ {"type": "...", "id": "...", ...}, ... ]}
```

- `version` は 1 です。未知のフィールドは拒否します。
- `id` は英数字・`_`・`-` からなる 1〜64 文字です。処理ノードは 256 個までです。
- 入力の参照は `"処理ノード"` か `"処理ノード.ポート"` の形式で書きます。`source` はポートを省略できません（`cam.video`・`cam.audio`）。
  `compose` のポートは `program`（既定）と `preview` です。その他の処理ノードのポートは `out` で、省略できます。
- 解像度は各辺 2 以上、最大 7680×4320 です。fps は 240 まで（合成のキャンバスは 120 まで）です。

ポートの型:

| 型 | 出力する処理ノード |
|---|---|
| エンコード済みの映像・音声（コーデック付き） | `source`（`video`・`audio`）、`encode` |
| 生の映像 | `decode`（映像）・`board`・`scale`・`crop`・`compose` |
| 生の音声（48 kHz） | `decode`（音声）・`audio_mix`（48 kHz ステレオ） |

### `options`

| 項目 | 既定 | 範囲 | 意味 |
|---|---|---|---|
| `clock` | `realtime` | `realtime`・`input` | `realtime`: 単調増加の時計で出力のタイミングを進めます（ライブ向け）。`input`: 入力のタイムスタンプで進めます（すべての入力がそろってから出力します。ファイル処理やテスト向け） |
| `overflow` | `drop` | `drop`・`block` | キューが一杯のときの動作。`drop`: 古いものを捨てます（映像は次のキーフレームまで）。`block`: 上流を待たせます（`block_timeout_ms` を超えたら捨てます） |
| `latency_ms` | 100 | 0〜10000 | 入力の揺らぎを吸収するための遅延。合成とミキサーは、この分だけ遅れた時刻のフレームを描画します |
| `missing_timeout_ms` | 2000 | 100〜60000 | 入力が届かないとき、途切れたと判断するまでの時間 |
| `block_timeout_ms` | 2000 | 10〜60000 | `block` で待つ時間の上限 |
| `input_queue_aus` | 256 | 8〜100000 | 映像の AU の入力キューの長さ |
| `frame_queue` | 8 | 1〜256 | 生のフレームのキューの長さ（処理ノードの入力ごと） |
| `status_interval_ms` | 100 | 0 または 20〜60000 | 状態とメーターを通知する間隔（0 で通知しない） |
| `commit_timeout_ms` | 10000 | 100〜600000 | 新しい世代への切り替えを待つ時間の上限（ノードの API からは使用しません） |

## 7.4 処理ノード

### `source`（入力）

| 項目 | 必須 | 意味 |
|---|---|---|
| `stream` | はい | SENCA のストリームの ID |
| `video`・`audio` | どちらか一方は必須 | 使うトラック。`{codec, track?, width?, height?, fps?, sample_rate?, channels?}` |

`codec` は `h264`・`h265`・`av1`・`vp8`・`vp9`・`opus`・`aac` など、入力のストリームが実際に送るコーデックを書きます。
`track` を省略すると、その種類の最初のトラックを使います。
`width`・`height`・`fps`・`sample_rate`（8000〜192000）・`channels`（1〜8）は、検証と見積もりに使います。

### `decode`（デコード）

`{input, max_width?, max_height?, threads?}`。エンコード済みの入力を、生のフレーム・音声にデコードします。
`max_width`・`max_height` は受け付ける解像度の上限（既定 3840×2160）、`threads` はソフトウェアコーデックのスレッド数（0 = 自動、64 まで）です。
AAC はデコードできません（`UNSUPPORTED_CODEC`）。

### `board`（板書の補正）

`{input, params}`。ホワイトボードや黒板を正面から見た形に補正し、見やすくします。

| `params` の項目 | 既定 | 意味 |
|---|---|---|
| `mode` | `whiteboard` | `whiteboard`・`blackboard` |
| `corners` | なし | 板の 4 隅の座標（左上・右上・右下・左下の順、入力の画素単位）`[[x,y],…]` |
| `auto_detect` | `false` | 4 隅を自動で検出します（見つかるまで毎フレーム試します） |
| `redetect_frames` | 0 | 検出をやり直す間隔（フレーム数。0 = やり直さない） |
| `output_width`・`output_height` | 1280・720 | 出力の大きさ（16 以上）。`corners` も `auto_detect` も指定しない場合は、入力の大きさのままです |
| `enhance` | `true` | 照明のむらや反射を補正し、文字を強調します |
| `stroke_gain`・`chroma_gain` | なし | 0〜10 |
| `glare_suppression` | なし | |
| `invert_to_white` | `false` | 黒板を、白地に暗い文字の画像に変換します |
| `clahe` | `false` | 輝度に CLAHE を適用します |

### `scale`・`crop`

- `scale`: `{input, width, height, filter?}`。`filter` は `nearest`・`bilinear`（既定）・`bicubic`・`lanczos3`・`area` です。
- `crop`: `{input, x, y, width, height}`（入力の画素単位）。大きさは偶数に切り下げます。入力の大きさが分かっている場合、範囲からはみ出すと `INVALID_PARAM` になります。

### `compose`（合成）

| 項目 | 既定 | 意味 |
|---|---|---|
| `inputs` | （必須） | シーンで使う名前 → 上流の参照（生の映像）。32 個まで |
| `canvas` | （必須） | `{width, height, fps_num, fps_den?(1), background?("#000000")}`。幅と高さは偶数 |
| `images` | `{}` | 静止画（ロゴなど）。名前 → `{png_base64}` か `{jpeg_base64}`。32 枚まで、1 枚あたり 8 MiB（base64）まで。検証時に実際にデコードして確かめます |
| `scenes` | （必須） | シーン名 → `{layers: [...]}`。1〜64 個 |
| `program` | （必須） | 出力するシーン |
| `preview` | なし | `preview` ポートに描画するシーン |
| `transition` | `{kind: cut, duration_ms: 0}` | `take` の既定のトランジション。`kind` は `cut`・`fade`・`wipe_left`・`wipe_right`、`duration_ms` は 60000 まで |
| `missing` | `{hold_ms: 2000, slate_color: "#000000"}` | 入力が届かないときの表示。`hold_ms`（600000 まで）の間は最後のフレームを表示し、その後はスレートを表示します（`slate_image` は `images` の名前で、色より優先されます） |
| `threads` | 0 | 描画の並列度（0 = CPU の数、64 まで） |

名前（入力・画像・シーン）は、英数字・`_`・`-` からなる 1〜64 文字です。

レイヤー（`layers[]`）には、`source`（`inputs` の名前）・`image`（`images` の名前）・`color`（`#RRGGBB`）の**どれか 1 つ**を指定します。

| 項目 | 既定 | 意味 |
|---|---|---|
| `x`・`y`・`width`・`height` | （必須） | キャンバス上の位置と大きさ |
| `crop` | 0 | `{left, top, right, bottom}`。各辺から割合で切り取ります |
| `fit` | `contain` | `stretch`・`contain`・`cover` |
| `z` | 0 | 重なりの順序（大きいほど手前） |
| `opacity` | 1.0 | 0〜1 |
| `corner_radius` | 0 | 4096 まで |
| `border` | なし | `{width (512 まで), color}` |
| `visible` | `true` | |
| `filter` | `bicubic` | 拡大縮小のフィルター |

### `audio_mix`（音声のミックス）

| 項目 | 既定 | 意味 |
|---|---|---|
| `inputs` | （必須） | 名前 → `{from, gain_db?(0), mute?, solo?, pan?(0), delay_ms?(0)}`。`from` は生の音声。1〜32 個 |
| `master_gain_db` | 0 | -120〜24 |
| `limiter` | `true` | |

`gain_db` は -120〜24、`pan` は -1〜1、`delay_ms` は 0〜5000 です。出力は 48 kHz ステレオです。

### `encode`（エンコード）

| 項目 | 既定 | 意味 |
|---|---|---|
| `input` | （必須） | 生の映像か生の音声 |
| `codec` | （必須） | 映像: `av1`・`vp8`・`vp9`・`h264`・`h265`。音声: `opus` |
| `bitrate_kbps` | （必須） | 1〜800000 |
| `gop` | 2 秒分 | キーフレームの間隔（フレーム数、100000 まで） |
| `low_latency` | `true` | |
| `rate_control` | `cbr` | `cbr`・`vbr` |
| `width`・`height` | 入力の大きさ | 2 つをそろえて指定します。大きさの違うフレームは拡大縮小します |
| `frame_ms` | 20 | Opus の 1 パケットの長さ。2.5・5・10・20・40・60 |
| `threads` | 0 | 64 まで |

`h264`・`h265` は、GPU（NVENC・oneVPL・AMF）が無いと `CAPACITY_UNAVAILABLE` になります。`av1` は SVT-AV1（またはハードウェア）、
`vp8`・`vp9` は libvpx、`opus` は libopus でエンコードします。AAC のエンコードには対応していません（[14. コーデックと特許](14-codecs-and-patents.md)）。

### `snapshot_on_change`（板書の変化を静止画に保存）

`{input, params}`。生の映像を解析し、内容が変わってから落ち着いた時点で JPEG を保存します。保存先は**最初の `sink` のストリーム**の静止画です
（`GET /streams/{id}/snapshots`、`reason: board_change`、`changed_fraction` 付き）。この方法で保存した静止画は、枚数の上限による削除の対象になりません。

| `params` の項目 | 既定 | 範囲 |
|---|---|---|
| `mode` | `whiteboard` | `whiteboard`・`blackboard` |
| `enabled` | `true` | `false` の場合は、操作 `snapshot` で開始します |
| `analysis_fps` | 2.0 | 0〜120（0 = 全フレーム） |
| `stable_frames` | 3 | 1〜10000 |
| `change_fraction` | 0.004 | 0 より大きく 1 より小さい値 |
| `jpeg_quality` | 85 | 1〜100 |

### `sink`（出力）

`{inputs, stream}`。`inputs` には、エンコード済みの映像 1 つと音声 1 つまで（1〜2 個）を指定し、並びの順にトラック番号 0、1 が付きます。
`source` のトラックをそのまま指定すると、再エンコードせずにパススルーします。`stream` は出力先のストリームの ID です。

出力先のストリームでは、HLS のコンテナがそのコーデックを格納できるようにしてください（AV1・VP9 は `hls.container: fmp4`。[4.9](04-ingest-playback.md)）。

## 7.5 例: ピクチャー・イン・ピクチャー

板書のカメラ（`jb`）を全面に表示し、講師（`jt`）を右下に小さく重ねます。講師の音声を Opus に、映像を AV1 にエンコードして出力します。
3 つのストリームはすべて `codec_profile: royalty_free`・`hls.container: fmp4` で作成します。

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

`jt`・`jb` に VP9（と Opus）で配信すると、`jout` が配信中になります。`jout` の再生 URL を開くと、合成の結果を確認できます。

この構成には、VP9 のデコード（libvpx）、AV1 のエンコード（SVT-AV1）、Opus のエンコード（libopus）が必要です。
インストール方法は [14. コーデックと特許](14-codecs-and-patents.md) を参照してください。

## 7.6 オペレーターの操作（control）

`POST /pipelines/{id}/control` に、操作を表す JSON を送ります（`"op"` で操作の種類を選びます。未知のフィールドは 400）。
合成とミキサーへの操作は、次の出力フレームの境目で反映されます。

```sh
# プレビュー（teacher）をプログラムに切り替える（既定のトランジション = fade 300 ms）。プレビューには元のプログラムが入る
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"take","node":"prod"}'
# トランジションを指定してシーンを切り替える
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"switch_program","node":"prod","scene":"pinp","transition":{"kind":"wipe_left","duration_ms":500}}'
# マイクのフェーダーを下げる
curl -s -X POST "$API/pipelines/$PID/control" "${H[@]}" -d '{"op":"set_audio","node":"mix","input":"mic","gain_db":-6}'
```

`node` には、操作する処理ノードの `id` を指定します。

| `op` | 項目 | 意味 |
|---|---|---|
| `switch_program` | `node`、`scene`、`transition?` | プログラムのシーンを切り替えます（`transition` を省略するとカット） |
| `set_preview` | `node`、`scene` | プレビューのシーンを設定します |
| `take` | `node`、`transition?` | プレビューをプログラムに切り替えます（既定は合成の `transition`）。プレビューには元のプログラムが入ります |
| `update_layer` | `node`、`scene`、`layer`（番号）、`patch` | レイヤーの一部を変更します。`patch` に書けるのは `x`・`y`・`width`・`height`・`z`・`opacity`・`visible`・`source` です |
| `set_scene` | `node`、`scene`、`layers` | シーンを作成するか、置き換えます |
| `set_audio` | `node`（`audio_mix`）、`input`、`gain_db?`・`mute?`・`solo?`・`pan?`・`delay_ms?` | 音声の入力を変更します（指定した項目だけ） |
| `set_master` | `node`、`gain_db` | マスターの音量を設定します |
| `snapshot` | `node`（`snapshot_on_change`）、`enabled` | 変化の検出を開始・停止します |
| `request_keyframe` | `node?` | キーフレームを要求します（省略するとすべての映像のエンコード） |

- 成功すると 200 `{"ok": true, "reply": {...}}` を返します。パイプラインが動いていなければ 409、
  操作の内容に誤りがある場合（存在しない処理ノード・シーンなど）は 400（`details.reason`）です。
- `set_scene`・`update_layer` などの操作は動作中のパイプラインにだけ反映され、保存しているグラフは変わりません（起動し直すとグラフの内容に戻ります）。
- `prepare`・`commit`・`rollback`（世代の操作）はこの API では使えません（400）。グラフを変えるには `PATCH` を使ってください。

## 7.7 縮退（degrade）

デバイスやノードの資源が足りないとき、グリッドはパイプラインを停止する代わりに、あらかじめ決めておいた**縮退レベル**に従って
一部の処理ノードを外した状態で動かせます。

```sh
curl -s -X PATCH "$API/pipelines/$PID" "${H[@]}" -H "If-Match: $ETAG" -d '{
  "placement": {"priority": "high", "degrade": [
    {"label": "no-board", "disable": ["b"]}
  ]}
}'
```

- `placement.degrade` は、品質の高い順に並べたレベルの配列です（8 レベルまで）。各レベルは `{label, disable}` です。
  `label` は重複できず、`full`（レベル 0 の名前）は使えません。`disable` は空にできず、グラフにある処理ノードの ID だけを指定できます。
- レベルは**累積します**。レベル 2 では、レベル 1 とレベル 2 の両方の `disable` を外します。
- 外し方: 指定した処理ノードを外し、入力がすべて外れた下流の処理ノードも外します。入力の一部だけが外れる処理ノード
  （複数の入力を持つ `sink`・`compose`・`audio_mix`）は、外れた入力を除いて残します。`compose` では、外れた入力を使うレイヤーを各シーンから取り除きます。
- 外した後のグラフも検証します。上流を残して下流だけを外すと、上流の出力が `UNCONSUMED_OUTPUT` になって動かないため、**上流（`source` など）から外してください**。
  上の例では板書の `source`（`b`）を外します。すると `decode`（`db`）も連鎖して外れ、合成から板書のレイヤーが消えます（講師だけの映像になります）。
- レベルが変わると、パイプラインを新しいグラフで起動し直します。結果は `placement_status` に表示されます（`state: degraded`、`level`、`level_label`）。
- 各レベルで必要な資源（CPU・メモリ・GPU のセッション）は、処理ノードを外した後のグラフから見積もります。

レベルの選ばれ方、優先度、デバイスの停止については [9. クラスターとグリッド](09-cluster-grid.md) を参照してください。
