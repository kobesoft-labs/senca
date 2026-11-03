# 入力・出力・パイプライン・字幕の実装メモ / Inputs, outputs, pipelines and captions: implementation notes

利用者ガイドの [6. 入力と出力](../../guide/ja/06-inputs-outputs.md)、[7. パイプライン](../../guide/ja/07-pipelines.md)、
[8. 字幕と翻訳](../../guide/ja/08-captions.md) から、実装に関する記述をこちらへ移しました。

These implementation details were moved here from the user guide chapters [6](../../guide/en/06-inputs-outputs.md),
[7](../../guide/en/07-pipelines.md) and [8](../../guide/en/08-captions.md).

## 入力と出力 / Inputs and outputs

- `crates/senca-node/src/io.rs`・`hls_pull.rs`・`rtsp_pull.rs`・`srt.rs`・`capture.rs`・`paas.rs`・`egress.rs`・`api/io.rs`
- `crates/senca-paas`（Cloudflare Stream・Amazon IVS・AWS Elemental MediaLive の API）
- 相互接続のテスト / Interop test: `tools/interop/paas.sh`

## パイプライン / Pipelines

- 定義 / definition: `crates/senca-pipeline/src/model.rs`、検証 / validation: `validate.rs`、操作 / control: `control.rs`
- ノード / node: `crates/senca-node/src/pipelines.rs`・`api/pipelines.rs`
- 縮退 / degradation: `crates/senca-control/src/workloads.rs`。縮退レベルのグラフは `prune_graph` で作る。
  The graph for a degradation level is built by `prune_graph`.
- 端から端までのテスト / End-to-end test: `tools/interop/node.sh` の場面 J（scenario J）。ガイド 7.5 のピクチャー・イン・ピクチャーの例はこの場面と同じ構成。
  The picture-in-picture example in guide section 7.5 uses the same setup as this scenario.

## 字幕 / Captions

- `crates/senca-node/src/captions.rs`
- `crates/senca-caption`（`chunk.rs`・`asr.rs`・`merge.rs`・`translate.rs`・`hls.rs`・`vtt.rs`）
- 映像を遅らせて字幕と同期させる方式は、見積もりの部品（`senca-caption` の `latency`）だけがあり、ノードでは使っていない。
  Delaying video to align it with captions exists only as an estimator (`latency` in `senca-caption`); the node does not use it.
- テスト / Test: `tools/interop/captions.sh`（`caption_mock.py`）
