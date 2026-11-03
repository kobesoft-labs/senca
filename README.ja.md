# SENCA

[English](README.md) | 日本語

SENCA は、ライブ配信・録画・リアルタイム通信のためのメディアサーバーです（Rust 製）。1 台のノードでも、
複数のノードをまとめたグリッドでも動作します。グリッドでは、ノードの資源に応じてワークロードを配置し、
ノードに障害が起きても動作を続けます。

> **バージョン: v0.0.1。** リリースの手順と、リリース前に行う確認は [docs/dev/RELEASE.md](docs/dev/RELEASE.md) にあります。

## 主な機能

- **取り込み（インジェスト）**: RTMP/RTMPS（Enhanced RTMP: AV1・VP9・H.265）、SRT（listener・caller、AES 暗号化）、
  WHIP（WebRTC）、RTSP pull、RTMP・SRT・HLS の pull、キャプチャデバイス
- **配信**: HLS（TS・fMP4・LL-HLS）、RTMP・SRT・RTSP の play、WHEP、署名付きの再生 URL、ノード間の中継（ミラー）
- **録画**: ライブ配信と並行した録画、障害後の録画の復旧、VOD、MP4 の書き出し、予約録画、保持期間
- **加工**: パイプライン（デコード・合成・板書補正・音声ミキサー・エンコード）、配信制作の操作（シーン・トランジション）、
  静止画・サムネイル、字幕と翻訳（WebVTT）
- **外部への送信**: RTMP・SRT の push、PaaS（クラウドの配信サービス）との連携
- **リアルタイムの部屋**（ADR-0015）: SFU、サイマルキャスト、スポットライト、転送フィルター、DataChannel、
  参加トークン（Ed25519）、SSE によるイベント通知、E2EE のパススルー、ブラウザー用 SDK（`apps/sdk-js`）
- **メディアパイプライン**（ADR-0016）: 部屋の参加者をストリームとして書き出し、合成、録画、RTMP での送信、文字起こし
- **グリッド**（ADR-0013）: ノードの資源（CPU・メモリ・帯域・GPU・キャプチャデバイス）を集計してワークロードを配置します。
  ノードや GPU の手動での割り当て、優先度、ノードやデバイスが停止したときの移動と段階的な縮退、復旧したときの再配置に対応します
- **クラスター**: 独自実装の Raft による制御プレーンの複製、mTLS、ストリームの所有権と fencing、ノード間の中継（ADR-0014）
- **管理**: REST API（OpenAPI: [docs/api/openapi.yaml](docs/api/openapi.yaml)）、SSE、管理画面（`apps/console`）、
  TypeScript・Rust の API クライアント（`apps/sdk-js`・`crates/senca-client`）
- **QUIC**（`--quic`）: HTTP/3 での管理 API・署名付き HLS、MoQ（draft-22、ネイティブ QUIC と WebTransport）による
  ライブの購読（hang の catalog と CMAF）と CMAF の配信

## 特許とコーデック

H.264/H.265 のエンコード・デコードは、GPU メーカーの SDK（NVIDIA NVENC/NVDEC、Intel oneVPL、AMD AMF、VA-API）を
実行時に読み込んで行います。処理は隔離された codec worker の中で動きます。H.264/H.265/AAC のソフトウェアコーデックは
同梱していません。GPU がない環境では、これらの変換は `503 CAPACITY_UNAVAILABLE` になります。受信した H.264/AAC を
そのまま配信・録画することは、GPU がなくてもできます。

ロイヤリティフリーのコーデック（AV1: dav1d・SVT-AV1、VP8/VP9: libvpx、Opus: libopus）はソフトウェアで処理します。
コンテンツの配信にともなうライセンス料の扱いは、利用者ご自身で確認してください。
詳しくは [ADR-0004](docs/dev/adr/0004-codecs-patents.md) を参照してください。

## はじめかた

```sh
cargo build --release -p senca-node -p senca-codec-worker
./target/release/senca-node --data-dir ./data --http 127.0.0.1:8080 --rtmp 127.0.0.1:1935 \
  --webrtc-udp 127.0.0.1:8443
cat ./data/control/bootstrap-admin-token   # 管理 API のトークン
```

- コンテナで動かす: `Dockerfile`・`compose.yaml`（[ADR-0008](docs/dev/adr/0008-container-deployment.md)）
- クラスターで動かす: `--cluster-config`（[ADR-0009](docs/dev/adr/0009-cluster.md)・[ADR-0013](docs/dev/adr/0013-grid.md)）

## ドキュメント

**利用ガイド: [docs/guide/ja](docs/guide/ja/README.md)**（導入・設定・配信・録画・グリッド・部屋・メディアパイプライン・API と SDK）

| | |
|---|---|
| API | [docs/api/openapi.yaml](docs/api/openapi.yaml)、部屋のシグナリング [docs/realtime/signaling.md](docs/realtime/signaling.md) |
| SDK | [apps/sdk-js](apps/sdk-js/README.md)・[crates/senca-client](crates/senca-client/README.md) |
| 仕様・要件 | [docs/dev/spec](docs/dev/spec)・[docs/dev/requirements](docs/dev/requirements) |
| 設計の決定記録（ADR） | [docs/dev/adr](docs/dev/adr) |
| 開発の状況 | [docs/dev/ROADMAP.md](docs/dev/ROADMAP.md)、適合台帳 [docs/dev/conformance](docs/dev/conformance) |
| 開発者向け | [docs/dev/DEVELOPMENT.md](docs/dev/DEVELOPMENT.md)、リリース [docs/dev/RELEASE.md](docs/dev/RELEASE.md)、依存ライブラリのライセンス [docs/licenses](docs/licenses) |

## ライセンス

[Apache License 2.0](LICENSE) です。[NOTICE](NOTICE) もあわせて参照してください。
