# Changelog / 変更履歴

## Unreleased

### Added / 追加

- Room cascading across nodes: with the room setting `cascade: true` (or `--rooms-cascade`), participants are served on the
  node they connect to (edge) and joined to the room's node (home) over the inter-node relay. Presence and tracks are synced as
  full snapshots, media is subscribed as `@room/<room>/<track>`, app messages go directly to the recipients' nodes. When the home
  dies, an edge with participants becomes the new home.
  ／ ノードをまたぐ部屋のカスケード: 部屋の設定 `cascade: true`（または `--rooms-cascade`）で、参加者はつないだノード（edge）で
  受け、部屋の配置先（home）とノード間の中継でつながります。在室とトラックは全量で同期し、メディアは `@room/<部屋>/<トラック>`
  の購読、アプリのメッセージは受け手のノードへ直接送ります。home が落ちると、参加者のいる edge が新しい home になります。
- senca-relay: versioned, bounded App frame (`0x0f`, capability `APP`) for integration messages (`send_app`, `take_app_messages`).
  ／ senca-relay: 統合側のメッセージのための、版つき・上限つきの App 枠（`0x0f`、capability `APP`）。
- Auth webhook in the node: room setting `auth_webhook_url` or `--rooms-auth-webhook`, `--rooms-auth-mode after_token|only`,
  signed with `--rooms-webhook-secret-file`, destinations checked by the egress policy. Rooms without a URL are admitted by token.
  ／ ノードの認証 Webhook: 部屋の設定 `auth_webhook_url` か `--rooms-auth-webhook`、`--rooms-auth-mode after_token|only`、
  `--rooms-webhook-secret-file` で署名、宛先は egress の方針で検査。URL の無い部屋はトークンで認めます。
- Event webhook: `--rooms-event-webhook` delivers room events (signed, at least once, retries, dead letters shown in
  `GET /api/v1/nodes`); SSE is unchanged.
  ／ イベント Webhook: `--rooms-event-webhook` で部屋の出来事を送ります（署名つき・少なくとも 1 回・送り直し・諦めた件数は
  `GET /api/v1/nodes` に表示）。SSE は今までどおりです。
- Interop test `tools/interop/cluster_rooms.sh` (3 nodes, Chrome, home node failure, mock webhook receiver).
  ／ 相互接続試験 `tools/interop/cluster_rooms.sh`（3 台、Chrome、home のノードの障害、Webhook の模擬の受け手）。

## v0.0.0 — 2026-11-03

First public snapshot. **Experimental**: the release gate in docs/RELEASE.md (long fuzzing, soak test,
independent security review, signed builds) has not been passed. Not for production use.

最初の公開。**実験的**: docs/RELEASE.md の出荷 gate（長時間の fuzz、連続試験、独立したセキュリティレビュー、
署名付きの配布）は通していない。本番には使わないこと。

### Included / 含まれるもの

- Ingest: RTMP/RTMPS (Enhanced RTMP), SRT, WHIP, RTSP/RTMP/SRT/HLS pull, capture devices
  ／ 受信: RTMP/RTMPS（Enhanced RTMP）、SRT、WHIP、RTSP・RTMP・SRT・HLS の pull、キャプチャ機器
- Delivery: HLS (TS, fMP4, LL-HLS), RTMP/SRT/RTSP play, WHEP, signed URLs, relay mirrors across nodes
  ／ 配信: HLS（TS・fMP4・LL-HLS）、RTMP・SRT・RTSP の play、WHEP、署名付き URL、ノード間の中継の写し
- Recording, VOD, MP4 export, schedules, crash recovery ／ 録画・VOD・MP4 の書き出し・予約・障害からの回収
- Pipelines and production controls, snapshots, captions and translation
  ／ パイプラインと制作の操作、静止画、字幕と翻訳
- Codecs: H.264/H.265 through GPU vendor SDKs only; AV1, VP8/VP9 and Opus in software
  ／ コーデック: H.264/H.265 は GPU の SDK だけ、AV1・VP8/VP9・Opus はソフトウェア
- Clustering: in-house Raft, mTLS, ownership with fencing ／ 分散: 自前の Raft、mTLS、所有権と fencing
- Grid: resource-aware placement, manual pins, priorities, failover with degradation and restore
  ／ グリッド: 資源を数えた配置、手動の割り当て、優先度、縮退を伴う移動と復帰
- Realtime rooms (SFU, simulcast, spotlight, forwarding filters, DataChannels, join tokens)
  ／ リアルタイムの部屋（SFU・サイマルキャスト・スポットライト・転送フィルター・DataChannel・参加トークン）
- Media pipelines for rooms (export, composite, record, live connector, transcription)
  ／ 部屋のメディアパイプライン（書き出し・合成・録画・送り出し・文字起こし）
- REST API (OpenAPI), SSE, web console, browser SDK, TypeScript and Rust API clients
  ／ REST API（OpenAPI）、SSE、管理画面、ブラウザーの SDK、TypeScript・Rust の API クライアント
- QUIC v1, HTTP/3, WebTransport and MoQ as libraries (not yet wired into the node)
  ／ QUIC v1・HTTP/3・WebTransport・MoQ の部品（ノードへの組み込みはこれから）

### Not verified yet / まだ確かめていないこと

- GPU codecs on real hardware, capture hardware, iOS/Android sender apps
  ／ 実機の GPU のコーデック、キャプチャ機器、iOS・Android の送り手のアプリ
- Browsers other than Chrome, TURN paths ／ Chrome 以外のブラウザー、TURN の経路
- Room cascading across nodes, auth webhook for rooms in the node
  ／ ノードをまたぐ部屋のカスケード、ノードでの部屋の認証 Webhook
- Real cloud PaaS accounts (tested against mocks only) ／ 実際のクラウドの PaaS（模擬の API でのみ確認）
