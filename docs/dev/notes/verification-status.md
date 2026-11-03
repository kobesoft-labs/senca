# 検証の詳細と内部構造のメモ / Verification details and internal notes

利用者ガイド（[1. 概要](../../guide/ja/01-overview.md)）から、開発者向けの詳細をこちらへ移しました。

These developer-oriented details were moved here from the user guide ([1. Overview](../../guide/en/01-overview.md)).

## 制御状態の入口 / Control state entry point

- 制御状態を変える入口は `App::propose` の 1 つだけです。クラスターでは `senca-raft` がコマンドを複製します。
- The control state has exactly one entry point that changes it (`App::propose`). In a cluster, commands are replicated by `senca-raft`.
- payload は `bytes::Bytes` で共有します（[DEVELOPMENT.md](../DEVELOPMENT.md) の「コピーしない」）。
- Payloads are shared as `bytes::Bytes` (see the zero-copy rule in [DEVELOPMENT.en.md](../DEVELOPMENT.en.md)).

## 相互接続テストの件数 / Interop test counts

- QUIC v1・HTTP/3・WebTransport・MoQ の部品: aioquic・OpenSSL・Chrome・moq-dev と 43/43。
  QUIC v1, HTTP/3, WebTransport and MoQ as components: 43/43 against aioquic, OpenSSL, Chrome and moq-dev.
- ノードの `--quic`（HTTP/3 を aioquic・Chrome、MoQ の購読を moq-dev の `moq export`・Chrome の WebTransport）: 34/34。
  The node's `--quic` (HTTP/3 with aioquic and Chrome, MoQ subscriptions with moq-dev's `moq export` and Chrome WebTransport): 34/34.
- 相互接続テストは `tools/interop/*.sh` にあります。/ The interop tests are in `tools/interop/*.sh`.

## リリース前の確認で未実施の項目 / Release checks not yet done

[RELEASE.md](../RELEASE.md)（[English](../RELEASE.en.md)）の確認項目のうち、v0.0.0 の時点で次は未実施です。

As of v0.0.0, the following items of the release checklist have not been done:

- 暗号・認可・分散の所有権の独立レビュー（G8）/ Independent review of the cryptography, authorization and distributed ownership (G8)
- 24 時間の fuzz（60 秒の smoke のみ実施）/ 24-hour fuzzing (only a 60-second smoke run)
- 72 時間の連続試験、基準環境での負荷試験 / The 72-hour soak, and load tests in the reference environment
- 署名付きの配布 / Signed builds

## 記録の食い違い / Records to reconcile

- ノードをまたぐ部屋のカスケードと、ノードの認証 Webhook・イベント Webhook は実装済みです（CHANGELOG の Unreleased、
  `--rooms-cascade`・`--rooms-auth-webhook`・`--rooms-event-webhook`）。ただし [ROADMAP.md](../ROADMAP.md) の M17 は、まだ
  「カスケードは未」と記載しています。
- Room cascading across nodes and the node's auth and event webhooks are implemented (CHANGELOG Unreleased; `--rooms-cascade`,
  `--rooms-auth-webhook`, `--rooms-event-webhook`), but M17 in [ROADMAP.md](../ROADMAP.md) still says cascading is not done.

## 制御ログの形式 / Control log format

単一ノードの `control/log` は、`長さ:u32be | crc32:u32be | JSON` のレコードを追記したものです（1 件ごとに fsync）。

The single-node `control/log` is a sequence of `len:u32be | crc32:u32be | JSON` records, each fsynced.

## 既知の不具合 / Known issues

- （解消）録画中の `gaps` が再起動まで空だった件は、書き手が欠落を索引に書いたときに `RecorderEvent::Gap` を出し、`rec_info` に足すように
  しました。欠落の後の最初のセグメントには、書き手が不連続の印を付けます（配信が終わってから予約の録画に送り手が戻った場合を含む）。
- (Resolved) `gaps` being empty until restart: the recorder now emits `RecorderEvent::Gap` when it writes a gap to the index, and
  `rec_info` is updated. The recorder also marks the first segment after a gap as a discontinuity (including a sender returning to
  a scheduled recording after the broadcast ended).

## 実装のメモ / Implementation notes

- 視聴者の数（`crates/senca-node/src/viewers.rs`）: 接続を持つ視聴は `ViewerGuard` を購読（`hub::Subscription`）か MoQ の `SubRef`
  に持たせ、手放すと数が減ります。HLS は `(IP, User-Agent)` の印と最後の取得の時刻で近似します。上限はノードごとで、クラスター全体の
  合計ではありません。写しのノードが所有者から字幕を取るときの要求（`x-senca-relay-proxy`）は数えません。
- Viewer counting (`crates/senca-node/src/viewers.rs`): connection-based viewers hold a `ViewerGuard` inside their subscription
  (`hub::Subscription`) or MoQ `SubRef`; dropping it decrements. HLS is approximated by an `(IP, User-Agent)` key and the last fetch
  time. Limits are per node, not a cluster-wide total. Caption fetches from mirror nodes (`x-senca-relay-proxy`) are not counted.
- 入力のビットレートの上限（`hub.rs` の `IngressMeter`）: AU の payload の大きさを 1 秒の箱で数え、直近 3 秒の平均を秒が変わるたびに比べます。
  切った送り手は `evicted`・`evicted_why` で `Publisher::access_unit` が誤りを返し、接続の側が閉じます。
- Ingest limit (`IngressMeter` in `hub.rs`): AU payload bytes in 1-second buckets, the 3-second average is compared on every second
  boundary. The evicted publisher gets an error from `Publisher::access_unit` (`evicted`, `evicted_why`) and its connection closes.
- 予約の録画は hub の `persistent` に入り、配信の終わり（猶予切れ）でも閉じません。窓の終わりの停止（`StopRecording`）、取り消し・削除の
  後始末（`scheduler::cleanup_canceled`）で閉じます。
- Scheduled recordings are kept in the hub's `persistent` list and survive end-of-stream; they close at the window end
  (`StopRecording`) or via `scheduler::cleanup_canceled` after a cancel or delete.
- `POST /streams/{id}/plan` の H.264 → AV1 の変換は hub には実装していません（ハードウェアの復号が無い開発機では試せないため、計画を
  実際の動作に合わせました）。変換はパイプラインの `decode`・`encode` で行えます。
- The hub does not implement an H.264 → AV1 transcode for `royalty_free` (it cannot be tested without hardware decoding on the
  development machine), so the plan was made to match the actual behavior. Pipelines (`decode`, `encode`) can do the conversion.

## 利用者ガイドの各ページに対応するソース / Source files behind the user guide pages

| ページ / Page | ソース / Source |
|---|---|
| 3. 設定 / Configuration | `crates/senca-node/src/config.rs` |
| 4. 取り込みと再生 / Ingest and playback | `crates/senca-node/src/rtmp.rs`・`srt.rs`・`webrtc.rs`・`capture.rs`・`http.rs`・`live.rs`・`packager.rs`・`rtsp_serve.rs`・`api/streams.rs`。MoQ の draft の固定 / MoQ draft pinning: `crates/senca-moq/DRAFT.md` |
| 5. 録画 / Recording | `crates/senca-node/src/recorder.rs`・`recovery.rs`・`export.rs`・`thumbnail.rs`・`scheduler.rs`・`api/recordings.rs`・`api/schedules.rs`、`crates/senca-record`、`crates/senca-control/src/schedule.rs` |
| 4.3 視聴者と入力の上限 / Viewer and ingest limits | `crates/senca-node/src/viewers.rs`・`hub.rs`（`IngressMeter`）・`http.rs`・`moq.rs` |
