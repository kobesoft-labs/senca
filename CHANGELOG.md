# Changelog / 変更履歴

## Unreleased / 未リリース

### Added / 追加

- Recording storage on S3-compatible object storage (AWS S3, MinIO, Cloudflare R2): recordings are written locally first and
  uploaded once immutable; the index carries object keys; local copies are removed after `--recording-keep-local`
  (`--recording-storage`, `--s3-endpoint`, `--s3-region`, `--s3-addressing`, `--s3-credentials`, `--s3-part-size`)
  ／ 録画の保存先に S3 互換のオブジェクトストレージ（AWS S3・MinIO・Cloudflare R2）: まず手元に書き、変わらなくなったものから上げる。
  索引にオブジェクトのキーを記録し、`--recording-keep-local` の後に手元の写しを消す
- VOD (including `Range`), MP4 export and thumbnails are read from storage when the local copy is gone; the first export is
  stored and reused (multipart upload for large objects)
  ／ 手元に無い録画の VOD（`Range` を含む）・MP4 の書き出し・サムネイルを保存先から読む。最初の書き出しを保存先に置いて使い回す
  （大きなものはマルチパートアップロード）
- Recordings carry `storage` (`{uri, archived_at}`) in the API and the control state
  ／ 録画の API と制御状態に `storage`（`{uri, archived_at}`）
- `senca-s3` crate: SigV4 signing, request layout and bounded XML parsing (sans-io), tested with the AWS SigV4 test vectors
  ／ `senca-s3`: SigV4 の署名・要求の組み立て・上限付きの XML の読み手（sans-io）。AWS の SigV4 の試験の値で確認
- `tools/interop/s3.sh`: end-to-end test against an S3-compatible server in Docker
  ／ `tools/interop/s3.sh`: Docker の S3 互換のサーバーを相手にした通しの試験

### Changed / 変更

- In a cluster, any node configured with the same storage serves stored recordings (VOD, export, thumbnail) itself; before,
  export and thumbnails only worked on the node that made the recording
  ／ クラスタでは、同じ保存先を設定したどのノードでも保存先の録画（VOD・書き出し・サムネイル）を出す。前は書き出し・サムネイルは
  録画を作ったノードでしか動かなかった
- Deleting a recording (API or retention) removes its stored objects; in a cluster the node that made the recording performs the
  deletion, whichever node received the request, and retries when storage is unreachable
  ／ 録画の削除（API・保持期限）で保存先のオブジェクトも消す。クラスタでは、要求を受けたノードに関わらず録画を作ったノードが消し、
  保存先に届かなければやり直す

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
