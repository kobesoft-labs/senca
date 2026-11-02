import AVFoundation
import SencaKit

/// カメラ・マイクの取り込みと符号化・送信をまとめる。前後同時（AVCaptureMultiCamSession、R9-8）では
/// 前面カメラを 2 つ目の送り先へ送る（SDK の 1 つの送り手は映像 1 本）。
final class CaptureEngine: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate, AVCaptureAudioDataOutputSampleBufferDelegate {
    static var multiCamSupported: Bool {
        #if os(iOS)
        AVCaptureMultiCamSession.isMultiCamSupported
        #else
        false
        #endif
    }

    let sender: Sender
    private let sender2: Sender?
    private let session: AVCaptureSession
    private let queue = DispatchQueue(label: "senca.capture")
    private let audioQueue = DispatchQueue(label: "senca.audio")
    private var encoders: [ObjectIdentifier: VideoEncoder] = [:]
    private var audio: AudioEncoder?
    private let hevc: Bool
    private let keyBox = KeyBox()

    /// 送り手のイベントからキーフレームの要求を符号化器へ渡す。
    private final class KeyBox: @unchecked Sendable {
        var encoders: [VideoEncoder] = []
    }

    init(config: String, config2: String?, hevc: Bool, opus: Bool, onState: @escaping (SenderState, String?) -> Void) throws {
        self.hevc = hevc
        let kb = keyBox
        sender = try Sender(config: config) { ev in
            switch ev {
            case let .state(s, reason): onState(s, reason)
            case .keyframeRequested: kb.encoders.first?.requestKeyframe()
            }
        }
        sender2 = try config2.map { c in
            try Sender(config: c) { ev in if case .keyframeRequested = ev { kb.encoders.last?.requestKeyframe() } }
        }
        #if os(iOS)
        session = sender2 != nil ? AVCaptureMultiCamSession() : AVCaptureSession()
        #else
        session = AVCaptureSession()
        #endif
        super.init()
        try configure(opus: opus)
    }

    private func configure(opus: Bool) throws {
        session.beginConfiguration()
        defer { session.commitConfiguration() }
        #if os(iOS)
        if !(session is AVCaptureMultiCamSession) { session.sessionPreset = .hd1280x720 }
        #else
        session.sessionPreset = .hd1280x720
        #endif
        // 後ろ（macOS は既定）のカメラ → 1 本目
        #if os(iOS)
        let backDevice = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .back)
        #else
        let backDevice = AVCaptureDevice.default(for: .video)
        #endif
        guard let back = backDevice else { throw SencaError(code: -3, message: "カメラが見つかりません") }
        try addVideo(device: back, sender: sender)
        #if os(iOS)
        if let s2 = sender2, let front = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .front) {
            try addVideo(device: front, sender: s2)
        }
        #endif
        if let mic = AVCaptureDevice.default(for: .audio) {
            let input = try AVCaptureDeviceInput(device: mic)
            if session.canAddInput(input) { session.addInput(input) }
            let out = AVCaptureAudioDataOutput()
            out.setSampleBufferDelegate(self, queue: audioQueue)
            if session.canAddOutput(out) { session.addOutput(out) }
            audio = try AudioEncoder(sender: sender, opus: opus)
        }
    }

    private func addVideo(device: AVCaptureDevice, sender target: Sender) throws {
        let input = try AVCaptureDeviceInput(device: device)
        let out = AVCaptureVideoDataOutput()
        out.alwaysDiscardsLateVideoFrames = true
        out.videoSettings = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange]
        out.setSampleBufferDelegate(self, queue: queue)
        #if os(iOS)
        if let multi = session as? AVCaptureMultiCamSession {
            // 複数カメラでは自動の接続を使わず、カメラと出力を 1 対 1 でつなぐ
            multi.addInputWithNoConnections(input)
            multi.addOutputWithNoConnections(out)
            guard let port = input.ports(for: .video, sourceDeviceType: device.deviceType, sourceDevicePosition: device.position).first
            else { throw SencaError(code: -3, message: "カメラの接続口がありません") }
            let conn = AVCaptureConnection(inputPorts: [port], output: out)
            guard multi.canAddConnection(conn) else { throw SencaError(code: -3, message: "この組み合わせは同時に使えません") }
            multi.addConnection(conn)
        } else {
            session.addInput(input)
            session.addOutput(out)
        }
        #else
        session.addInput(input)
        session.addOutput(out)
        #endif
        let dims = CMVideoFormatDescriptionGetDimensions(device.activeFormat.formatDescription)
        let w = min(dims.width, 1280), h = min(dims.height, 720)
        let enc = try VideoEncoder(sender: target, hevc: hevc, width: w, height: h, bitrate: 3_000_000)
        encoders[ObjectIdentifier(out)] = enc
        keyBox.encoders.append(enc)
    }

    func start() throws {
        try sender.start()
        try sender2?.start()
        queue.async { self.session.startRunning() }
    }

    /// ブロックする（メインスレッドから呼ばない）。
    func stop() {
        session.stopRunning()
        sender.stop()
        sender2?.stop()
    }

    func captureOutput(_ output: AVCaptureOutput, didOutput sb: CMSampleBuffer, from connection: AVCaptureConnection) {
        if output is AVCaptureAudioDataOutput {
            audio?.encode(sb)
        } else {
            encoders[ObjectIdentifier(output)]?.encode(sb)
        }
    }
}
