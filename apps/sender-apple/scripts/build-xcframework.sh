#!/usr/bin/env bash
# SDK（crates/senca-sdk）を Apple の各ターゲット向けの静的ライブラリにして xcframework にまとめる。macOS で動かす。
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin x86_64-apple-darwin
set -euo pipefail
HERE=$(cd "$(dirname "$0")/.." && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$HERE/Frameworks/SencaSDK.xcframework
T=${CARGO_TARGET_DIR:-$ROOT/target}
cd "$ROOT"
for t in aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin x86_64-apple-darwin; do
  cargo build -p senca-sdk --release --target "$t"
done
H=$(mktemp -d); trap 'rm -rf "$H"' EXIT
mkdir -p "$H/include" "$H/mac"
cp crates/senca-sdk/include/senca.h "$H/include/"
cat > "$H/include/module.modulemap" <<'MAP'
module CSenca {
    header "senca.h"
    link "senca_sdk"
    export *
}
MAP
lipo -create "$T/aarch64-apple-darwin/release/libsenca_sdk.a" "$T/x86_64-apple-darwin/release/libsenca_sdk.a" \
  -output "$H/mac/libsenca_sdk.a"
rm -rf "$OUT"
xcodebuild -create-xcframework \
  -library "$T/aarch64-apple-ios/release/libsenca_sdk.a" -headers "$H/include" \
  -library "$T/aarch64-apple-ios-sim/release/libsenca_sdk.a" -headers "$H/include" \
  -library "$H/mac/libsenca_sdk.a" -headers "$H/include" \
  -output "$OUT"
echo "built $OUT"
