import SencaKit
import SwiftUI

/// SENCA 送信のアプリ（iOS / iPadOS / macOS）。`senca://ingest?…` のリンクで送り先を受ける（Info.plist の CFBundleURLTypes）。
@main
struct SenderApp: App {
    @StateObject private var model = SenderModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
                .onOpenURL { url in
                    if url.scheme == "senca" { model.config = url.absoluteString }
                }
        }
    }
}
