# Changelog / 変更履歴

## Unreleased / 未公開

### Added / 追加

- `--quic ADDR`: a QUIC v1 listener on one UDP port (off by default; uses `--tls-cert`/`--tls-key`), driven by one task
  over the sans-io `senca-quic` endpoint
  ／ `--quic ADDR`: 1 つの UDP ポートで QUIC v1 の接続を受け付けます（既定は無効。`--tls-cert`・`--tls-key` を使用）。
  sans-io の `senca-quic` の endpoint を 1 つのタスクで処理します
- HTTP/3 (ALPN `h3`): the same routes as TCP HTTP (`/api/v1`, signed HLS including LL-HLS blocking reloads, WHIP/WHEP,
  `/healthz`); SSE and `/signaling` answer 501. TCP HTTP responses advertise `Alt-Svc: h3=":PORT"`
  ／ HTTP/3（ALPN `h3`）: TCP の HTTP と同じルート（`/api/v1`、LL-HLS のブロッキングリロードを含む署名付き HLS、WHIP/WHEP、
  `/healthz`）に対応します。SSE と `/signaling` は 501 を返します。TCP の HTTP の応答には `Alt-Svc: h3=":ポート"` を付けます
- MoQ draft-22 subscriptions (native QUIC `moqt-22` and WebTransport `/moq`): every live stream is the namespace
  `senca/live/{stream_id}` with a hang `catalog.json` (CMAF init, codec description) and CMAF tracks (one fragment per
  object, one group per GOP), fed from the stream hub or a relay mirror; authorized by the playback signature
  (`exp`/`sig` as an AUTHORIZATION TOKEN, native PATH or WebTransport URL query). Playback URLs gain `moq`;
  capabilities gain `output.moq` and `output.http3`
  ／ MoQ draft-22 の購読（ネイティブ QUIC の `moqt-22` と WebTransport の `/moq`）: すべてのライブストリームを namespace
  `senca/live/{stream_id}` として公開します。内容は hang の `catalog.json`（CMAF の init、コーデックの description）と
  CMAF のトラック（1 object が 1 fragment、GOP ごとに 1 group）です。ストリームの hub または中継のミラーから生成します。
  認可には再生の署名を使います（`exp`・`sig` を AUTHORIZATION TOKEN、ネイティブの PATH、または WebTransport の URL の
  クエリで渡します）。再生 URL に `moq`、capabilities に `output.moq`・`output.http3` を追加しました
- MoQ publishing: `PUBLISH_NAMESPACE senca/live/{stream_id}` with the stream key as AUTHORIZATION TOKEN; the node
  subscribes to the catalog and CMAF tracks and becomes the stream's publisher (`cmaf` container only)
  ／ MoQ での配信（送信）: ストリームキーを AUTHORIZATION TOKEN に入れて `PUBLISH_NAMESPACE senca/live/{stream_id}` を
  送ります。ノードは catalog と CMAF のトラックを購読し、そのストリームの配信者になります（コンテナは `cmaf` のみ）
- `senca-moq`: draft-22 FILL_PARAMETERS answered with a fill stream (`Session::fill_object`/`fill_finish`), and an
  optional `description` in catalog renditions
  ／ `senca-moq`: draft-22 の FILL_PARAMETERS に fill の stream で応答します（`Session::fill_object`・`fill_finish`）。
  catalog の rendition に省略可能な `description` を追加しました
- `tools/interop/node_quic.sh`: aioquic (HTTP/3), moq-dev `moq export fmp4`/`fetch`, Chrome (HTTP/3 and WebTransport)
  ／ `tools/interop/node_quic.sh`: aioquic（HTTP/3）、moq-dev の `moq export fmp4`・`fetch`、Chrome（HTTP/3・WebTransport）

- Room cascading across nodes: with the room setting `cascade: true` (or `--rooms-cascade`), participants are served on the
  node they connect to (edge) and joined to the room's node (home) over the inter-node relay. Presence and tracks are synced as
  full snapshots, media is subscribed as `@room/<room>/<track>`, app messages go directly to the recipients' nodes. When the home
  dies, an edge with participants becomes the new home.
  ／ ノードをまたぐ部屋のカスケード: 部屋の設定 `cascade: true`（または `--rooms-cascade`）を有効にすると、参加者は接続した
  ノード（edge）で処理され、部屋の配置先のノード（home）とノード間の中継で接続されます。在室情報とトラックはスナップショット全体で
  同期し、メディアは `@room/<部屋>/<トラック>` として購読します。アプリのメッセージは宛先の参加者がいるノードへ直接送ります。
  home が停止すると、参加者のいる edge が新しい home になります。
- senca-relay: versioned, bounded App frame (`0x0f`, capability `APP`) for integration messages (`send_app`, `take_app_messages`).
  ／ senca-relay: 連携用メッセージのための、バージョン付き・サイズ上限付きの App フレーム（`0x0f`、capability `APP`）を追加しました（`send_app`・`take_app_messages`）。
- Auth webhook in the node: room setting `auth_webhook_url` or `--rooms-auth-webhook`, `--rooms-auth-mode after_token|only`,
  signed with `--rooms-webhook-secret-file`, destinations checked by the egress policy. Rooms without a URL are admitted by token.
  ／ ノードの認証 Webhook: 部屋の設定 `auth_webhook_url` または `--rooms-auth-webhook` で指定し、`--rooms-auth-mode after_token|only`
  で方式を選びます。リクエストは `--rooms-webhook-secret-file` の鍵で署名し、宛先は egress ポリシーで検査します。URL が未設定の
  部屋は、トークンだけで参加を認めます。
- Event webhook: `--rooms-event-webhook` delivers room events (signed, at least once, retries, dead letters shown in
  `GET /api/v1/nodes`); SSE is unchanged.
  ／ イベント Webhook: `--rooms-event-webhook` で部屋のイベントを送信します（署名付き、少なくとも 1 回の配送、再送あり。
  配送をあきらめた件数（dead letter）は `GET /api/v1/nodes` に表示）。SSE は従来どおりです。
- Interop test `tools/interop/cluster_rooms.sh` (3 nodes, Chrome, home node failure, mock webhook receiver).
  ／ 相互接続テスト `tools/interop/cluster_rooms.sh`（3 ノード、Chrome、home ノードの障害、Webhook のモック受信側）。

### Security / セキュリティ

- Node-to-node authentication (Raft and relay) is now a mutual challenge-response with fresh nonces and HMAC keys
  derived from the cluster secret (HKDF-SHA256); the old fixed secret hash, which could be replayed, is gone. It also
  runs inside mTLS. Nodes of earlier versions cannot join; upgrade the whole cluster together
  ／ ノード間（Raft・中継）の認証を、新しい乱数とクラスタシークレットから導出した鍵（HKDF-SHA256）の HMAC による相互の
  チャレンジレスポンス認証にしました。再送で通ってしまう固定のハッシュは送りません。mTLS の中でも行います。以前のバージョンの
  ノードとは接続できないため、クラスタはまとめて更新してください
- Secrets are sealed with a cluster-wide data key, wrapped with a key derived from the cluster secret and replicated
  in the control state, so a secret stored on one node opens on every node. Secrets sealed by earlier versions with a
  node's own `secret-key` are re-sealed automatically. Single-node behavior is unchanged
  ／ 秘密値をクラスタ共通のデータ鍵で暗号化するようにしました（データ鍵はクラスタシークレットから導出した鍵で暗号化して制御状態で
  複製）。どのノードで登録した秘密値もすべてのノードで使えます。以前のバージョンでノードの `secret-key` で暗号化した値は自動で
  暗号化し直します。単一ノードの動作は変わりません

### Cluster and grid / クラスタとグリッド

- Fencing: grid work (pull inputs, push outputs, pipelines) checks that it still holds the current assignment before
  external side effects and every 0.5 s while running. A node that has lost contact with a quorum for more than 4 s
  stops its grid work and reports 503 on `/readyz` until contact returns. Recording finalization waits for a quorum
  and closes as `interrupted` if the stream's ownership moved
  ／ フェンシング: ワークロード（pull の入力・push の出力・パイプライン）は、外部への副作用の前と実行中 0.5 秒ごとに、現在も
  自ノードへの割り当てかを確認します。過半数との通信が 4 秒を超えて途絶えたノードはワークロードを停止し、回復するまで
  `/readyz` は 503 を返します。録画の確定は過半数との通信を待ち、所有権が移っていれば `interrupted` で閉じます
- Stream ownership is released (`ReleaseStream`) when the input ends after the reconnect grace, and cleared for nodes
  marked down; viewers on other nodes get 404 instead of a redirect to a stale owner
  ／ 入力が終わる（再接続の猶予の後）と所有権を解放し、停止と判定したノードの所有権も解放します。他のノードの視聴者は停止した
  所有者へリダイレクトされず 404 になります
- Draining nodes refuse new RTMP, SRT and WHIP publishers (WHIP is redirected with 307 to a node that is not draining);
  connected sessions are kept
  ／ drain 中のノードは、新しい RTMP・SRT・WHIP の配信者を拒否します（WHIP は drain 中でないノードへ 307 でリダイレクト）。接続済みのセッションは継続します
- Heartbeats report measured CPU, memory and network usage from `/proc`, shown as `usage` in `GET /api/v1/grid`.
  Devices are re-probed every 10 s (fresh codec worker Hello, capture device list) and devices that disappear raise
  `device.down` automatically (`device.up` when back); the manual disable flag stays separate
  ／ ハートビートに `/proc` から測定した CPU・メモリ・帯域の使用量を含めます（`GET /api/v1/grid` の `usage`）。デバイスは 10 秒ごとに
  再検出し（新しい codec worker の Hello、キャプチャデバイスの一覧）、検出されなくなると自動で `device.down`、戻ると `device.up` に
  なります。手動の無効化とは別です
- The node refuses to start when a relay address (default Raft port + 1) collides with another node's Raft or relay
  address, or when peer IDs are duplicated
  ／ 中継アドレス（既定は Raft のポート + 1）が他のノードの Raft・中継のアドレスと重なる設定や、peers の番号の重複は起動時にエラーにします

### Configuration and logs / 設定とログ

- `labels` in the JSON config file are validated like `--label` (also: no control characters, `raft_id` reserved,
  at most 64) ／ 設定ファイルの `labels` も `--label` と同じ規則で検査します（制御文字は不可、`raft_id` も予約、64 個まで）
- `--log-json` now also covers the first-admin-token notice, codec/capture worker stderr lines, configuration errors
  and fatal errors ／ `--log-json` で、最初の管理者トークンの通知・codec/capture worker の標準エラー出力・設定の誤り・
  起動の失敗も JSON で出力します

## v0.0.0 — 2026-11-03

First public snapshot.

最初の公開版です。

### Included / 含まれるもの

- Ingest: RTMP/RTMPS (Enhanced RTMP), SRT, WHIP, RTSP/RTMP/SRT/HLS pull, capture devices
  ／ 取り込み: RTMP/RTMPS（Enhanced RTMP）、SRT、WHIP、RTSP・RTMP・SRT・HLS の pull、キャプチャデバイス
- Delivery: HLS (TS, fMP4, LL-HLS), RTMP/SRT/RTSP play, WHEP, signed URLs, relay mirrors across nodes
  ／ 配信: HLS（TS・fMP4・LL-HLS）、RTMP・SRT・RTSP の play、WHEP、署名付き URL、ノード間の中継のミラー
- Recording, VOD, MP4 export, schedules, crash recovery ／ 録画・VOD・MP4 の書き出し・予約録画・障害後の復旧
- Pipelines and production controls, snapshots, captions and translation
  ／ パイプラインと配信制作の操作、静止画、字幕と翻訳
- Codecs: H.264/H.265 through GPU vendor SDKs only; AV1, VP8/VP9 and Opus in software
  ／ コーデック: H.264/H.265 は GPU メーカーの SDK のみ、AV1・VP8/VP9・Opus はソフトウェア
- Clustering: in-house Raft, mTLS, ownership with fencing ／ クラスター: 独自実装の Raft、mTLS、所有権と fencing
- Grid: resource-aware placement, manual pins, priorities, failover with degradation and restore
  ／ グリッド: 資源に応じた配置、手動の割り当て、優先度、縮退を伴うフェイルオーバーと復帰
- Realtime rooms (SFU, simulcast, spotlight, forwarding filters, DataChannels, join tokens)
  ／ リアルタイムの部屋（SFU・サイマルキャスト・スポットライト・転送フィルター・DataChannel・参加トークン）
- Media pipelines for rooms (export, composite, record, live connector, transcription)
  ／ 部屋のメディアパイプライン（書き出し・合成・録画・ライブ配信サービスへの送信・文字起こし）
- REST API (OpenAPI), SSE, web console, browser SDK, TypeScript and Rust API clients
  ／ REST API（OpenAPI）、SSE、管理画面、ブラウザー用 SDK、TypeScript・Rust の API クライアント
- QUIC v1, HTTP/3, WebTransport and MoQ as libraries (not yet wired into the node)
  ／ QUIC v1・HTTP/3・WebTransport・MoQ のライブラリ（この時点ではノードに組み込まれていません）

### Not verified yet / 未検証の項目

- GPU codecs on real hardware, capture hardware, iOS/Android sender apps
  ／ 実機 GPU でのコーデック、キャプチャデバイス、iOS・Android の配信アプリ
- Browsers other than Chrome, TURN paths ／ Chrome 以外のブラウザー、TURN の経路
- Room cascading across nodes, auth webhook for rooms in the node
  ／ ノードをまたぐ部屋のカスケード、ノードでの部屋の認証 Webhook
- Real cloud PaaS accounts (tested against mocks only) ／ 実際のクラウド PaaS のアカウント（モックの API でのみ確認）
