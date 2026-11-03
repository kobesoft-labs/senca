# ADR-0001 全体構成

- 状態: 採用（2026-11-02）

## 決定

- Rust のモノレポ（Cargo workspace）。製品・SDK・管理画面・送り手アプリ・試験道具を 1 つのリポジトリに置く。
- **sans-io の crate** と **I/O を持つ実行ファイル** に分ける。プロトコルの状態機械はランタイムから独立して単体試験できる（仕様 3.2）。
- 非同期ランタイムは tokio（マルチスレッド）。stream・接続ごとにタスクを持ち、キューはすべて有界（仕様 3.2、REQ-010 R10-1）。
- 単一ノードでは `senca-node` 1 つの実行ファイルに、gateway・media・origin・control を同居させる（仕様 3.1）。codec worker は別プロセス（`senca-codec-worker`）。
- ノード内の部品はメモリ・パイプ・共有メモリでつなぐ。ループバックの TCP や外部ブローカーを通さない（REQ-010 R10-8）。

## crate の一覧（増えるたびに追記）

| crate | 役割 |
|---|---|
| senca-core | 共通メディアモデル、ID、時刻、ビット読み書き |
| senca-h264 / senca-h265 / senca-aac / senca-av1 / senca-opus | ビットストリームの解析と形式変換 |
| senca-amf0 / senca-flv / senca-rtmp | RTMP |
| senca-mpegts | MPEG-TS の多重化・分離 |
| senca-fmp4 | ISO BMFF / fMP4 |
| senca-hls | HLS のプレイリストとセグメント分割 |
| senca-http | 厳格な HTTP/1.1 |
| senca-record | 録画（不変セグメント＋索引） |
| senca-control | 制御状態（決定的な状態機械）・認証・予約の計算 |
| senca-codec / senca-codec-worker | コーデックの抽象と提供者（GPU ベンダー・ロイヤリティフリー）、隔離した worker プロセス |
| senca-image / senca-board / senca-compose | 画像処理、板書補正、合成・音声ミキサー |
| senca-stun / senca-ice / senca-turn | STUN、ICE（full・lite、trickle、consent）、TURN のサーバーとクライアント |
| senca-srt | SRT（HSv5、caller/listener、ARQ、TSBPD、AES-CTR/GCM、鍵の更新） |
| senca-rtp / senca-sdp / senca-rtsp | RTP/RTCP と各コーデックのペイロード形式、SDP と offer/answer、RTSP 1.0 のクライアントとサーバー |
| senca-tls | TLS 1.3（クライアント・サーバー、mTLS、チケット、QUIC 向けの使い方） |
| senca-dtls / senca-srtp | DTLS 1.2（WebRTC 用）と SRTP/SRTCP（AES-CM・AEAD-GCM） |
| senca-sctp / senca-webrtc | SCTP（DataChannel）と WebRTC のピア接続（ICE・DTLS-SRTP・NACK/RTX・帯域推定・サイマルキャスト）、SFU |
| senca-raft | 自前の Raft（PreVote・CheckQuorum・joint consensus・ReadIndex・スナップショット、ファイルの保存） |
| senca-pipeline | パイプライン（DAG）の検証・世代・実行（復号・板書補正・合成・ミキサー・符号化）。codec worker の中で動く |
| senca-paas | クラウドの PaaS（Cloudflare Stream・Amazon IVS・MediaLive）の要求の組み立てと応答の解析、SigV4、後片付けの判断 |
| senca-caption | 字幕（復号しない音声の切り出し、音声認識・翻訳のアダプター、WebVTT・HLS 字幕） |
| senca-sdk | 送り手の SDK（C ABI、RTMP・SRT、再接続、senca: の URI と QR、VISCA over IP） |
| senca-devtools | 開発・試験の道具（配布しない） |
| senca-node | 実行ファイル |
