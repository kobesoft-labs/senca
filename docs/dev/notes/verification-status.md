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

- 録画の `gaps` は、ノードの起動後に作成した録画では再起動まで空で表示されます。`App::recording_info` は `rec_info` のキャッシュを
  優先して返しますが、`RecorderEvent::Appended` では `gaps` を更新していないためです（`crates/senca-node/src/app.rs`）。
- For recordings created since the node started, `gaps` is shown as empty until restart: `App::recording_info` returns the cached
  `rec_info` first, and `RecorderEvent::Appended` does not update `gaps` (`crates/senca-node/src/app.rs`).

## 利用者ガイドの各ページに対応するソース / Source files behind the user guide pages

| ページ / Page | ソース / Source |
|---|---|
| 3. 設定 / Configuration | `crates/senca-node/src/config.rs` |
| 4. 取り込みと再生 / Ingest and playback | `crates/senca-node/src/rtmp.rs`・`srt.rs`・`webrtc.rs`・`capture.rs`・`http.rs`・`live.rs`・`packager.rs`・`rtsp_serve.rs`・`api/streams.rs`。MoQ の draft の固定 / MoQ draft pinning: `crates/senca-moq/DRAFT.md` |
| 5. 録画 / Recording | `crates/senca-node/src/recorder.rs`・`recovery.rs`・`export.rs`・`thumbnail.rs`・`scheduler.rs`・`api/recordings.rs`・`api/schedules.rs`、`crates/senca-record`、`crates/senca-control/src/schedule.rs` |
