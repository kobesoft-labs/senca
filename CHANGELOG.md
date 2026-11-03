# Changelog / 変更履歴

## Unreleased / 未公開

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
  ／ drain 中のノードは新しい RTMP・SRT・WHIP の送り手を拒否します（WHIP は drain 中でないノードへ 307）。接続済みの送り手は継続します
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
