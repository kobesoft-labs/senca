# REQ-004 ハードウェアボードへの幅広い対応

- 追加日: 2026-10-26（利用者の指示）
- 状態: 要件として受理。

## 要件

GPU のエンコーダー・デコーダーと、映像の入出力ボードに幅広く対応する。

| 種類 | 対象（初期候補） | 使う SDK・API |
|---|---|---|
| NVIDIA GPU | NVENC / NVDEC | NVIDIA Video Codec SDK |
| AMD GPU | AMF（Windows/Linux）、VAAPI（Linux） | AMF SDK、libva |
| Intel GPU | Quick Sync | oneVPL、libva |
| Apple | VideoToolbox（開発・SDK 用） | VideoToolbox |
| キャプチャ／出力ボード | Blackmagic DeckLink | DeckLink SDK（Desktop Video） |
| その他の入出力（後続） | AJA、Magewell、V4L2 対応機器 | 各社 SDK、V4L2 |

## 仕様書との関係

- GPU のエンコード・デコードは「コーデックは外部利用可」（仕様 2.1）の範囲。`senca-codec-worker` に隔離して呼ぶ（仕様 3.1・9.4）。
- キャプチャボードは「ドライバー・ハードウェアは外部利用」（仕様 2.1）の範囲。SDK は入力アダプターの中に閉じ込め、得た映像・音声は共通メディアモデル（仕様 4 章）に変換する。DeckLink の映像は未圧縮なので、エンコードは codec worker を通す。
- 色変換・リサイズ・合成は原則自前（仕様 2.1）。GPU 上で行う場合は自前のカーネル（CUDA/Vulkan/Metal のシェーダー）で書き、SDK の処理機能に頼らない。例外は ADR に記録する。

## 決まり

- **能力の検出。** ノード起動時に GPU・ボードを検出し、`GET /devices` と capabilities に出す。配置（仕様 8.3）は能力を見て決める。
- **プロバイダーの抽象化。** `CodecProvider`（仕様 11 章）をハードウェアごとに実装し、同じパイプライン定義のままソフトウェア・ハードウェアを切り替えられる。
- **資源の枠。** GPU のセッション数（NVENC の同時セッション制限など）とメモリを資源として予約する（`Scheduler.reserve`）。
- **障害の隔離。** ドライバーの異常で codec worker が落ちても、制御面と他テナントに波及させない（仕様 3.1、AC-17）。
- **試験。** 各ボードは実機で試験し、型番・ドライバー版を相互接続表に記録する。実機がないものは「未検証」と表示する。
