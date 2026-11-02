# SENCA 送信（Android）

> **この環境では作っていない（unbuilt here）。** Android SDK・JDK・NDK が無いため、ビルド・実機での確認はしていない。
> コードは `crates/senca-sdk` の C ABI（`include/senca.h`）に合わせて書いてあり、C ABI 自体は
> `tools/interop/sdk.sh` で SENCA のノードへの送信まで確かめてある。

## 構成

| ファイル | 内容 |
|---|---|
| `app/src/main/cpp/senca_jni.c` | JNI の糊。SDK のコールバックは SDK の送信スレッドから来るので JVM へ attach して呼ぶ |
| `SencaNative.kt` / `SencaSender.kt` | Kotlin の bindings（`SencaSender` は `AutoCloseable`） |
| `Encoders.kt` | MediaCodec のハードウェア H.264 / H.265（B フレームなし）、AAC-LC / Opus |
| `CameraController.kt` | CameraX（Preview ＋ ImageAnalysis）。前後同時は CameraX の concurrent camera（R9-8） |
| `ViscaReceiver.kt` | VISCA over IP の受け口（UDP 52381）。ズーム・オートフォーカス・プリセット・ズーム位置の問い合わせ |
| `MainActivity.kt` | Jetpack Compose の画面。`senca://ingest?…` のリンク（intent filter）で設定を受ける |

## 作り方

前提: Android Studio（AGP 8.7）、NDK 27、JDK 17、Rust（`rustup target add aarch64-linux-android x86_64-linux-android`）、
[`cargo-ndk`](https://github.com/bbqsrc/cargo-ndk)。

```sh
# 1. SDK を Android 向けに作る（静的ライブラリ）
cd crates/senca-sdk
cargo ndk -t arm64-v8a -t x86_64 --platform 26 build --release
mkdir -p ../../apps/sender-android/build/senca-sdk/{arm64-v8a,x86_64}
cp ../../target/aarch64-linux-android/release/libsenca_sdk.a ../../apps/sender-android/build/senca-sdk/arm64-v8a/
cp ../../target/x86_64-linux-android/release/libsenca_sdk.a  ../../apps/sender-android/build/senca-sdk/x86_64/

# 2. アプリ（gradle wrapper は Android Studio で作るか `gradle wrapper` で足す）
cd ../../apps/sender-android
./gradlew :app:assembleDebug
adb install app/build/outputs/apk/debug/app-debug.apk

# 3. 設定を渡す（QR を読んだカメラアプリからのリンクと同じ）
adb shell am start -a android.intent.action.VIEW -d 'senca://ingest?v=1\&proto=srt\&host=192.0.2.10\&port=9000\&streamid=publish:sk_xxx'
```

## 決めたこと・できないこと

- **QR の読み取りはアプリに入れていない。** ML Kit（Google の非公開部品）は使わない方針で、自前の QR 復号器は大きいため。
  端末のカメラアプリ（Android 標準で QR の URL を開ける）で `senca://ingest?…` を読み、リンクでこのアプリを開く。
  文字の入力欄でも設定できる。アプリ内で読む部品は、ライセンスを確認したうえで後で選ぶ（ADR で決める）。
- **WHIP はまだ送れない**（SDK の native の WebRTC は後続）。設定が WHIP なら SDK が `SENCA_ERR_UNSUPPORTED` を返す。
- **センサー（GPS・深度・IMU）の時刻付きメタデータは未実装。** SDK にメタデータのトラック（TS の metadata PES・
  E-RTMP）がまだ無いため。位置情報は既定で送らない設計にする（REQ-009 プライバシー）。
- 前後同時（R9-8）は 2 本の別の送り先（2 つ目の設定）へ送る。SDK の 1 つの送り手は映像 1 本。
- ImageAnalysis の YUV を CPU で符号化器へ写している（720p30 想定）。Surface 入力（CameraX の effect で
  Preview と共有）にすると CPU を減らせる。
- 画面の回転・HDR・手動フォーカス（Camera2 interop）・露出のモード・パン/チルトの切り抜きは後続
  （VISCA ではできない操作に「実行できない」を返す）。
