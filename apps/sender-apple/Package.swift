// swift-tools-version:5.9
// SENCA 送信（iOS / iPadOS / macOS）の共通部品。SDK（Rust、C ABI）は scripts/build-xcframework.sh で
// Frameworks/SencaSDK.xcframework にしてから使う。アプリの画面（App/）は Xcode のアプリのターゲットに入れる（README）。
import PackageDescription

let package = Package(
    name: "SencaSender",
    platforms: [.iOS(.v16), .macOS(.v13)],
    products: [
        .library(name: "SencaKit", targets: ["SencaKit"]),
    ],
    targets: [
        // libsenca_sdk.a と senca.h、module.modulemap（module CSenca）を含む
        .binaryTarget(name: "CSenca", path: "Frameworks/SencaSDK.xcframework"),
        .target(name: "SencaKit", dependencies: ["CSenca"]),
    ]
)
