import CSenca
import Foundation

/// SDK の失敗（`code` は senca.h の SENCA_ERR_*）。
public struct SencaError: Error, CustomStringConvertible {
    public let code: Int32
    public let message: String
    public var description: String { message }

    static func check(_ r: Int32) throws {
        if r < 0 { throw SencaError(code: r, message: String(cString: senca_last_error())) }
    }
}

public enum SenderState: Int32, Sendable {
    case idle = 0, connecting, connected, reconnecting, stopped, failed

    public var label: String {
        switch self {
        case .idle: "待機中"
        case .connecting: "接続中"
        case .connected: "送信中"
        case .reconnecting: "つなぎ直し中"
        case .stopped: "停止"
        case .failed: "失敗"
        }
    }
}

public enum SenderEvent: Sendable {
    case state(SenderState, reason: String?)
    case keyframeRequested
}

public enum VideoCodec: Int32 { case h264 = 1, h265 = 2 }
public enum AudioCodec: Int32 { case aac = 3, opus = 4 }

public struct SenderStats: Sendable {
    public var state: SenderState
    public var bytesSent: UInt64
    public var bitrateBps: UInt64
    public var videoSent: UInt64
    public var dropped: UInt64
    public var queueMs: UInt64
    /// SRT の往復時間（µs）。分からなければ nil。
    public var rttUs: Int64?
    public var reconnects: UInt64
}

/// 送り手（senca_sender を包む）。push はどのスレッドから呼んでもよい。
/// イベントは SDK の送信スレッドから届く（中で `stop` を呼ばないこと）。
public final class Sender: @unchecked Sendable {
    private var handle: OpaquePointer?
    private let box: Unmanaged<HandlerBox>

    private final class HandlerBox {
        let handler: (SenderEvent) -> Void
        init(_ h: @escaping (SenderEvent) -> Void) { handler = h }
    }

    /// `config` は senca://ingest?…・srt://…・rtmp://…/KEY。
    public init(config: String, onEvent: @escaping (SenderEvent) -> Void) throws {
        var opts = senca_sender_options()
        opts.struct_size = UInt32(MemoryLayout<senca_sender_options>.size)
        var h: OpaquePointer?
        try SencaError.check(senca_sender_create(config, &opts, &h))
        handle = h
        box = Unmanaged.passRetained(HandlerBox(onEvent))
        try SencaError.check(senca_sender_set_callback(h, { user, ev in
            guard let user, let ev else { return }
            let b = Unmanaged<HandlerBox>.fromOpaque(user).takeUnretainedValue()
            switch ev.pointee.type {
            case Int32(SENCA_EVENT_STATE_CHANGED):
                let reason = ev.pointee.reason.map { String(cString: $0) }
                b.handler(.state(SenderState(rawValue: ev.pointee.state) ?? .idle, reason: reason))
            case Int32(SENCA_EVENT_KEYFRAME_REQUESTED):
                b.handler(.keyframeRequested)
            default:
                break
            }
        }, box.toOpaque()))
    }

    deinit {
        if let h = handle { senca_sender_destroy(h) }
        box.release()
    }

    public func start() throws { try SencaError.check(senca_sender_start(handle)) }

    /// avcC / hvcC のレコード（VideoToolbox の format description の atoms から取れる）。
    public func setVideoConfig(_ codec: VideoCodec, record: Data) throws {
        try record.withUnsafeBytes { p in
            try SencaError.check(senca_sender_set_video_config(handle, codec.rawValue, p.bindMemory(to: UInt8.self).baseAddress, p.count))
        }
    }

    /// `extra` が nil なら SDK が作る（AAC-LC の AudioSpecificConfig / OpusHead）。
    public func setAudioConfig(_ codec: AudioCodec, sampleRate: UInt32, channels: UInt32, extra: Data? = nil) throws {
        let e = extra ?? Data()
        try e.withUnsafeBytes { p in
            try SencaError.check(senca_sender_set_audio_config(handle, codec.rawValue, sampleRate, channels,
                                                               p.bindMemory(to: UInt8.self).baseAddress, p.count))
        }
    }

    /// 4 バイト長さ前置（VideoToolbox の出力そのまま）。捨てたら false。
    @discardableResult
    public func pushVideo(_ codec: VideoCodec, lengthPrefixed data: UnsafeRawBufferPointer, ptsUs: Int64, dtsUs: Int64, keyframe: Bool) throws -> Bool {
        let r = senca_sender_push_video(handle, codec.rawValue, Int32(SENCA_FORMAT_LENGTH_PREFIXED),
                                        data.bindMemory(to: UInt8.self).baseAddress, data.count, ptsUs, dtsUs, keyframe ? 1 : 0)
        try SencaError.check(r)
        return r == SENCA_OK
    }

    @discardableResult
    public func pushAudio(_ codec: AudioCodec, raw data: UnsafeRawBufferPointer, ptsUs: Int64) throws -> Bool {
        let r = senca_sender_push_audio(handle, codec.rawValue, Int32(SENCA_FORMAT_RAW),
                                        data.bindMemory(to: UInt8.self).baseAddress, data.count, ptsUs)
        try SencaError.check(r)
        return r == SENCA_OK
    }

    public func stats() -> SenderStats? {
        var s = senca_stats()
        s.struct_size = UInt32(MemoryLayout<senca_stats>.size)
        guard senca_sender_get_stats(handle, &s) == SENCA_OK else { return nil }
        return SenderStats(state: SenderState(rawValue: s.state) ?? .idle, bytesSent: s.bytes_sent, bitrateBps: s.bitrate_bps,
                           videoSent: s.video_frames_sent, dropped: s.video_frames_dropped + s.audio_frames_dropped,
                           queueMs: s.queue_duration_ms, rttUs: s.rtt_us < 0 ? nil : s.rtt_us, reconnects: s.reconnects)
    }

    /// 送り残しを送って止める（ブロックする。メインスレッドから呼ばない）。
    public func stop() { senca_sender_stop(handle) }

    /// 設定文字列を検査して正規形（senca://…）にする。
    public static func normalize(_ config: String) throws -> String {
        var out: UnsafeMutablePointer<CChar>?
        try SencaError.check(senca_uri_normalize(config, &out))
        defer { senca_string_free(out) }
        return String(cString: out!)
    }
}
