#if os(iOS)
import AVFoundation
import SwiftUI
import UIKit

/// QR コードを読む（AVFoundation の標準の metadata output。外部の部品は使わない）。
/// senca://ingest?… か srt:// / rtmp:// / https:// の文字列だけを受け取る。
struct QRScannerView: UIViewControllerRepresentable {
    let onFound: (String) -> Void

    func makeUIViewController(context: Context) -> ScannerController {
        let c = ScannerController()
        c.onFound = onFound
        return c
    }

    func updateUIViewController(_ vc: ScannerController, context: Context) {}

    final class ScannerController: UIViewController, AVCaptureMetadataOutputObjectsDelegate {
        var onFound: ((String) -> Void)?
        private let session = AVCaptureSession()
        private var done = false

        override func viewDidLoad() {
            super.viewDidLoad()
            guard let cam = AVCaptureDevice.default(for: .video), let input = try? AVCaptureDeviceInput(device: cam),
                  session.canAddInput(input) else { return }
            session.addInput(input)
            let out = AVCaptureMetadataOutput()
            guard session.canAddOutput(out) else { return }
            session.addOutput(out)
            out.setMetadataObjectsDelegate(self, queue: .main)
            out.metadataObjectTypes = [.qr]
            let layer = AVCaptureVideoPreviewLayer(session: session)
            layer.videoGravity = .resizeAspectFill
            layer.frame = view.bounds
            view.layer.addSublayer(layer)
            DispatchQueue.global(qos: .userInitiated).async { self.session.startRunning() }
        }

        override func viewWillDisappear(_ animated: Bool) {
            super.viewWillDisappear(animated)
            session.stopRunning()
        }

        func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput objects: [AVMetadataObject], from connection: AVCaptureConnection) {
            guard !done, let s = (objects.first as? AVMetadataMachineReadableCodeObject)?.stringValue else { return }
            let lower = s.lowercased()
            guard ["senca://", "srt://", "rtmp://", "rtmps://", "https://"].contains(where: lower.hasPrefix) else { return }
            done = true
            onFound?(s)
        }
    }
}
#endif
