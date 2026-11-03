# SENCA 利用者ガイド

[English](../en/README.md) | 日本語

このガイドでは、SENCA（ライブ配信・録画・リアルタイム通信のためのメディアサーバー）の導入、使い方、運用の方法を説明します。
対象のバージョンは v0.0.0 です。

はじめて使う方は、[1. 概要](01-overview.md) で全体の構成をつかみ、[2. クイックスタート](02-quickstart.md) で実際に動かしてみてください。

## 目次

| # | ページ | 内容 |
|---|---|---|
| 1 | [概要](01-overview.md) | SENCA とは何か、構成（制御プレーン・hub・packager・recorder・codec worker・グリッド・中継・部屋・メディアパイプライン）、機能の一覧、動作確認の状況 |
| 2 | [クイックスタート](02-quickstart.md) | ソースからのビルド、単一ノードの起動、ストリームの作成、RTMP での配信、HLS での再生、Docker |
| 3 | [設定](03-configuration.md) | すべてのオプション、設定ファイル（JSON）、クラスターの設定、環境変数、データディレクトリ |
| 4 | [取り込みと再生](04-ingest-playback.md) | RTMP・SRT・WHIP での取り込み、HLS・LL-HLS・RTMP play・WHEP・RTSP での再生 |
| 5 | [録画](05-recording.md) | 録画、VOD、MP4 の書き出し、予約録画、保持期間 |
| 6 | [入力と出力](06-inputs-outputs.md) | pull 入力、push 出力、キャプチャデバイス、PaaS 連携、秘密値 |
| 7 | [パイプライン](07-pipelines.md) | デコード・合成・板書補正・音声ミキサー・エンコード、配信制作の操作 |
| 8 | [字幕と翻訳](08-captions.md) | 音声認識・翻訳と、WebVTT による HLS の字幕 |
| 9 | [クラスターとグリッド](09-cluster-grid.md) | Raft による制御プレーン、mTLS、所有権、グリッドの配置と縮退、中継 |
| 10 | [リアルタイムの部屋](10-rooms.md) | リアルタイムの部屋（SFU）、参加トークン、シグナリング |
| 11 | [メディアパイプライン](11-media-pipelines.md) | 部屋の書き出し・合成・録画・外部への送信・文字起こし |
| 12 | [セキュリティ](12-security.md) | トークンと権限、JWT、ストリームキー、署名付き URL、秘密値、外部への接続の制限、mTLS、サンドボックス |
| 13 | [運用](13-operations.md) | ヘルスチェック、ログ、監視、イベント、障害からの復旧、バックアップ、更新、drain、停止、容量 |
| 14 | [コーデックと特許](14-codecs-and-patents.md) | ハードウェアコーデックとロイヤリティフリーのコーデック、特許への対応方針 |
| 15 | [管理 API と SDK](15-api-and-sdks.md) | REST API の規約、OpenAPI、SSE、TypeScript・Rust のクライアント |

## 関連ドキュメント

| | |
|---|---|
| 設計の決定記録（ADR） | [docs/dev/adr](../../dev/adr) |
| API の定義 | [docs/api/openapi.yaml](../../api/openapi.yaml)、部屋のシグナリング [docs/realtime/signaling.md](../../realtime/signaling.md) |
| 開発の状況 | [docs/dev/ROADMAP.md](../../dev/ROADMAP.md)、適合台帳 [docs/dev/conformance/ledger.md](../../dev/conformance/ledger.md) |
| 開発・リリース | [docs/dev/DEVELOPMENT.md](../../dev/DEVELOPMENT.md)、[docs/dev/RELEASE.md](../../dev/RELEASE.md) |
