# REQ-003 Docker だけで立ち上がる導入

- 追加日: 2026-10-26（利用者の指示）
- 状態: 要件として受理。

## 要件

最終的に、Docker（と Docker Compose）を入れたホストで、1 つのコマンドで SENCA 一式が立ち上がること。

```bash
docker compose up -d
```

## 範囲

| 構成 | Docker だけで動くか |
|---|---|
| 単一ノード（制御・メディア・管理画面を同居） | 動く。既定の構成 |
| 複数ノード（制御 3 台 + メディア n 台） | 動く。ノード登録トークンで参加 |
| GPU エンコード（NVENC） | ホストに NVIDIA ドライバーと NVIDIA Container Toolkit が必要 |
| GPU エンコード（AMD AMF / VAAPI、Intel QSV） | ホストにドライバーが必要。`/dev/dri` などをコンテナに渡す |
| Blackmagic DeckLink などのキャプチャボード | ホストに Desktop Video ドライバーが必要。デバイスをコンテナに渡す |

ハードウェアを使う構成は「ドライバーを入れてから `docker compose up`」とし、導入手順と検出結果（`GET /devices`）で足りないものを示す。

## 決まり

- イメージは署名付き・SBOM 付きで配布する（仕様 9.5）。
- 秘密値は環境変数に直接書かず、Docker secrets またはファイル参照で渡す。
- 初回起動で管理者の初期化を行い、既定パスワードを持たない。
- データ（合意ログ・録画・設定）は named volume に置き、バックアップ手順を用意する。
- ネットワーク: UDP（WebRTC/SRT/QUIC）のポート範囲を compose で明示する。
