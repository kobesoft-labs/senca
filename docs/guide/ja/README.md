# SENCA 利用者ガイド

[English](../en/README.md) | 日本語

SENCA（ライブ配信・録画・リアルタイム通信のためのメディアサーバー）を動かし、使い、運用するための手引き。
版は v0.0.0（実験的）。本番には使わないこと（[1. 概要](01-overview.md)）。

## 目次

| # | ページ | 内容 |
|---|---|---|
| 1 | [概要](01-overview.md) | SENCA とは何か、構成（制御面・hub・packager・recorder・codec worker・グリッド・中継・部屋・メディアパイプライン）、機能の一覧、状態 |
| 2 | [クイックスタート](02-quickstart.md) | ソースからのビルド、単一ノードの起動、stream の作成、RTMP での送出、HLS での再生、Docker |
| 3 | [設定](03-configuration.md) | すべてのオプション、設定ファイル（JSON）、クラスタの設定、環境変数、データ置き場 |
| 4 | [取り込みと再生](04-ingest-playback.md) | RTMP・SRT・WHIP での受信、HLS・LL-HLS・RTMP play・WHEP・RTSP での再生 |
| 5 | [録画](05-recording.md) | 録画、VOD、MP4 の書き出し、予約、保持期間 |
| 6 | [入力と出力](06-inputs-outputs.md) | pull 入力、push 出力、キャプチャ機器、PaaS 連携、秘密値 |
| 7 | [パイプライン](07-pipelines.md) | 復号・合成・板書補正・音声ミキサー・符号化、制作の操作 |
| 8 | [字幕と翻訳](08-captions.md) | 音声認識・翻訳と WebVTT の HLS 字幕 |
| 9 | [クラスタとグリッド](09-cluster-grid.md) | Raft の制御面、mTLS、所有権、グリッドの配置と縮退、中継 |
| 10 | [リアルタイムの部屋](10-rooms.md) | リアルタイムの部屋（SFU）、参加トークン、シグナリング |
| 11 | [メディアパイプライン](11-media-pipelines.md) | 部屋の書き出し・合成・録画・送り出し・文字起こし |
| 12 | [セキュリティ](12-security.md) | トークンと権限、JWT、stream key、署名付き URL、秘密値、外へ出る接続の制限、mTLS、サンドボックス |
| 13 | [運用](13-operations.md) | ヘルスチェック、ログ、監視、イベント、回収、バックアップ、更新、排出、終了、容量 |
| 14 | [コーデックと特許](14-codecs-and-patents.md) | ハードウェアとロイヤリティフリーのコーデック、特許の方針 |
| 15 | [管理 API と SDK](15-api-and-sdks.md) | REST の約束、OpenAPI、SSE、TypeScript・Rust のクライアント |

## 関係する文書

| | |
|---|---|
| 設計の決定 | [docs/dev/adr](../../dev/adr) |
| API の定義 | [docs/api/openapi.yaml](../../api/openapi.yaml)、部屋のシグナリング [docs/realtime/signaling.md](../../realtime/signaling.md) |
| 進み具合 | [docs/dev/ROADMAP.md](../../dev/ROADMAP.md)、適合台帳 [docs/dev/conformance/ledger.md](../../dev/conformance/ledger.md) |
| 開発・リリース | [docs/dev/DEVELOPMENT.md](../../dev/DEVELOPMENT.md)、[docs/dev/RELEASE.md](../../dev/RELEASE.md) |
