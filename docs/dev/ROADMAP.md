# ロードマップ

仕様書のフェーズ（P0〜P7）と追加要件（REQ-001〜010）を、検証できる大きさのマイルストーンに並べたもの。
2026-11-02 に 1 から作り直した（H.264/H.265 はハードウェアのコーデックだけ。ADR-0004）。

| # | 内容 | 仕様・要件 | 状態 |
|---|---|---|---|
| M1 | RTMP で受けて HLS で配信（単一ノード） | P1・P2 の一部 | 完了（2026-11-02） |
| M2 | 録画（アーカイブ）と配信の分岐、VOD 再生、障害後の回収、MP4 書き出し | 5.3、8.4、REQ-006 R6-1 | 完了（2026-11-02） |
| M3 | 制御 API（stream・録画・予約・トークン、認証、Idempotency、OpenAPI） | 10 章、REQ-002 | 完了（2026-11-02） |
| M4 | codec worker（隔離・プール）、ハードウェアのコーデック（NVENC/NVDEC・oneVPL・AMF・VA-API）、ロイヤリティフリーのコーデック、静止画・サムネイル | 2.1、3.1、R6-8/9、REQ-004、REQ-007、REQ-010 | 完了（GPU の実機は未検証）（2026-11-02） |
| M5 | 板書補正、複数チャネル・合成、制作機能（シーン・切り替え・音声ミキサー） | R6-5/6/7、REQ-005、REQ-008 | 完了（2026-11-02。H.264/H.265 の経路は GPU の実機で未検証） |
| M6 | 字幕と翻訳（WebVTT） | R6-4 | 完了（2026-11-02。録画への字幕の保存・WebRTC の DataChannel は未） |
| M7 | 入力の追加（SRT、RTSP pull、RTP、TS demux、HLS pull） | P2 | 完了（2026-11-02。RTSP pull は TCP interleaved） |
| M8 | WebRTC（ICE/STUN/TURN、DTLS-SRTP、SDP、WHIP/WHEP、SFU）、ブラウザーからの送出 | P3、R6-3、REQ-008 | 完了（2026-11-02。Chrome で確認。Firefox・Safari・TURN 経路・独立監査は未） |
| M9 | 分散（自前 Raft、配置、fencing、振り直し、ノードの追加） | P4、REQ-005、REQ-010 | 完了（2026-11-02。制御面の複製・mTLS・所有権と fencing・所有者への案内。中継の木と共有ストレージは未） |
| M10 | QUIC・TLS 1.3・HTTP/3・WebTransport・MoQ | P5・P6 | ノードに組み込み済み（2026-11-03、`--quic`。部品は aioquic・OpenSSL・Chrome・moq-dev と 43/43、ノードは `tools/interop/node_quic.sh` 34/34: HTTP/3 の HLS・API を aioquic・Chrome、MoQ の購読を moq-dev・Chrome で）。MoQ の送出は CMAF だけで、独立実装の送り手（moq-dev の `legacy` 容器）とは未接続。クラスタの写しからの MoQ の配信は試験していない |
| M11 | H.265・AV1/VP9/Opus の経路、fMP4、LL-HLS、Enhanced RTMP | P6、REQ-007 | 完了（2026-11-02。LL-HLS は Apple の検証器・Safari で未確認） |
| M12 | 管理画面（GUI、パイプライン編集、ノード管理、制作画面） | REQ-002、REQ-005、REQ-008 | 済み（試験 74/74） |
| M13 | Docker 導入、キャプチャボード、PaaS 連携 | REQ-003/004/001 | 完了（2026-11-02。キャプチャ機器・GPU は実機で未検証、PaaS は模擬の API でのみ確認） |
| M15 | 送り手のアプリ（ブラウザー WHIP、iOS/iPadOS/Android/macOS、senca-sdk、QR で設定、カメラ制御） | REQ-009 | 完了（2026-11-02。SDK・ブラウザーの WHIP は確認済み。Android・Apple のアプリはこの環境でビルドしていない） |
| M14 | 本番の仕上げ（長時間試験、fuzz、SBOM、署名付き配布） | 13 章、15 章 | |
| M16 | グリッド（資源を数えた配置、手動の割り当て、停止での移動・縮退・復帰）とノード間の中継 | ADR-0013・0014、REQ-005、REQ-010 | 完了（2026-11-03。分散の試験 47/47、1 台のグリッド 11/11、配置係のシミュレーション 5,000 種。GPU の実機は未検証） |
| M17 | リアルタイムの部屋（SFU・サイマルキャスト・スポットライト・転送フィルター・DataChannel・Webhook・E2EE の素通し） | ADR-0015 | 完了（2026-11-03。Chrome 3 台 22/22、ノードの停止で部屋が移る。ノードをまたぐカスケード・認証 Webhook・イベント Webhook も済み: `tools/interop/cluster_rooms.sh` 30/30） |
| M18 | メディアパイプライン（AWS Chime SDK Media Pipelines 相当: 書き出し・合成・録画・送り出し・文字起こし） | ADR-0016 | 完了（2026-11-03。Chrome 2 台 17/17。文字起こしは stream の字幕の仕組みで、部屋での試験は未） |
| M19 | SDK（ブラウザーの部屋の SDK、TypeScript・Rust の API クライアント） | ADR-0015 | 完了（2026-11-03。Chrome 3 台で 33/33。Firefox・Safari は未） |
| M20 | 利用者のアカウント（パスワード・TOTP・回復コード）、ログインのセッション、OIDC のシングルサインオン（バックエンド） | ADR-0017、REQ-002 | 完了（2026-11-04。`tools/interop/auth.sh` 155/155、試験用の IdP と 3 台のクラスタ。ログインの画面は管理画面の側で未） |

各マイルストーンの完了は「独立実装との相互接続の記録」「適合台帳の更新」「文書の更新」「コミット」をそろえたときとする。
