# 開発の約束

## 全体

- Rust のモノレポ（`crates/*`）。edition 2024。`cargo test --workspace` と `cargo clippy --workspace --all-targets -- -D warnings` が通ること。
- 文書・コメントは日本語。識別子は英語。
- コミットや追跡するファイルに、作業に使った道具の名前や共同作者の行などの痕跡を残さない。

## 層

- **sans-io**: プロトコル・コンテナ・ビットストリームの crate（`senca-h264`、`senca-rtmp`、`senca-mpegts` など）は I/O・スレッド・時計を持たない。バイト列を入れるとイベントや出力バイト列が返る状態機械にする。時刻は引数で渡す。
- **I/O** は `senca-node` などの実行ファイル側だけ（tokio）。
- 共通の型は `senca-core`（`AccessUnit`・`CodecConfig`・`Timebase`・`Id128`・`BitReader`）。

## 守ること

- **厳格に拒否する。** 長さ・整数演算・入れ子・総量に上限を置き、壊れた入力は `Err` を返す。パニック・無限ループ・無制限の確保をしない（仕様 9.4・AC-01）。`unsafe` は禁止（FFI の crate だけ例外で、理由と境界をコメントに書く）。
- **コピーしない。** payload は `bytes::Bytes` で受け、切り出し（`slice`）で渡す（REQ-010 R10-7）。
- **独立実装で確かめる。** 単体試験に加えて、FFmpeg など別の実装と突き合わせる試験を `tools/interop/` に置く（FFmpeg は試験相手としてだけ使う。製品には入れない）。
- **秘密を出さない。** stream key・トークンはログ・エラー・Debug 表示に出さない。
- **暗号の部品は自分で書かない。** AES・SHA・HMAC・HKDF・署名・鍵交換・乱数は検証済みのライブラリ（ADR-0002）。プロトコル（TLS/DTLS/SRTP/QUIC のハンドシェイクや記録層）は自前。
- **依存を足すときは ADR-0002 の一覧を更新する。**

## 試験用の道具（開発機）

- `~/tools/ffmpeg`・`~/tools/ffprobe`（静的ビルド 7.0.2）。`FFMPEG`・`FFPROBE` 環境変数で上書きできる。
- Docker（Chromium などの独立実装を動かす）。
