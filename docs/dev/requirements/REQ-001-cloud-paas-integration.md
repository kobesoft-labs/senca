# REQ-001 クラウドのメディア PaaS との連携

- 追加日: 2026-10-26（利用者の指示）
- 状態: 要件として受理。着手は仕様書 v1.0 の本体要件の後。
- 仕様書との関係: SENCA-SRS-001 に対する追加要件。v1.1 で本文へ取り込む。

## 要件

管理 API（senca-control）から、外部のクラウド PaaS との連携を設定・操作できること。

| 連携先（初期候補） | 主な使い方 | つなぎ方 |
|---|---|---|
| Cloudflare Stream | ライブ入力への送出、録画・VOD の受け取り | RTMPS / SRT / WHIP で送る。Stream API で Live Input を作成・削除 |
| Cloudflare Calls（Realtime） | WebRTC SFU との相互接続 | WHIP/WHEP。API でセッション管理 |
| AWS Elemental MediaLive | 放送品質のエンコード・ABR へ送る | RTMP push / SRT / RTP。API で Input を作成 |
| AWS Elemental MediaConnect | 拠点間の伝送 | SRT / Zixi 以外の標準方式。API で Flow を作成 |
| AWS Elemental MediaPackage | パッケージング・オリジン | HLS（HTTP PUT / WebDAV）で送る |
| Amazon IVS | 低遅延の大規模視聴 | RTMPS で送る。API で Channel を作成 |

「aws mediapipe」の指示は、上記 AWS のメディア系サービスと解釈した（MediaPipe は Google の機械学習フレームワークで別物）。要確認。

## 設計の方針

1. **連携先は出入口として扱う。** SENCA の配送の中核を外部に置き換えない（仕様 2.1「他社製メディアサーバーを包む構成を最終形としない」）。外部へ送る・外部から受ける出力・入力の一種にする。
2. **2 層に分ける。**
   - 送受信: SENCA 自前のプロトコル実装（RTMP/RTMPS・SRT・WHIP・HLS）でつなぐ。
   - 制御: 各社の API を呼ぶアダプター（`PaasProvider`）。受け口の作成・削除・状態取得・鍵の更新。
3. **管理 API の形。** `POST /streams/{id}/outputs` と `POST /streams/{id}/inputs` に `protocol: "paas"` と `provider`、`provider_resource`（既存のものを使う／SENCA が作る）を足す。認証情報は secret reference で渡す（仕様 10.2）。
4. **作ったものは SENCA が片付ける。** SENCA が作成した外部リソースは、所有権（owner/epoch/fencing、仕様 8.2）と結び付け、stream の削除で後始末する。二重作成は Idempotency-Key と外部側のタグで防ぐ。
5. **能力は capabilities で返す。** 連携先ごとの対応範囲（プロトコル・コーデック・遅延）を `GET /capabilities` に出し、変換計画（仕様 7 章）で無理な組み合わせを拒否する。
6. **秘密情報。** API トークン・stream key はログ・エラー・監視ラベルに出さない（仕様 6.7・9.3）。

## 受け入れ試験（案）

| ID | 内容 | 合格条件 |
|---|---|---|
| AC-P01 | 管理 API から Cloudflare Stream の Live Input を作り、SENCA から RTMPS で送る | 再生でき、stream 削除で Live Input も消える |
| AC-P02 | 管理 API から MediaLive の Input を作り、SRT または RTMP で送る | MediaLive 側で入力を検出、削除で後始末 |
| AC-P03 | 外部 API の失敗・タイムアウト・二重要求 | 部分的に作られたリソースを残さない。リトライで二重作成しない |
| AC-P04 | 秘密情報の漏れ | ログ・イベント・エラーに token / stream key が出ない |
