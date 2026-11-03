# SENCA

[English](README.md) | 日本語

ライブ配信・録画・リアルタイム通信のためのメディアサーバー（Rust）。1 台でも、何台ものノードを束ねたグリッドでも動く。

> **状態: v0.0.0（実験的）。** 機能は揃ってきたが、出荷 gate（24 時間の fuzz、72 時間の連続試験、暗号・認可の独立レビュー、
> 署名付きの配布）をまだ通していない。本番には使わないこと。手順は [docs/RELEASE.md](docs/RELEASE.md)。

## できること

- **受ける**: RTMP/RTMPS（Enhanced RTMP: AV1・VP9・H.265）、SRT（listener・caller、AES）、WHIP（WebRTC）、RTSP pull、
  RTMP・SRT・HLS の pull、キャプチャ機器
- **配る**: HLS（TS・fMP4・LL-HLS）、RTMP・SRT・RTSP の play、WHEP、署名付きの再生 URL、ノード間の中継（写し）
- **録る**: 録画と配信の分岐、障害からの回収、VOD、MP4 の書き出し、予約、保持期間
- **加工する**: パイプライン（復号・合成・板書補正・音声ミキサー・符号化）、制作の操作（シーン・トランジション）、
  静止画・サムネイル、字幕と翻訳（WebVTT）
- **送り出す**: RTMP・SRT の push、PaaS 連携（クラウドの配信サービス）
- **リアルタイムの部屋**（ADR-0015）: SFU、サイマルキャスト、スポットライト、転送フィルター、DataChannel、
  参加トークン（Ed25519）、出来事の SSE、E2EE の素通し、ブラウザー用 SDK（`apps/sdk-js`）
- **メディアパイプライン**（ADR-0016）: 部屋の参加者を stream に書き出す、合成、録画、RTMP への送り出し、文字起こし
- **グリッド**（ADR-0013）: ノードの資源（CPU・メモリ・帯域・GPU・キャプチャ機器）を数えて仕事を置く。ノード・GPU の
  手動の割り当て、優先度、ノード・装置が止まったときの移動と段階的な縮退、戻ったときの復帰
- **分散**: 自前の Raft で制御面を複製、mTLS、所有権と fencing、ノード間の中継（ADR-0014）
- **管理**: REST API（OpenAPI: [docs/api/openapi.yaml](docs/api/openapi.yaml)）、SSE、管理画面（`apps/console`）、
  TypeScript・Rust の API クライアント（`apps/sdk-js`・`crates/senca-client`）
- **部品**: QUIC v1・HTTP/3・WebTransport・MoQ（draft-22）。ノードへの組み込みはこれから

## 特許とコーデック

H.264/H.265 の符号化・復号は、GPU の製造元の SDK（NVIDIA NVENC/NVDEC、Intel oneVPL、AMD AMF、VA-API）を
実行時に読み込んで行う（隔離した codec worker の中）。ソフトウェアの H.264/H.265/AAC のコーデックは同梱しない。
GPU が無ければ、その変換は `503 CAPACITY_UNAVAILABLE` になる（受けた H.264/AAC をそのまま配る・録るのはできる）。
ロイヤリティフリーのコーデック（AV1: dav1d・SVT-AV1、VP8/VP9: libvpx、Opus: libopus）はソフトウェアで扱う。
コンテンツの配信に伴う使用料の扱いは、利用者が自分で確かめること。詳しくは [ADR-0004](docs/adr/0004-codecs-patents.md)。

## 使ってみる

```sh
cargo build --release -p senca-node -p senca-codec-worker
./target/release/senca-node --data-dir ./data --http 127.0.0.1:8080 --rtmp 127.0.0.1:1935 \
  --webrtc-udp 127.0.0.1:8443
cat ./data/control/bootstrap-admin-token   # 管理 API のトークン
```

- コンテナ: `Dockerfile`・`compose.yaml`（[ADR-0008](docs/adr/0008-container-deployment.md)）
- 分散: `--cluster-config`（[ADR-0009](docs/adr/0009-cluster.md)・[ADR-0013](docs/adr/0013-grid.md)）

## 文書

**利用の手引き: [docs/guide/ja](docs/guide/ja/README.md)**（導入・設定・配信・録画・グリッド・部屋・メディアパイプライン・API と SDK）

| | |
|---|---|
| 仕様・要件 | [docs/spec](docs/spec)・[docs/requirements](docs/requirements) |
| 設計の決定 | [docs/adr](docs/adr) |
| API | [docs/api/openapi.yaml](docs/api/openapi.yaml)、部屋のシグナリング [docs/realtime/signaling.md](docs/realtime/signaling.md) |
| 進み具合 | [docs/ROADMAP.md](docs/ROADMAP.md)、適合台帳 [docs/conformance](docs/conformance) |
| 開発 | [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)、リリース [docs/RELEASE.md](docs/RELEASE.md)、依存のライセンス [docs/licenses](docs/licenses) |

## ライセンス

[Apache License 2.0](LICENSE)。[NOTICE](NOTICE) も参照。
