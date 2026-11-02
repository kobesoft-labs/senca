# 追加要件

仕様書 SENCA-SRS-001 v1.0 に対し、利用者の指示で追加した要件。v1.1 で本文に取り込む。

| ID | 内容 |
|---|---|
| [REQ-001](REQ-001-cloud-paas-integration.md) | クラウドのメディア PaaS（Cloudflare・AWS）との連携を制御面から |
| [REQ-002](REQ-002-control-api-and-console.md) | 予定実行などの API 追加、パイプライン・ノード管理の GUI |
| [REQ-003](REQ-003-docker-deployment.md) | Docker だけで立ち上がる導入 |
| [REQ-004](REQ-004-hardware-acceleration.md) | NVENC・AMD・Intel・DeckLink などハードウェアへの対応 |
| [REQ-005](REQ-005-pipeline-builder-and-failover.md) | GUI のパイプライン構築、ロジックと計算資源の分離、ノード障害時の継続 |
| [REQ-006](REQ-006-camera-archive-live-and-classroom.md) | カメラ RTMP のアーカイブ＋配信（最優先）、1 対 1／1 対多、字幕と翻訳、黒板・ホワイトボード補正、静止画・サムネイル |
| [REQ-007](REQ-007-royalty-free-profile.md) | ロイヤリティフリー（AV1・VP9・VP8・Opus・WebM 等）への対応と「ロイヤリティフリーのみ」プロファイル |
| [REQ-008](REQ-008-production-switcher.md) | OBS のような制作機能（シーン・ソース・切り替え・音声ミキサー・フィルター・出力）、ブラウザからの送出、Web のスイッチャーとカメラ操作盤 |
| [REQ-009](REQ-009-sender-apps.md) | 送り手のアプリ（Web カメラ・PC/Mac・スマホ・タブレット）、エンドポイントを入れれば使える標準準拠（WHIP/SRT/RTMPS）、カメラ制御（VISCA over IP・ONVIF）、深度・GPS 等のセンサーと前後カメラの同時送出 |
| [REQ-010](REQ-010-performance-and-scale.md) | 性能とスケール: マルチスレッド・マルチ CPU・codec worker のプール・GPU、ノードを足して指定の規模まで速くする（必須） |
