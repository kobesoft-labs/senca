import SwiftUI

struct ContentView: View {
    @EnvironmentObject var model: SenderModel
    @State private var scanning = false

    var body: some View {
        Form {
            Section("状態") {
                Text(model.status)
                if !model.stats.isEmpty { Text(model.stats).font(.caption).monospacedDigit() }
            }
            Section("送り先") {
                TextField("senca://ingest?… / srt:// / rtmp://", text: $model.config, axis: .vertical)
                    .font(.system(.footnote, design: .monospaced))
                    .disabled(model.running)
                #if os(iOS)
                Button("QR コードを読む") { scanning = true }.disabled(model.running)
                #endif
            }
            Section("映像・音声") {
                Toggle("H.265（HEVC）", isOn: $model.hevc)
                Toggle("Opus（AAC の代わりに）", isOn: $model.opus)
                if model.dualSupported {
                    Toggle("前後のカメラを同時に送る", isOn: $model.dual)
                    if model.dual {
                        TextField("前面カメラの送り先", text: $model.config2, axis: .vertical)
                            .font(.system(.footnote, design: .monospaced))
                    }
                }
            }
            .disabled(model.running)
            Section {
                if model.running {
                    Button("停止", role: .destructive) { model.stop() }
                } else {
                    Button("送信開始") { model.start() }.disabled(model.config.isEmpty)
                }
            }
        }
        #if os(iOS)
        .sheet(isPresented: $scanning) {
            QRScannerView { text in
                model.config = text
                scanning = false
            }
            .ignoresSafeArea()
        }
        #endif
    }
}
