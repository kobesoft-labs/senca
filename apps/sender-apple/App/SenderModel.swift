import AVFoundation
import SencaKit
import SwiftUI

/// 画面の状態と送信の開始・停止。
@MainActor
final class SenderModel: ObservableObject {
    @Published var config = ""
    @Published var config2 = ""
    @Published var status = "待機中"
    @Published var stats = ""
    @Published var hevc = false
    @Published var opus = false
    @Published var dual = false
    @Published var running = false

    private var engine: CaptureEngine?
    private var timer: Timer?

    var dualSupported: Bool { CaptureEngine.multiCamSupported }

    func start() {
        do {
            _ = try Sender.normalize(config)
            let e = try CaptureEngine(config: config, config2: dual && !config2.isEmpty ? config2 : nil,
                                      hevc: hevc, opus: opus) { [weak self] state, reason in
                Task { @MainActor in self?.status = state.label + (reason.map { "（\($0)）" } ?? "") }
            }
            try e.start()
            engine = e
            running = true
            timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
                Task { @MainActor in self?.refreshStats() }
            }
        } catch {
            status = "開始できません: \(error)"
        }
    }

    func stop() {
        timer?.invalidate()
        let e = engine
        engine = nil
        running = false
        Task.detached { e?.stop() }
        status = "停止"
    }

    private func refreshStats() {
        guard let s = engine?.sender.stats() else { return }
        let rtt = s.rttUs.map { "\($0 / 1000) ms" } ?? "—"
        stats = String(format: "%.2f Mbps・映像 %llu・落とした %llu・往復 %@・つなぎ直し %llu",
                       Double(s.bitrateBps) / 1e6, s.videoSent, s.dropped, rtt, s.reconnects)
    }
}
