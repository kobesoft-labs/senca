# Changelog / 変更履歴

## Unreleased / 未リリース

### Added / 追加

- Container images on GitHub Container Registry, `ghcr.io/kobesoft-labs/senca`, for linux/amd64 and linux/arm64 (Raspberry Pi 4/5
  with a 64-bit OS): `latest` (standard: royalty-free codecs and the VA-API runtime), `slim` (no codec libraries; pass-through
  only) and `gpu` (amd64; Intel and AMD GPU drivers). Built natively per architecture by `.github/workflows/docker.yml`
  ／ GitHub Container Registry でコンテナイメージ `ghcr.io/kobesoft-labs/senca` を公開します。linux/amd64 と linux/arm64
  （64 bit の OS の Raspberry Pi 4・5）に対応し、種類は `latest`（standard: ロイヤリティフリーのコーデックと VA-API の実行時）、
  `slim`（コーデックのライブラリなし。そのままの配信・録画・中継）、`gpu`（amd64。Intel・AMD の GPU のドライバー）です。
  `.github/workflows/docker.yml` で、アーキテクチャーごとにその機械でビルドします
- Adding drivers and libraries to the image: mount shared libraries at `/opt/senca/lib` (searched first by the codec and capture
  workers), derive from the published image, or build with `--build-arg EXTRA_PACKAGES=...` / `INTEL_VA_DRIVER=...`
  ／ イメージへのドライバー・ライブラリの追加: 共有ライブラリを `/opt/senca/lib` にマウントする（codec worker とキャプチャーの worker が
  最初に探します）、公開イメージから派生させる、`--build-arg EXTRA_PACKAGES=...`・`INTEL_VA_DRIVER=...` でビルドする、のいずれかでできます
- Guide page 16, Container images ／ ガイドに「16. コンテナイメージ」を追加しました

### Changed / 変更

- The image no longer passes `--codec-lib-dir /usr/lib/x86_64-linux-gnu` (it broke on arm64); libraries are found in
  `/opt/senca/lib` and then the system's default locations. `compose.yaml` runs the published image (`SENCA_IMAGE`) and builds the
  standard target with `--build`
  ／ イメージは `--codec-lib-dir /usr/lib/x86_64-linux-gnu` を指定しなくなりました（arm64 で合わないため）。ライブラリは
  `/opt/senca/lib`、次にシステムの既定の場所から探します。`compose.yaml` は公開イメージ（`SENCA_IMAGE`）で起動し、`--build` では
  standard を作ります

## v0.0.1 — 2026-11-03

### Added / 追加

- `--quic ADDR`: a QUIC v1 listener on one UDP port (off by default; uses `--tls-cert`/`--tls-key`), driven by one task
  over the sans-io `senca-quic` endpoint
  ／ `--quic ADDR`: 1 つの UDP ポートで QUIC v1 の接続を受け付けます（既定は無効。`--tls-cert`・`--tls-key` を使用）。
  sans-io の `senca-quic` の endpoint を 1 つのタスクで処理します
- HTTP/3 (ALPN `h3`): the same routes as TCP HTTP (`/api/v1`, signed HLS including LL-HLS blocking reloads, WHIP/WHEP,
  `/healthz`); SSE and `/signaling` answer 501. TCP HTTP responses advertise `Alt-Svc: h3=":PORT"`
  ／ HTTP/3（ALPN `h3`）: TCP の HTTP と同じルート（`/api/v1`、LL-HLS のブロッキングリロードを含む署名付き HLS、WHIP/WHEP、
  `/healthz`）に対応します。SSE と `/signaling` は 501 を返します。TCP の HTTP の応答には `Alt-Svc: h3=":ポート"` を付けます
- MoQ draft-22 subscriptions (native QUIC `moqt-22` and WebTransport `/moq`): every live stream is the namespace
  `senca/live/{stream_id}` with a hang `catalog.json` (CMAF init, codec description) and CMAF tracks (one fragment per
  object, one group per GOP), fed from the stream hub or a relay mirror; authorized by the playback signature
  (`exp`/`sig` as an AUTHORIZATION TOKEN, native PATH or WebTransport URL query). Playback URLs gain `moq`;
  capabilities gain `output.moq` and `output.http3`
  ／ MoQ draft-22 の購読（ネイティブ QUIC の `moqt-22` と WebTransport の `/moq`）: すべてのライブストリームを namespace
  `senca/live/{stream_id}` として公開します。内容は hang の `catalog.json`（CMAF の init、コーデックの description）と
  CMAF のトラック（1 object が 1 fragment、GOP ごとに 1 group）です。ストリームの hub または中継のミラーから生成します。
  認可には再生の署名を使います（`exp`・`sig` を AUTHORIZATION TOKEN、ネイティブの PATH、または WebTransport の URL の
  クエリで渡します）。再生 URL に `moq`、capabilities に `output.moq`・`output.http3` を追加しました
- MoQ publishing: `PUBLISH_NAMESPACE senca/live/{stream_id}` with the stream key as AUTHORIZATION TOKEN; the node
  subscribes to the catalog and CMAF tracks and becomes the stream's publisher (`cmaf` container only)
  ／ MoQ での配信（送信）: ストリームキーを AUTHORIZATION TOKEN に入れて `PUBLISH_NAMESPACE senca/live/{stream_id}` を
  送ります。ノードは catalog と CMAF のトラックを購読し、そのストリームの配信者になります（コンテナは `cmaf` のみ）
- `senca-moq`: draft-22 FILL_PARAMETERS answered with a fill stream (`Session::fill_object`/`fill_finish`), and an
  optional `description` in catalog renditions
  ／ `senca-moq`: draft-22 の FILL_PARAMETERS に fill の stream で応答します（`Session::fill_object`・`fill_finish`）。
  catalog の rendition に省略可能な `description` を追加しました
- `tools/interop/node_quic.sh`: aioquic (HTTP/3), moq-dev `moq export fmp4`/`fetch`, Chrome (HTTP/3 and WebTransport)
  ／ `tools/interop/node_quic.sh`: aioquic（HTTP/3）、moq-dev の `moq export fmp4`・`fetch`、Chrome（HTTP/3・WebTransport）

- Room cascading across nodes: with the room setting `cascade: true` (or `--rooms-cascade`), participants are served on the
  node they connect to (edge) and joined to the room's node (home) over the inter-node relay. Presence and tracks are synced as
  full snapshots, media is subscribed as `@room/<room>/<track>`, app messages go directly to the recipients' nodes. When the home
  dies, an edge with participants becomes the new home.
  ／ ノードをまたぐ部屋のカスケード: 部屋の設定 `cascade: true`（または `--rooms-cascade`）を有効にすると、参加者は接続した
  ノード（edge）で処理され、部屋の配置先のノード（home）とノード間の中継で接続されます。在室情報とトラックはスナップショット全体で
  同期し、メディアは `@room/<部屋>/<トラック>` として購読します。アプリのメッセージは宛先の参加者がいるノードへ直接送ります。
  home が停止すると、参加者のいる edge が新しい home になります。
- senca-relay: versioned, bounded App frame (`0x0f`, capability `APP`) for integration messages (`send_app`, `take_app_messages`).
  ／ senca-relay: 連携用メッセージのための、バージョン付き・サイズ上限付きの App フレーム（`0x0f`、capability `APP`）を追加しました（`send_app`・`take_app_messages`）。
- Auth webhook in the node: room setting `auth_webhook_url` or `--rooms-auth-webhook`, `--rooms-auth-mode after_token|only`,
  signed with `--rooms-webhook-secret-file`, destinations checked by the egress policy. Rooms without a URL are admitted by token.
  ／ ノードの認証 Webhook: 部屋の設定 `auth_webhook_url` または `--rooms-auth-webhook` で指定し、`--rooms-auth-mode after_token|only`
  で方式を選びます。リクエストは `--rooms-webhook-secret-file` の鍵で署名し、宛先は egress ポリシーで検査します。URL が未設定の
  部屋は、トークンだけで参加を認めます。
- Event webhook: `--rooms-event-webhook` delivers room events (signed, at least once, retries, dead letters shown in
  `GET /api/v1/nodes`); SSE is unchanged.
  ／ イベント Webhook: `--rooms-event-webhook` で部屋のイベントを送信します（署名付き、少なくとも 1 回の配送、再送あり。
  配送をあきらめた件数（dead letter）は `GET /api/v1/nodes` に表示）。SSE は従来どおりです。
- Interop test `tools/interop/cluster_rooms.sh` (3 nodes, Chrome, home node failure, mock webhook receiver).
  ／ 相互接続テスト `tools/interop/cluster_rooms.sh`（3 ノード、Chrome、home ノードの障害、Webhook のモック受信側）。
- Stream `limits.max_viewers` is enforced per node: HLS clients (source IP and User-Agent seen within 3 segment durations, at
  least 10 s) plus RTMP/SRT/RTSP play, WHEP and MoQ sessions; new viewers over the limit get 503 (HLS, WHEP, RTSP),
  `NetStream.Play.Failed` (RTMP), `SRT_REJX_OVERLOAD` (SRT) or `EXCESSIVE_LOAD` (MoQ). The stream's `live.viewers` shows the counts.
  ／ ストリームの `limits.max_viewers` をノードごとに適用します。数えるのは HLS の客（送り元の IP と User-Agent。セグメント 3 本分、
  10 秒以上の間）と、RTMP・SRT・RTSP の play、WHEP、MoQ の接続です。上限を超える新しい視聴は、503（HLS・WHEP・RTSP）、
  `NetStream.Play.Failed`（RTMP）、`SRT_REJX_OVERLOAD`（SRT）、`EXCESSIVE_LOAD`（MoQ）で断ります。数はストリームの `live.viewers` に出ます。
- Stream `limits.max_ingress_bps` is enforced: a sender whose 3-second average stays above the limit for 5 seconds is disconnected
  (logged), and new senders are refused for 10 seconds.
  ／ ストリームの `limits.max_ingress_bps` を適用します。直近 3 秒の平均が上限を超えた状態が 5 秒続いた配信者を切断し（ログに出力）、
  その後 10 秒は新しい配信者を拒否します。
- `latency_profile: interactive` now has its own behavior: LL-HLS parts of 200 ms, a 0.25 s wait (instead of 1 s) for a missing
  video/audio track before HLS starts, a 1 MiB RTMP play backlog limit, and an 80 ms SRT receive latency. `standard` and `low` are unchanged.
  ／ `latency_profile: interactive` に専用の動作を付けました。LL-HLS の part を 200 ms にし、映像・音声の片方が届かないときの HLS の
  開始までの待ちを 1 秒から 0.25 秒に、RTMP play の送信待ちの上限を 1 MiB に、SRT の受信遅延を 80 ms にします。`standard`・`low` は変わりません。
- `rtsps://` RTSP pull inputs connect over TLS (system CAs, or a pinned key with `options.tls_spki_sha256`).
  ／ `rtsps://` の RTSP pull の入力を TLS でつなぎます（OS の認証局、または `options.tls_spki_sha256` での公開鍵の固定）。
- MP4 export takes `start` and `end` (RFC 3339) to export a range: `GET /recordings/{id}/export.mp4?start=&end=`.
  ／ MP4 の書き出しで `start`・`end`（RFC 3339）を指定して範囲を切り出せます: `GET /recordings/{id}/export.mp4?start=&end=`。
- Pipeline `snapshot_on_change` images are stored per pipeline (`GET /pipelines/{id}/snapshots`, `GET`/`DELETE
  /pipelines/{id}/snapshots/{snapshot_id}`) and pruned to the node's new `params.keep` (default 1000).
  ／ パイプラインの `snapshot_on_change` の静止画を、パイプラインごとに保存します（`GET /pipelines/{id}/snapshots`、
  `GET`・`DELETE /pipelines/{id}/snapshots/{snapshot_id}`）。部品の新しい `params.keep`（既定 1000）の枚数まで残します。
- Captions on relay mirrors: a non-owner node fetches `master.m3u8`, subtitle playlists and WebVTT segments from the owner node,
  so captions work on any node's playback URL.
  ／ 中継のミラーでの字幕: 所有者でないノードは、`master.m3u8`・字幕のプレイリスト・WebVTT のセグメントを所有者のノードから取得して
  返します。どのノードの再生 URL でも字幕を見られます。
- Recording storage on S3-compatible object storage (AWS S3, MinIO, Cloudflare R2): recordings are written locally first and
  uploaded once immutable; the stored index carries object keys; local copies are removed after `--recording-keep-local`
  (`--recording-storage`, `--s3-endpoint`, `--s3-region`, `--s3-addressing`, `--s3-credentials`, `--s3-part-size`)
  ／ 録画の保存先に S3 互換のオブジェクトストレージ（AWS S3・MinIO・Cloudflare R2）を使えます。録画はまず手元に書き、
  変わらなくなったものから保存先へ上げます。保存先のインデックスにはオブジェクトのキーを記録し、`--recording-keep-local` の後にローカルのコピーを削除します
- VOD (including `Range`), MP4 export and thumbnails are read from storage when the local copy is gone; the first whole-recording
  export is stored and reused (multipart upload for large objects; exports with `start`/`end` are not stored)
  ／ 手元に無い録画の VOD（`Range` を含む）・MP4 の書き出し・サムネイルを保存先から読みます。録画全体の最初の書き出しを保存先に
  置いて使い回します（大きなものはマルチパートアップロード。`start`・`end` を指定した書き出しは置きません）
- Recordings carry `storage` (`{uri, archived_at}`) in the API and the control state
  ／ 録画の API と制御状態に `storage`（`{uri, archived_at}`）を追加しました
- `senca-s3` crate: SigV4 signing, request layout and bounded XML parsing (sans-io), tested with the AWS SigV4 test vectors
  ／ `senca-s3`: SigV4 の署名・リクエストの組み立て・上限付きの XML の読み取り（sans-io）。AWS の SigV4 のテストベクターで確認しています
- `tools/interop/s3.sh`: end-to-end test against an S3-compatible server in Docker
  ／ `tools/interop/s3.sh`: Docker の S3 互換サーバーを相手にした通しのテスト
- SSE replay: `GET /api/v1/events` with `Last-Event-ID` now sends the missed events from a bounded window the node keeps
  (`--events-retain`, default 4096; a single node also reloads, after a restart, the events written since the last control-log
  snapshot). `resync` is sent only when the position is older than the window. Event IDs are the number of applied commands, the
  same on every node of a cluster (previously the Raft log index), so clients can reconnect to any node
  ／ SSE の再送: `GET /api/v1/events` に `Last-Event-ID` を付けてつなぎ直すと、ノードが残している直近のイベント
  （`--events-retain`、既定 4096 件。単一ノードでは再起動の後も、制御ログのスナップショットより後の分）から、取りこぼした分を送ります。
  `resync` は、位置が残している範囲より古いときだけ送ります。イベントの `id` は適用したコマンドの数で、クラスターのどのノードでも
  同じ値です（以前は Raft のログの位置）。どのノードにつなぎ直しても続きから受け取れます
- Rate limits: 429 `RATE_LIMITED` with `Retry-After` and `details.limit` for `/api/v1` per client IP (`--rate-limit-ip`, 300/s)
  and per token or JWT subject (`--rate-limit-token`, 200/s), failed authentications per IP (`--rate-limit-auth-failures`,
  30/min; once used up, even valid credentials get 429), and new connections to `/signaling`, WHIP, WHEP and SSE per IP
  (`--rate-limit-connect`, 30/s). `0` disables a limit
  ／ 要求の量の上限: `/api/v1` の送信元の IP ごと（`--rate-limit-ip`、1 秒に 300）とトークン・JWT の subject ごと
  （`--rate-limit-token`、1 秒に 200）、IP ごとの認証の失敗（`--rate-limit-auth-failures`、1 分に 30。使い切ると正しい資格情報でも
  429）、IP ごとの `/signaling`・WHIP・WHEP・SSE の接続の開始（`--rate-limit-connect`、1 秒に 30）を数え、超えると 429
  `RATE_LIMITED`（`Retry-After`・`details.limit` 付き）を返します。`0` で無効にできます
- Problem codes now in use: `UNSUPPORTED_PROTOCOL_VERSION` (422) for `/api/v{N}` other than v1, `INPUT_TIMEOUT` (504) when a
  current frame is requested but no keyframe arrives within 5 seconds, `OWNERSHIP_LOST` (409, with `details.owner_api`) for
  starting a recording or taking a current frame of a stream that another cluster node is receiving
  ／ 使われていなかった問題の型を返すようにしました: `/api/v1` 以外の版のパスは `UNSUPPORTED_PROTOCOL_VERSION`（422）、今の画を
  頼んで 5 秒以内にキーフレームが届かなければ `INPUT_TIMEOUT`（504）、クラスターで別のノードが配信を受けている stream の録画の開始・
  今の画は `OWNERSHIP_LOST`（409、`details.owner_api` 付き）
- The `publish` scope is now meaningful: WHIP accepts an API token or JWT with `publish` of the same tenant instead of the stream key
  ／ `publish` の権限を使えるようにしました: WHIP は、stream key の代わりに、同じテナントの `publish` を持つ API トークン・JWT を受け付けます
- `--webrtc-public-ip IP[,IP]` sets the ICE candidate addresses; when `--webrtc-udp` is bound to `0.0.0.0`/`::` without it,
  the node announces its interface addresses instead of the wildcard. Applies to WHIP, WHEP and rooms. The Docker image and
  `compose.yaml` enable WebRTC on UDP 8443 by default (`SENCA_PUBLIC_IP` in compose)
  ／ `--webrtc-public-ip IP[,IP]` で ICE の候補のアドレスを指定できます。指定せずに `--webrtc-udp` を `0.0.0.0`・`::` で待つと、
  ワイルドカードの代わりにノードのインターフェースのアドレスを知らせます。WHIP・WHEP・部屋に共通です。Docker イメージと
  `compose.yaml` は、既定で UDP 8443 の WebRTC を有効にしました（compose では `SENCA_PUBLIC_IP`）
- Rooms: the media pipeline `speaker` layout follows the room's dominant speaker (hysteresis: no switch while the speaker is still
  talking or within 2 s; switches the composite's scene and requests a keyframe; `status.active_speaker`); the room setting
  `recording: per_participant|composite` creates and maintains a managed recording media pipeline (`room-rec.{room}`,
  `managed: true`); media pipelines also export participants on cascade edges; updating or disconnecting a participant through
  any node's REST API is forwarded to the participant's node
  ／ 部屋: メディアパイプラインの `speaker` の並びが、部屋の主な話者に合わせて切り替わります（話している間と切り替えから 2 秒は
  替えない。合成のシーンを切り替えてキーフレームを求める。`status.active_speaker`）。部屋の設定 `recording: per_participant|composite`
  で、録画用のメディアパイプライン（`room-rec.{部屋}`、`managed: true`）を自動で作り、設定に合わせます。メディアパイプラインは
  カスケードの edge にいる参加者も書き出します。参加者の変更・切断は、どのノードの REST API で呼んでも、参加者のいるノードへ転送します

### Security / セキュリティ

- Node-to-node authentication (Raft and relay) is now a mutual challenge-response with fresh nonces and HMAC keys
  derived from the cluster secret (HKDF-SHA256); the old fixed secret hash, which could be replayed, is gone. It also
  runs inside mTLS. Nodes of earlier versions cannot join; upgrade the whole cluster together
  ／ ノード間（Raft・中継）の認証を、新しい乱数とクラスタシークレットから導出した鍵（HKDF-SHA256）の HMAC による相互の
  チャレンジレスポンス認証にしました。再送で通ってしまう固定のハッシュは送りません。mTLS の中でも行います。以前のバージョンの
  ノードとは接続できないため、クラスタはまとめて更新してください
- Secrets are sealed with a cluster-wide data key, wrapped with a key derived from the cluster secret and replicated
  in the control state, so a secret stored on one node opens on every node. Secrets sealed by earlier versions with a
  node's own `secret-key` are re-sealed automatically. Single-node behavior is unchanged
  ／ 秘密値をクラスタ共通のデータ鍵で暗号化するようにしました（データ鍵はクラスタシークレットから導出した鍵で暗号化して制御状態で
  複製）。どのノードで登録した秘密値もすべてのノードで使えます。以前のバージョンでノードの `secret-key` で暗号化した値は自動で
  暗号化し直します。単一ノードの動作は変わりません

### Cluster and grid / クラスタとグリッド

- Fencing: grid work (pull inputs, push outputs, pipelines) checks that it still holds the current assignment before
  external side effects and every 0.5 s while running. A node that has lost contact with a quorum for more than 4 s
  stops its grid work and reports 503 on `/readyz` until contact returns. Recording finalization waits for a quorum
  and closes as `interrupted` if the stream's ownership moved
  ／ フェンシング: ワークロード（pull の入力・push の出力・パイプライン）は、外部への副作用の前と実行中 0.5 秒ごとに、現在も
  自ノードへの割り当てかを確認します。過半数との通信が 4 秒を超えて途絶えたノードはワークロードを停止し、回復するまで
  `/readyz` は 503 を返します。録画の確定は過半数との通信を待ち、所有権が移っていれば `interrupted` で閉じます
- Stream ownership is released (`ReleaseStream`) when the input ends after the reconnect grace, and cleared for nodes
  marked down; viewers on other nodes get 404 instead of a redirect to a stale owner
  ／ 入力が終わる（再接続の猶予の後）と所有権を解放し、停止と判定したノードの所有権も解放します。他のノードの視聴者は停止した
  所有者へリダイレクトされず 404 になります
- Draining nodes refuse new RTMP, SRT and WHIP publishers (WHIP is redirected with 307 to a node that is not draining);
  connected sessions are kept
  ／ drain 中のノードは、新しい RTMP・SRT・WHIP の配信者を拒否します（WHIP は drain 中でないノードへ 307 でリダイレクト）。接続済みのセッションは継続します
- Heartbeats report measured CPU, memory and network usage from `/proc`, shown as `usage` in `GET /api/v1/grid`.
  Devices are re-probed every 10 s (fresh codec worker Hello, capture device list) and devices that disappear raise
  `device.down` automatically (`device.up` when back); the manual disable flag stays separate
  ／ ハートビートに `/proc` から測定した CPU・メモリ・帯域の使用量を含めます（`GET /api/v1/grid` の `usage`）。デバイスは 10 秒ごとに
  再検出し（新しい codec worker の Hello、キャプチャデバイスの一覧）、検出されなくなると自動で `device.down`、戻ると `device.up` に
  なります。手動の無効化とは別です
- The node refuses to start when a relay address (default Raft port + 1) collides with another node's Raft or relay
  address, or when peer IDs are duplicated
  ／ 中継アドレス（既定は Raft のポート + 1）が他のノードの Raft・中継のアドレスと重なる設定や、peers の番号の重複は起動時にエラーにします

### Configuration and logs / 設定とログ

- `labels` in the JSON config file are validated like `--label` (also: no control characters, `raft_id` reserved,
  at most 64) ／ 設定ファイルの `labels` も `--label` と同じ規則で検査します（制御文字は不可、`raft_id` も予約、64 個まで）
- `--log-json` now also covers the first-admin-token notice, codec/capture worker stderr lines, configuration errors
  and fatal errors ／ `--log-json` で、最初の管理者トークンの通知・codec/capture worker の標準エラー出力・設定の誤り・
  起動の失敗も JSON で出力します

### Changed / 変更

- `PATCH /rooms/{id}`: `settings` is now a JSON Merge Patch (only the fields sent change; `null` resets a field to its default)
  instead of replacing the whole object
  ／ `PATCH /rooms/{id}` の `settings` を、全体の置き換えから JSON Merge Patch に変えました（送った項目だけが変わり、`null` で既定に戻ります）
- E2EE rooms: every server-side processing request answers 422 (`UNSUPPORTED_CODEC`, `details.reason: "e2ee"`), including
  `recording: composite` (was 400), turning on `e2ee` with a composite or captions media pipeline, and, for streams exported from
  E2EE rooms (marked `metadata.e2ee: "true"`), still images, captions, saved snapshots, snapshot schedules and pipeline sources;
  their recordings get no thumbnail
  ／ E2EE の部屋: サーバーで中身を処理する要求はすべて 422（`UNSUPPORTED_CODEC`、`details.reason: "e2ee"`）にしました。
  `recording: composite`（以前は 400）、合成・文字起こしのメディアパイプラインがある部屋の `e2ee` を有効にすること、E2EE の部屋から
  書き出した stream（`metadata.e2ee: "true"` の印）の静止画・字幕・静止画の保存・静止画の予約・パイプラインの入力が対象です。
  その録画にはサムネイルを作りません
- `GET /streams/{id}/snapshot` waits up to 5 seconds for a keyframe (was an immediate 409)
  ／ `GET /streams/{id}/snapshot` は、キーフレームを 5 秒まで待つようにしました（以前はすぐに 409）
- A scheduled recording lasts until the end of its window: if the sender returns after the broadcast ended mid-window, the same
  recording continues with a `publisher_reconnect` gap and a discontinuity.
  ／ 予約による録画は時間帯の終わりまで続きます。時間帯の途中で配信が終了した後に配信者が戻ると、`publisher_reconnect` の欠落と
  不連続を挟んで同じ録画に記録します。
- Only `pending` and `running` `live` schedules restrict publishing; finished (`completed`, `missed`, `failed`) ones no longer block
  publishing outside their windows.
  ／ 配信を制限する `live` の予約は、`pending`・`running` のものだけになりました。終了した（`completed`・`missed`・`failed`）予約は、
  時間帯の外の配信を妨げません。
- Cancelling or deleting a running schedule stops its recording and still-image saving; the run becomes `canceled`.
  ／ 実行中の予約を取り消す・削除すると、その回の録画と静止画の保存を止め、回は `canceled` になります。
- `POST /streams/{id}/plan` reports only what the node does: no `transcode` for `royalty_free` H.264 (the track is dropped and shown
  as `unsupported` with a reason) or for H.265 over WebRTC.
  ／ `POST /streams/{id}/plan` は、ノードが実際に行うことだけを返します。`royalty_free` の H.264（トラックは破棄され、理由付きの
  `unsupported` になります）と WebRTC への H.265 で `transcode` と表示しなくなりました。
- In a cluster, any node configured with the same storage serves stored recordings (VOD, export, thumbnail) itself; before,
  export and thumbnails only worked on the node that made the recording
  ／ クラスターでは、同じ保存先を設定したどのノードでも、保存先の録画（VOD・書き出し・サムネイル）を返します。以前は書き出しと
  サムネイルは録画を作ったノードでしか動きませんでした
- Deleting a recording (API or retention) removes its stored objects; in a cluster the node that made the recording performs the
  deletion, whichever node received the request, and retries when storage is unreachable
  ／ 録画の削除（API・保持期限）で保存先のオブジェクトも消します。クラスターでは、リクエストを受けたノードに関わらず録画を作った
  ノードが削除し、保存先に届かないときはやり直します

### Fixed / 修正

- Recording `gaps` are shown while the recording is running (previously only after a node restart).
  ／ 録画の `gaps` が録画中にも表示されるようになりました（以前はノードを再起動するまで表示されませんでした）。

## v0.0.0 — 2026-11-03

First public snapshot.

最初の公開版です。

### Included / 含まれるもの

- Ingest: RTMP/RTMPS (Enhanced RTMP), SRT, WHIP, RTSP/RTMP/SRT/HLS pull, capture devices
  ／ 取り込み: RTMP/RTMPS（Enhanced RTMP）、SRT、WHIP、RTSP・RTMP・SRT・HLS の pull、キャプチャデバイス
- Delivery: HLS (TS, fMP4, LL-HLS), RTMP/SRT/RTSP play, WHEP, signed URLs, relay mirrors across nodes
  ／ 配信: HLS（TS・fMP4・LL-HLS）、RTMP・SRT・RTSP の play、WHEP、署名付き URL、ノード間の中継のミラー
- Recording, VOD, MP4 export, schedules, crash recovery ／ 録画・VOD・MP4 の書き出し・予約録画・障害後の復旧
- Pipelines and production controls, snapshots, captions and translation
  ／ パイプラインと配信制作の操作、静止画、字幕と翻訳
- Codecs: H.264/H.265 through GPU vendor SDKs only; AV1, VP8/VP9 and Opus in software
  ／ コーデック: H.264/H.265 は GPU メーカーの SDK のみ、AV1・VP8/VP9・Opus はソフトウェア
- Clustering: in-house Raft, mTLS, ownership with fencing ／ クラスター: 独自実装の Raft、mTLS、所有権と fencing
- Grid: resource-aware placement, manual pins, priorities, failover with degradation and restore
  ／ グリッド: 資源に応じた配置、手動の割り当て、優先度、縮退を伴うフェイルオーバーと復帰
- Realtime rooms (SFU, simulcast, spotlight, forwarding filters, DataChannels, join tokens)
  ／ リアルタイムの部屋（SFU・サイマルキャスト・スポットライト・転送フィルター・DataChannel・参加トークン）
- Media pipelines for rooms (export, composite, record, live connector, transcription)
  ／ 部屋のメディアパイプライン（書き出し・合成・録画・ライブ配信サービスへの送信・文字起こし）
- REST API (OpenAPI), SSE, web console, browser SDK, TypeScript and Rust API clients
  ／ REST API（OpenAPI）、SSE、管理画面、ブラウザー用 SDK、TypeScript・Rust の API クライアント
- QUIC v1, HTTP/3, WebTransport and MoQ as libraries (not yet wired into the node)
  ／ QUIC v1・HTTP/3・WebTransport・MoQ のライブラリ（この時点ではノードに組み込まれていません）

### Not verified yet / 未検証の項目

- GPU codecs on real hardware, capture hardware, iOS/Android sender apps
  ／ 実機 GPU でのコーデック、キャプチャデバイス、iOS・Android の配信アプリ
- Browsers other than Chrome, TURN paths ／ Chrome 以外のブラウザー、TURN の経路
- Room cascading across nodes, auth webhook for rooms in the node
  ／ ノードをまたぐ部屋のカスケード、ノードでの部屋の認証 Webhook
- Real cloud PaaS accounts (tested against mocks only) ／ 実際のクラウド PaaS のアカウント（モックの API でのみ確認）
