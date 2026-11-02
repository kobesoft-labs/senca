import AVFoundation
import SencaKit
import VideoToolbox

/// 映像の符号化（VideoToolbox のハードウェア H.264 / HEVC）。並べ替え（B フレーム）は使わない（DTS = PTS）。
/// 出力は 4 バイト長さ前置のまま SDK に渡し、設定は format description の avcC / hvcC をそのまま渡す。
final class VideoEncoder {
    private let sender: Sender
    private let codec: VideoCodec
    private var session: VTCompressionSession?
    private var lastRecord: Data?
    private var forceKey = true
    private let lock = NSLock()

    init(sender: Sender, hevc: Bool, width: Int32, height: Int32, bitrate: Int, fps: Int = 30) throws {
        self.sender = sender
        codec = hevc ? .h265 : .h264
        var s: VTCompressionSession?
        let st = VTCompressionSessionCreate(allocator: nil, width: width, height: height,
                                            codecType: hevc ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264,
                                            encoderSpecification: nil, imageBufferAttributes: nil, compressedDataAllocator: nil,
                                            outputCallback: nil, refcon: nil, compressionSessionOut: &s)
        guard st == noErr, let s else { throw SencaError(code: -3, message: "符号化器を作れません（\(st)）") }
        let props: [CFString: Any] = [
            kVTCompressionPropertyKey_RealTime: true,
            kVTCompressionPropertyKey_AllowFrameReordering: false,
            kVTCompressionPropertyKey_AverageBitRate: bitrate,
            kVTCompressionPropertyKey_ExpectedFrameRate: fps,
            kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: 2,
            kVTCompressionPropertyKey_ProfileLevel: hevc ? kVTProfileLevel_HEVC_Main_AutoLevel : kVTProfileLevel_H264_Main_AutoLevel,
        ]
        VTSessionSetProperties(s, propertyDictionary: props as CFDictionary)
        VTCompressionSessionPrepareToEncodeFrames(s)
        session = s
    }

    deinit {
        if let s = session { VTCompressionSessionInvalidate(s) }
    }

    func requestKeyframe() {
        lock.lock(); forceKey = true; lock.unlock()
    }

    func encode(_ sample: CMSampleBuffer) {
        guard let s = session, let img = CMSampleBufferGetImageBuffer(sample) else { return }
        lock.lock(); let key = forceKey; forceKey = false; lock.unlock()
        let opts: CFDictionary? = key ? [kVTEncodeFrameOptionKey_ForceKeyFrame: true] as CFDictionary : nil
        VTCompressionSessionEncodeFrame(s, imageBuffer: img, presentationTimeStamp: CMSampleBufferGetPresentationTimeStamp(sample),
                                        duration: .invalid, frameProperties: opts, infoFlagsOut: nil) { [weak self] status, _, out in
            guard status == noErr, let out, let self else { return }
            self.output(out)
        }
    }

    private func output(_ sb: CMSampleBuffer) {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sb, createIfNecessary: false) as? [[CFString: Any]]
        let keyframe = !(attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool ?? false)
        if keyframe, let fmt = CMSampleBufferGetFormatDescription(sb),
           let atoms = CMFormatDescriptionGetExtension(fmt, extensionKey: kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms) as? [String: Any],
           let record = (atoms["avcC"] ?? atoms["hvcC"]) as? Data, record != lastRecord {
            try? sender.setVideoConfig(codec, record: record)
            lastRecord = record
        }
        guard let block = CMSampleBufferGetDataBuffer(sb) else { return }
        var data = Data(count: CMBlockBufferGetDataLength(block))
        data.withUnsafeMutableBytes { p in
            _ = CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: p.count, destination: p.baseAddress!)
        }
        let pts = CMTimeConvertScale(CMSampleBufferGetPresentationTimeStamp(sb), timescale: 1_000_000, method: .roundHalfAwayFromZero).value
        data.withUnsafeBytes { p in
            _ = try? sender.pushVideo(codec, lengthPrefixed: p, ptsUs: pts, dtsUs: pts, keyframe: keyframe)
        }
    }
}

/// 音声の符号化（AVAudioConverter で AAC-LC か Opus、48 kHz モノラル）。設定は SDK に作らせる。
final class AudioEncoder {
    private let sender: Sender
    private let codec: AudioCodec
    private let outFormat: AVAudioFormat
    private let samplesPerPacket: Int64
    private var converter: AVAudioConverter?
    private var pending: [AVAudioPCMBuffer] = []
    private var firstPtsUs: Int64?
    private var samplesOut: Int64 = 0

    init(sender: Sender, opus: Bool) throws {
        self.sender = sender
        codec = opus ? .opus : .aac
        samplesPerPacket = opus ? 960 : 1024
        guard let f = AVAudioFormat(settings: [
            AVFormatIDKey: opus ? kAudioFormatOpus : kAudioFormatMPEG4AAC,
            AVSampleRateKey: 48_000,
            AVNumberOfChannelsKey: 1,
        ]) else { throw SencaError(code: -3, message: "音声の形式を作れません") }
        outFormat = f
        try sender.setAudioConfig(codec, sampleRate: 48_000, channels: 1)
    }

    func encode(_ sb: CMSampleBuffer) {
        guard let desc = CMSampleBufferGetFormatDescription(sb) else { return }
        let inFormat = AVAudioFormat(cmAudioFormatDescription: desc)
        if converter == nil {
            converter = AVAudioConverter(from: inFormat, to: outFormat)
            converter?.bitRate = codec == .opus ? 64_000 : 128_000
            firstPtsUs = CMTimeConvertScale(CMSampleBufferGetPresentationTimeStamp(sb), timescale: 1_000_000, method: .roundHalfAwayFromZero).value
        }
        let frames = AVAudioFrameCount(CMSampleBufferGetNumSamples(sb))
        guard let pcm = AVAudioPCMBuffer(pcmFormat: inFormat, frameCapacity: frames) else { return }
        pcm.frameLength = frames
        guard CMSampleBufferCopyPCMDataIntoAudioBufferList(sb, at: 0, frameCount: Int32(frames), into: pcm.mutableAudioBufferList) == noErr else { return }
        pending.append(pcm)
        drain()
    }

    private func drain() {
        guard let conv = converter else { return }
        let out = AVAudioCompressedBuffer(format: outFormat, packetCapacity: 8, maximumPacketSize: max(conv.maximumOutputPacketSize, 1536))
        while true {
            var err: NSError?
            let status = conv.convert(to: out, error: &err) { _, inStatus in
                if self.pending.isEmpty {
                    inStatus.pointee = .noDataNow
                    return nil
                }
                inStatus.pointee = .haveData
                return self.pending.removeFirst()
            }
            for i in 0..<Int(out.packetCount) {
                guard let d = out.packetDescriptions?[i] else { continue }
                let p = UnsafeRawBufferPointer(start: out.data.advanced(by: Int(d.mStartOffset)), count: Int(d.mDataByteSize))
                let pts = (firstPtsUs ?? 0) + samplesOut * 1_000_000 / 48_000
                _ = try? sender.pushAudio(codec, raw: p, ptsUs: pts)
                samplesOut += samplesPerPacket
            }
            if status != .haveData || out.packetCount == 0 { break }
        }
    }
}
