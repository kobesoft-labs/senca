# ADR-0015 リアルタイムの部屋

- 状態: 採用（2026-11-03）
- 関係: 仕様 6.3、R6-3、REQ-008、ADR-0012、ADR-0013

## 機能

| 求められる機能 | SENCA |
|---|---|
| チャネル、role（sendrecv/sendonly/recvonly） | 部屋（room）、参加の役割 |
| マルチストリーム、サイマルキャスト、スポットライト | 同じ（スポットライトは話者の検出で数を絞る） |
| シグナリング（WebSocket、DataChannel への切り替え） | 同じ（JSON の版付きのプロトコル） |
| 認証 Webhook、セッション・イベント Webhook | 同じ（署名つき、再送、冪等の ID） |
| メッセージング（DataChannel のラベル）、プッシュ API | 同じ（部屋・参加者へ送る API） |
| 録画（参加者ごと・合成） | メディアパイプライン（ADR-0016） |
| 統計（getStats 相当） | 参加者・部屋の統計の API とイベント |
| 転送フィルター | 同じ（規則で購読する相手・トラックを絞る） |
| メタデータ、client_id、bundle_id | 同じ |
| E2EE | 素通し（SFrame。サーバーは中身を見ない。合成・録画の合成はできないと計画で理由を返す） |
| クラスター（リダイレクト） | グリッド（ADR-0013）で部屋を複数のノードに分け、SFU をカスケードする |
| 音声ストリーミング（文字起こしへ） | メディアパイプライン（文字起こし） |

## API の設計の方針

- REST は資源の形（`/rooms`・`/rooms/{id}/participants`・`/rooms/{id}/messages`）で、版・ETag・Idempotency・cursor・problem+json を他の API とそろえる。「操作名の POST」型にはしない。
- イベントは SSE（`/api/v1/events`）と Webhook の両方に同じ形で出す（型付き、event_id で重複排除）。
- 参加の資格は短命の**参加トークン**（JWT 互換、部屋・役割・許可するトラック・期限）。認証 Webhook はその代わりにも併用にもできる。
- ブラウザーの SDK は Promise と非同期イテレーターの API、型定義つき。再接続・ICE restart・サイマルキャストの切り替え・購読の選択・統計を SDK が持つ。

## 実装の状況（2026-11-03）

- senca-node: 部屋はグリッドのワークロードとして 1 つのノード（home）に置く。部屋の設定 `cascade`（またはノードの
  `--rooms-cascade`）が偽なら、別のノードにつないだ参加者は `redirect` で home へ案内する。
- カスケード（決定）: 有効にするのは部屋の設定 `cascade: true` か、ノードの既定 `--rooms-cascade` のとき。参加者のつないだ
  ノードが home でなければ、そのノードに edge のインスタンスを作って受ける（退避中のノードは作らない）。ノードが満杯のときに
  自動で edge へ回す方式は採らない（明示した部屋だけが複数のノードにまたがる）。
  - 在室・トラックは senca-relay の App 枠（`0x0f`、capability bit `APP`、topic `senca.rooms/1`、JSON の版 1、
    1 MiB まで）で部屋ごとの全量を同期する（変化で即時、2 秒ごと、7 秒で失効）。星形（edge ↔ home）で、home が他の edge の
    参加者を配る。差分を送らないので、取りこぼし・順序の入れ替わり・つなぎ直しに強い。
  - メディアは既存の購読の枠組みに stream id `@room/<部屋>/<トラック>` で載せ、参加者のいるノードへ直接購読する
    （層は TrackId 1, 2, … に rid の順で割り当て、層の変更・キーフレームの要求は既存の Control 枠）。アプリの message は
    App 枠で受け手のノードへ直接送る。
  - home が落ちてグリッドが部屋を edge のノードへ移すと、その edge が参加者のまま home になる（`was an edge`）。
    home のノードが退避するだけで生きているなら、カスケードの部屋はそのノードを edge にして参加者を残す。
  - edge は `room.created`・`room.updated`・`room.destroyed` のイベントを出さない（home と重ねない）。
- 認証 Webhook: 部屋の設定 `auth_webhook_url`、無ければ `--rooms-auth-webhook`。既定は `AfterToken`。URL の無い部屋は
  `RoomService::on_auth_webhook_skipped` でトークンだけで決める（`Off` と同じ規則）。署名は `--rooms-webhook-secret-file`、
  宛先は egress の方針で検査する。
- イベント Webhook: `--rooms-event-webhook`。`webhook::Outbox`（少なくとも 1 回、送り直し、dead letter の記録）で各ノードが
  自分の出来事を送る。SSE にも今までどおり流す。
- 残っていること: 参加者の上限は同期の遅れの間だけ超えうる（厳密な数え上げは home の確認が要る）。home の REST から
  別のノードの参加者を切断・更新することはできない（その参加者のいるノードの API を使う）。メディアパイプラインの書き出しは
  home のノードの参加者だけ。制御面の出来事はイベント Webhook へ送らない（SSE だけ）。中継が切れている間のアプリの
  message は届かない（再送しない）。
- E2EE の部屋には合成・文字起こしのメディアパイプラインを作れない（422）。
