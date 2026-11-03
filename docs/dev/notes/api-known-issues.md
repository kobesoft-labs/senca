# 管理 API（openapi.yaml）の既知の差と実装上の注記

`docs/api/openapi.yaml` は利用者向けに、ノードが実際に返す挙動だけを書いています。
このファイルには、開発者向けの情報（仕様 SENCA-SRS-001 との差、実装の不備、内部の型・ファイル名、文書の根拠）をまとめます。
openapi.yaml の記述を変えるときは、ここも合わせて見直してください。

## 全体
- 文書の根拠: 仕様 SENCA-SRS-001 10 章、REQ-002。実装は `crates/senca-node/src/api/`（経路表は `api/mod.rs`）。
- `tools/interop/openapi.sh` が (a) 経路表と openapi.yaml の (method, path) の一致と `x-senca-scope` と handler の権限、(b) 実地の応答の状態コードと本文のスキーマを確かめる。
- 未知のフィールドの 400 は serde の `deny_unknown_fields`。
- 仕様との主な差:
  - 非同期処理の operation 資源（`GET /operations/{id}`）は無い。仕様で 202 + operation の操作は、202 で対象資源そのものを返す。
  - `PATCH /streams/{id}` は 202 ではなく 200（その場で反映した資源を返す）。`DELETE /streams/{id}` は 202 ではなく 204。
  - `POST /streams/{id}/sessions`、`DELETE /sessions/{id}`、`GET /inputs/{id}/reservations` は未実装。
  - 入力は pull だけ（push は stream の鍵で受ける）。
  - 入力・出力・秘密値・パイプライン・部屋・メディアパイプラインの DELETE は `If-Match` を読まない。
  - SSE は再送の保持を持たない。
  - 429 は `components.responses` に定義するが、レート制限は未実装でどの操作も返さない。
- タグの根拠: schedules は REQ-002、tokens は仕様 10.2 の表にない追加（ADR-0005）、io・secrets は仕様 10.2（「credentials は secret reference」）、pipelines は REQ-002・REQ-005・REQ-008、events は仕様 10.5。
- 入出力を動かすのはノードの `crates/senca-node/src/io.rs`。パイプラインのグラフは `crates/senca-pipeline/src/model.rs` の `Graph`。
- 以前の openapi.yaml で「部屋の核」と呼んでいたものは、部屋を置いたノードで WebRTC を駆動する部分（senca-rooms）。利用者向けには「部屋を配置したノード」と書いている。メディアパイプラインを動かす係は `crates/senca-node/src/mediapipes.rs`。
- media-pipelines タグの以前の記述では、capture の保存先を「録画の設定どおり、ローカル・S3 互換」としていたが、ガイド（11 章）ではノードのデータディレクトリのみで S3 互換の保存先はノードに未実装とされている。ノードのコードに S3 の扱いは無いため、タグの記述はデータディレクトリに合わせた。

## system
- `GET /nodes`: 仕様との差。ページングが無い（`items` だけ、`next_cursor` なし）。クエリは読まない。
- `POST /nodes/{id}/drain`: 仕様は 202 と operation 資源だが、実装は 202 で Node をそのまま返す。
- `GET /devices`: 仕様は全ノードのハードウェアだが、今の実装は自ノードだけを返す。

## events
- `GET /events` は HTTP 層の `crates/senca-node/src/events.rs` が扱う（ルーターの handler ではない）。
- 仕様との差: 制御のイベントの `data` に `event_id`・`generation` の欄が無い（`event_id` は SSE の `id`）。`occurred_at` は送った時刻。仕様の保持期間 24 時間の再送は無い。
- 部屋の核の出来事の `data.data` は部屋の核の `data` そのもの。

## streams
- `POST /streams`: 仕様 10.3 の `input`・`outputs`・`transform` は本文に無い。
- `PATCH /streams/{id}`: 仕様は 202、実装は 200。
- `DELETE /streams/{id}`: 仕様は 202、実装は 204（非同期の operation は無い）。録画を消さないのは仕様 10.2 のとおり。
- `POST /streams/{id}/recordings`: 仕様は operation 資源、実装は Recording を返す。

## recordings
- `GET /recordings`: 実装の不備。`state` クエリは受け付けるが絞り込みに使わない。
- `PATCH /recordings/{id}`: 要る権限が `recording:*` ではなく `stream:write` なのは実装の選択（仕様の表に無い操作）。

## schedules
- `GET /schedules`: 実装の不備。`state` クエリは受け付けるが絞り込みに使わない。

## tokens
- 以前の文書には「`POST /tokens` の `Location`（`/api/v1/tokens/{id}`）に対する `GET /tokens/{id}` は無い（405）」とあったが、経路表（`api/mod.rs`）には `GET /tokens/{id}`（`tokens::get`）があり、openapi.yaml にも `getToken` がある。古い注記と判断して削除した。

## io
- `POST /streams/{id}/outputs` は REQ-001。
- `whip_push` は未実装（400）。

## secrets
- `GET /secrets`: 実装の不備。ページングが無く、クエリを読まない（`limit`・`cursor`・知らないクエリも 400 にならない。他の一覧と違う）。

## pipelines
- `PATCH /pipelines/{id}`: 本文の検査の後に `If-Match` を比べるため、古い `If-Match` と不正な本文では 400 が先に返る（他の PATCH と順序が違う）。
- `POST /pipelines/{id}/control`: 実行時に操作が拒否された（知らない部品・シーン・レイヤーなど）ときは 409 `CONFLICT`（`title` に worker の誤り）。本来は 400 にすべきもの（実装の不備）。

## rooms
- 部屋の `settings` の検査は部屋の核（WebRTC の駆動役）と同じ。
- `PATCH /rooms/{id}`: 参加者がいる間の `e2ee`・`video_codec`・`audio_codec` の変更拒否（409）は、部屋を置いたノードがこのノードのときだけ確かめる（別のノードに置いた部屋では確かめない）。
- `POST /rooms/{id}/tokens`: 仕様との差。senca-rooms 単体の発行（`RoomService::api_issue_token`）の `ttl_secs` の既定は 3600 で `expires_at` は UNIX 秒だが、ノードの API は既定 600 秒で RFC 3339。

## media-pipelines
- `RoomFilter`（`GET /media-pipelines?room=`）の以前の説明に「今は 400 になる」とあったが、`Ctx::page`（`api/mod.rs`）は `/media-pipelines` の `room` を受け付け、`media_pipelines::list` で絞り込んでいる。古い注記と判断して削除した。

## playback
- ライブ HLS の字幕は R6-4。

## スキーマ
- `CodecProfile`: royalty_free の拒否は REQ-007。
- `Stream.observed_state`: 仕様 10.4 の provisioning・draining・degraded・failed は今は出さない（enum は `waiting_input`・`live`・`stopped` だけ）。
- `CaptionSettings` は R6-4。
- `PipelineGraph` は senca-pipeline の `Graph`（`crates/senca-pipeline/src/model.rs`）。処理ノードごとの設定のすべては `cargo doc -p senca-pipeline` にある。`PipelineStatus` は `Event`、メーターは `Event::Meters`、`PipelineLayer` は `LayerSpec`、`PipelineControl` は `Control`。
- `RoomSettings` の範囲の検査は、API では形と `max_participants` だけで、残りは部屋の核が部屋を開くときに行う。
- `GridNode.capture` は REQ-004。
- `Problem` は仕様 10.4。

## 文書と実装のずれ（未解決）
- `Node` の応答（`GET /nodes`・`GET /nodes/{id}`・drain・undrain）に、コミット f5f99d9（部屋のカスケード）で `rooms`（`crates/senca-node/src/app.rs` の `rooms_status()`、自ノードのときだけ値）が加わったが、openapi.yaml の `Node` スキーマには無い（`additionalProperties: false`）。そのため `tools/interop/openapi.sh` の実地検査がこの 4 操作で「文書に無いフィールド rooms」として失敗する。スキーマに `rooms` を足す必要がある（形は `crates/senca-node/src/cascade.rs` の状態の JSON `{rooms: [...]}`）。
- 生成物 `apps/sdk-js/src/rest/openapi.gen.d.ts`・`crates/senca-client/src/models.rs` は、openapi.yaml の `LivePlayback.playback.moq`・`Capabilities` の `http3`・`moq`、`GridNode.usage` を反映しないまま古くなっていた（生成し直すとこれらが加わる）。
