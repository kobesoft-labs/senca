# 部屋とシグナリングの実装メモ / Rooms and signaling: implementation notes

利用者ガイド（[10. リアルタイムの部屋](../../guide/ja/10-rooms.md)）とシグナリングの仕様（[シグナリング v1](../../realtime/signaling.md)）から、
サーバー内部の実装に関する記述をこちらへ移しました。

These server-internal details were moved here from the user guide ([10. Realtime rooms](../../guide/en/10-rooms.md)) and the
signaling specification ([Signaling v1](../../realtime/signaling.en.md)).

## コードの場所 / Where the code lives

| 内容 / Topic | 場所 / Location |
|---|---|
| 部屋の核（SFU の状態機械）/ Room core (SFU state machine) | `crates/senca-rooms`（`signaling.rs` がメッセージの型、`service.rs` が状態機械 / message types in `signaling.rs`, state machine in `service.rs`） |
| WebSocket | `crates/senca-ws` |
| ノードへの組み込み / Node integration | `crates/senca-node/src/rooms.rs` |
| REST | `crates/senca-node/src/api/rooms.rs` |
| メディアパイプラインの E2EE 検査 / E2EE check for media pipelines | `crates/senca-node/src/api/media_pipelines.rs` の `check` |
| グリッドの見積もり / Grid estimate | `crates/senca-control/src/workloads.rs` |
| Webhook の署名と再送 / Webhook signing and retries | `senca_rooms::webhook::verify_signature`、`senca_rooms::webhook::{Outbox, retry_delay}` |

利用者向けの文書では「部屋の核」という言葉を使わず、「部屋を配置したノード」「部屋の中のイベント」と書いています。
SSE の `room.connection.*` などは、部屋の核のイベント名（`connection.*`）にノードが `room.` を付けたものです。

The user documents avoid the term "room core" and speak of "the hosting node" and "in-room events". SSE names such as
`room.connection.*` are the room core's event names (`connection.*`) prefixed with `room.` by the node.

## 部屋の核の設定 / Room core configuration (`ServiceConfig`)

- 受信スロットの上限は `ServiceConfig::max_slots_per_kind`（32）。/ The slot limit is `ServiceConfig::max_slots_per_kind` (32).
- ノードあたりの部屋数・接続数は `max_rooms`・`max_connections`（どちらも既定 10000。ノードからは変更できない）。
  Rooms and connections per node are `max_rooms` and `max_connections` (both 10000 by default; not configurable from the node).
- 認証 Webhook は `ServiceConfig::auth_webhook` が `Only`（トークンの代わり）か `AfterToken`（トークンの後にも）。
  ノードの `--rooms-auth-mode only|after_token` がこれに対応する。
  The auth webhook mode is `ServiceConfig::auth_webhook` = `Only` (instead of tokens) or `AfterToken` (after tokens as well),
  which the node's `--rooms-auth-mode only|after_token` maps to.
- `overflow_redirect` は部屋の核にあるが、ノードからは設定できない。/ `overflow_redirect` exists in the room core but the node cannot set it.

## REST の更新の規則 / Update rules in REST

- `PATCH /rooms/{id}` の `settings` は JSON Merge Patch（`api::rooms::merge_patch`）。保存してある設定（既定値を埋めた全体）に重ね、
  `RoomSettings` として検査する。`settings.recording` から録画のメディアパイプライン（`room-rec.{部屋}`、`managed`）を導くのは
  `ControlState::apply` の `PutRoom`（`sync_room_recording`。決定的で、全ノードで同じ結果）。
- `PATCH /rooms/{id}` `settings` is a JSON Merge Patch (`api::rooms::merge_patch`) applied to the stored full settings and validated as
  `RoomSettings`. The managed recording media pipeline (`room-rec.{room}`) is derived from `settings.recording` inside `ControlState::apply`
  for `PutRoom` (`sync_room_recording`, deterministic on every node).

- ノードの `PATCH /rooms/{id}` は、参加者がいる間の `e2ee`・`video_codec`・`audio_codec` の変更を 409 で拒否する（部屋を置いたノードに参加者数を問い合わせる。
  問い合わせに失敗した場合は検査を省く）。
- 部屋の核の API（`RoomService` の更新）は、`e2ee` は参加者（リモートを含む）がいる間、`video_codec` はトラックがある間だけ拒否する。
  ノードの検査の方が厳しいので、利用者向けの文書にはノードの規則を書いている。
- The node's `PATCH /rooms/{id}` rejects changes to `e2ee`, `video_codec` and `audio_codec` with 409 while participants are present
  (it asks the hosting node for the participant count and skips the check if that query fails).
- The room core's own update API rejects `e2ee` changes while participants (including remote ones) exist, and `video_codec` changes
  only while tracks exist. The node's check is stricter, so the user documents describe the node's rule.

## E2EE と合成 / E2EE and compositing

- 部屋の核には `RoomView.compositing` と `RoomService::api_plan_composite` があり、E2EE の部屋での合成の計画を 422 で拒否する。
- ノードのメディアパイプライン API（`api/media_pipelines.rs` の `check`）は、E2EE の部屋で `composite` か `captions` を持つ
  メディアパイプラインの作成・変更を 422（`UNSUPPORTED_CODEC`、`details.reason: "e2ee"`）で拒否する。
  以前の利用者ガイドには「まだ検査しない」とあったが、コードは検査している。
- The room core has `RoomView.compositing` and `RoomService::api_plan_composite`, which reject compositing plans for E2EE rooms with 422.
- 部屋の `recording: composite` と `e2ee` の組は、ノードの REST では 422（`details.reason: "e2ee"`。部屋の核の `validate` の 400 より先に検査）。
  E2EE の部屋から書き出した stream には `metadata.e2ee: "true"` を付け（PATCH で消えない）、静止画・字幕・静止画の保存・静止画の予約・
  パイプラインの入力を 422 で断り、録画のサムネイルを作らない（`api::streams::stream_is_e2ee`）。
- With the room's `recording: composite` and `e2ee`, the node's REST answers 422 (`details.reason: "e2ee"`, checked before the room core's
  400 from `validate`). Streams exported from E2EE rooms carry `metadata.e2ee: "true"` (kept across PATCH); still images, captions,
  saved snapshots, snapshot schedules and pipeline sources are refused with 422, and recordings get no thumbnail (`api::streams::stream_is_e2ee`).
- The node's media pipeline API (`check` in `api/media_pipelines.rs`) rejects creating or updating a media pipeline with `composite`
  or `captions` for an E2EE room with 422 (`UNSUPPORTED_CODEC`, `details.reason: "e2ee"`). An earlier version of the user guide said
  this was not checked yet; the code does check it.

## カスケードの同期 / Cascade synchronization

- 在室と公開したトラックは、ノード間の中継（ADR-0014）の App 枠（topic `senca.rooms/1`、版つき、1 MiB まで）で、部屋ごとの全量を同期する。
  変化があればすぐ、無くても 2 秒ごとに送る。edge は自分の参加者を home へ、home は宛先のノード以外の全員を各 edge へ送る。
  7 秒同期が届かないノードの参加者は外す。
- メディアは、参加者のいるノードへ直接 `@room/<部屋>/<トラック>` を購読する。レイヤーの選択とキーフレームの要求も同じ購読で行う。
- ノード間で運ぶのは、そのノードの購読者が必要とするレイヤーだけ（自動選択の購読者がいれば全レイヤー、全員が固定のレイヤーならそのレイヤーだけ）。
- アプリケーションのメッセージは、受信者のいるノードへ直接送る。
- メディアパイプラインの書き出し（home の係）は、edge の参加者のトラックを `@room/<部屋>/<トラック>/export` で edge から直接購読する。
  edge は購読者を `<home>#export` という名前で部屋の核に登録する（部屋の核どうしの購読 `<home>` と重ならない）。全部の層を受け、
  層の選択・キーフレームの要求は書き出しの側（`bridge_deliver`）で行う。音声は home の話者の検出にも入れる（`observe_track_audio`）。
- 参加者の変更・切断（REST）は、受けたノードの部屋の核に聞き、手元の参加者なら実行、別のノードの参加者（`ParticipantInfo.node`）なら
  そのノードへ転送する。手元に無い部屋は home へ転送する。`X-Senca-Forwarded` に通ったノードを並べ、同じノードへ戻る・3 つ目を通る中継はしない。
- Presence and published tracks are synchronized as a full per-room snapshot over relay App frames (topic `senca.rooms/1`,
  versioned, up to 1 MiB): immediately on change, otherwise every 2 seconds. Edges send their own participants to the home; the home
  sends everyone except the destination's participants to each edge. Participants of a node silent for 7 seconds are removed.
- Media is subscribed directly from the participant's node as `@room/<room>/<track>`; layer selection and keyframe requests use the same subscription.
- Only the layers needed by a node's subscribers cross nodes (all layers if any subscriber uses automatic selection; otherwise only the fixed layers).
- Application messages go directly to the recipients' nodes.

## テスト / Tests

- `tools/interop/rooms.sh`: 部屋の核を Chrome 3 ページで確かめる。/ Exercises the room core with three Chrome pages.
- `crates/senca-rooms/tests/e2e.rs`: senca-webrtc どうしでの端から端までの試験。/ End-to-end tests with senca-webrtc on both sides.
- `tools/interop/node_rooms.sh`: ノードの REST で部屋を作り、トークンで参加し、互いの映像のデコード・DataChannel の往復・
  サイマルキャストのレイヤーの切り替え・REST の参加者・統計・プッシュ・SSE のイベントを Chrome 3 ページで確かめる。
  Creates a room through the node's REST API, joins with tokens, and checks mutual video decoding, DataChannel round trips,
  simulcast layer switching, the REST participants, stats and push, and SSE events with three Chrome pages.
  ノードは 0.0.0.0 で待ち `--webrtc-public-ip` の IP で Chrome がつながる。別の小さなノードで SSE の再送（再起動をまたぐ）・resync、
  量の上限（token・ip・connect・auth_failures）、`UNSUPPORTED_PROTOCOL_VERSION`・`INPUT_TIMEOUT`（キーフレームを落とした ffmpeg）、
  `publish` のトークンでの WHIP（手書きの offer）、0.0.0.0 の自動の候補、settings の Merge Patch、`recording` の managed、E2EE の 422 を確かめる。
  The node binds 0.0.0.0 and Chrome connects through the `--webrtc-public-ip` address. Separate small nodes check SSE replay (across a
  restart) and resync, rate limits (token, ip, connect, auth_failures), `UNSUPPORTED_PROTOCOL_VERSION` and `INPUT_TIMEOUT` (ffmpeg with
  keyframes dropped), WHIP with a `publish` token (hand-written offer), automatic candidates on 0.0.0.0, the settings merge patch, the
  managed `recording` pipeline and the E2EE 422s.
- `tools/interop/cluster_rooms.sh`: 3 台のクラスタで、カスケード（edge どうしの映像と DataChannel、home の停止後の入り直し）、
  認証 Webhook（許可・拒否・役割の上書き・部屋ごとの URL）、イベント Webhook（署名、500 での再送）を確かめる。
  On three nodes: cascading (video and DataChannel across edges, rejoining after the home stops), the auth webhook
  (allow, deny, role override, per-room URL) and the event webhook (signature, retry on 500).
  ほかに、SSE の id がノード間で同じで別のノードで Last-Event-ID から続くこと、edge の参加者の変更・切断を別のノードの REST から行えること、
  メディアパイプラインが edge の参加者を書き出して HLS で復号できること、配信を受けていないノードでの録画の開始が `OWNERSHIP_LOST` になること。
  Also: SSE ids are equal across nodes and resume on another node; an edge participant is updated and disconnected through another node's REST;
  a media pipeline exports an edge participant that decodes over HLS; starting a recording on a non-owner node is `OWNERSHIP_LOST`.
- `tools/interop/node_media_pipelines.sh`: `speaker` の並びが、声を出している参加者（雑音を送る bob）に切り替わり、音を止めた alice へは戻らないこと。
  The `speaker` layout switches to the participant with audio (bob sends noise) and never to the muted alice.
