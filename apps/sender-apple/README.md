# SENCA 送信（iOS / iPadOS / macOS）

> **この環境では作っていない（unbuilt here）。** Xcode・Swift が無いため、ビルド・実機での確認はしていない。
> SDK（`crates/senca-sdk`）の C ABI は `tools/interop/sdk.sh` で SENCA のノードへの送信まで確かめてある。

## 構成

| 場所 | 内容 |
|---|---|
| `Package.swift` | SwiftPM。`CSenca`（xcframework: `libsenca_sdk.a` ＋ `senca.h` ＋ module map）と `SencaKit` |
| `Sources/SencaKit/Sender.swift` | C ABI の Swift ラッパー（`Sender`、`SencaError`、統計、設定文字列の正規化） |
| `Sources/SencaKit/Visca.swift` | VISCA over IP のパケットの解釈・返事 |
| `App/` | SwiftUI のアプリ本体（Xcode のアプリのターゲットに入れる） |
| `App/Encoders.swift` | VideoToolbox の H.264 / HEVC（並べ替えなし、avcC/hvcC をそのまま SDK へ）、AVAudioConverter の AAC / Opus |
| `App/CaptureEngine.swift` | AVCaptureSession。前後同時は AVCaptureMultiCamSession（2 本目は別の送り先） |
| `App/QRScannerView.swift` | QR の読み取り（AVFoundation の標準の `AVCaptureMetadataOutput`。iOS / iPadOS） |
| `scripts/build-xcframework.sh` | SDK を iOS・シミュレーター・macOS 向けに作り xcframework にする |

## 作り方（macOS + Xcode 16）

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin x86_64-apple-darwin
apps/sender-apple/scripts/build-xcframework.sh      # → apps/sender-apple/Frameworks/SencaSDK.xcframework
cd apps/sender-apple && swift build                  # SencaKit（macOS）が通ることを確かめる
```

アプリ:

1. Xcode で「App」（iOS と macOS の multiplatform、SwiftUI）を新しく作り、`App/*.swift` を入れる。
2. File → Add Package Dependencies → Add Local… で `apps/sender-apple` を足し、`SencaKit` をリンクする。
3. Info.plist に足す:
   - `NSCameraUsageDescription`: 「映像を配信するためにカメラを使います」
   - `NSMicrophoneUsageDescription`: 「音声を配信するためにマイクを使います」
   - `CFBundleURLTypes`: URL scheme `senca`（QR を標準のカメラで読んだときもこのアプリで開く）
   - macOS: App Sandbox の Camera・Audio Input・Outgoing Connections (Client) を有効にする
4. 実機で動かす（シミュレーターにはカメラが無い）。

## 決めたこと・できないこと

- 送り先は `senca://ingest?…`（QR・リンク・入力欄）、`srt://…`、`rtmp://…/KEY`。**WHIP はまだ送れない**
  （SDK の native の WebRTC は後続。WHIP の設定は SDK が `SENCA_ERR_UNSUPPORTED` で断る）。RTMPS は SDK に
  TLS の部品を組み込んでから。
- 時刻は capture の時計（ホストの時計）を µs にしてそのまま SDK に渡す（映像・音声とも同じ時計）。
- 前後同時（R9-8）は 2 本の別の送り先へ送る。同じ時刻の基準なので受け側でそろえられる。
- **センサー（GPS・LiDAR/TrueDepth の深度・IMU）の時刻付きメタデータは未実装**（SDK にメタデータのトラックが無い）。
- VISCA の受け口（`ViscaCommand` の解釈・返事）は SencaKit にあるが、アプリの UDP の受け口とカメラ操作
  （`AVCaptureDevice.videoZoomFactor` など）への割り当ては後続（Android 版の `ViscaReceiver.kt` と同じ形にする）。
- ScreenCaptureKit（macOS の画面・ウィンドウ）は後続。ブラウザ（`apps/sender-web`）からは画面を送れる。
