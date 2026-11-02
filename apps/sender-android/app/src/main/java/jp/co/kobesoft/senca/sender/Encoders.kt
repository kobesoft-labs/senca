package jp.co.kobesoft.senca.sender

import android.annotation.SuppressLint
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaFormat
import android.media.MediaRecorder
import android.os.Build
import android.os.Bundle
import androidx.camera.core.ImageProxy
import java.util.concurrent.atomic.AtomicBoolean

/**
 * 映像の符号化（MediaCodec のハードウェア H.264 / H.265）。CameraX の ImageAnalysis の YUV を入れ、
 * 出力（Annex B。最初の CODEC_CONFIG に SPS/PPS(/VPS)）をそのまま SDK に渡す。B フレームは使わない（DTS = PTS）。
 */
class VideoEncoder(
    private val sender: SencaSender,
    val hevc: Boolean,
    private val width: Int,
    private val height: Int,
    fps: Int,
    bitrate: Int,
) {
    private val mime = if (hevc) MediaFormat.MIMETYPE_VIDEO_HEVC else MediaFormat.MIMETYPE_VIDEO_AVC
    private val codecId = if (hevc) SencaSender.CODEC_H265 else SencaSender.CODEC_H264
    private val codec: MediaCodec = MediaCodec.createEncoderByType(mime)
    private val running = AtomicBoolean(true)

    init {
        val f = MediaFormat.createVideoFormat(mime, width, height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatYUV420Flexible)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
            if (Build.VERSION.SDK_INT >= 29) setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
        }
        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) = Unit // 入力は queueFrame で入れる
            override fun onOutputBufferAvailable(c: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                val buf = c.getOutputBuffer(index)
                if (buf != null && info.size > 0 && running.get()) {
                    val key = if (info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0) 1 else SencaSender.KEYFRAME_AUTO
                    runCatching {
                        sender.pushVideo(codecId, SencaSender.FORMAT_ANNEXB, buf, info.offset, info.size,
                            info.presentationTimeUs, info.presentationTimeUs, key)
                    }
                }
                c.releaseOutputBuffer(index, false)
            }
            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) = Unit
            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) = Unit
        })
        codec.configure(f, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        codec.start()
    }

    /** ImageAnalysis の 1 フレームを入れる（空きが無ければ捨てる＝ライブ優先）。 */
    fun queueFrame(image: ImageProxy) {
        image.use {
            if (!running.get()) return
            val index = codec.dequeueInputBuffer(0)
            if (index < 0) return
            val dst = codec.getInputImage(index) ?: return
            // Y・U・V の各面を行ごとに写す（画素の間隔が違ってもよいように）
            for (p in 0 until 3) {
                val s = image.planes[p]
                val d = dst.planes[p]
                val w = if (p == 0) width else width / 2
                val h = if (p == 0) height else height / 2
                for (y in 0 until h) for (x in 0 until w) {
                    d.buffer.put(y * d.rowStride + x * d.pixelStride, s.buffer.get(y * s.rowStride + x * s.pixelStride))
                }
            }
            // 映像と音声は同じ単調な時計（CLOCK_MONOTONIC の µs）にそろえる。カメラの時刻源が REALTIME の
            // 機種（SENSOR_INFO_TIMESTAMP_SOURCE）は CameraController が差を足して渡す
            codec.queueInputBuffer(index, 0, width * height * 3 / 2, image.imageInfo.timestamp / 1000, 0)
        }
    }

    /** SDK がキーフレームを頼んだとき。 */
    fun requestKeyframe() {
        codec.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
    }

    fun release() {
        running.set(false)
        runCatching { codec.stop() }
        codec.release()
    }
}

/**
 * 音声の符号化（AudioRecord → MediaCodec の AAC-LC か Opus）。CODEC_CONFIG（AAC: AudioSpecificConfig、
 * Opus: OpusHead）は設定として、他は raw のフレームとして SDK に渡す。
 */
class AudioEncoder(private val sender: SencaSender, private val opus: Boolean, bitrate: Int) {
    private val rate = 48_000
    private val channels = 1
    private val codecId = if (opus) SencaSender.CODEC_OPUS else SencaSender.CODEC_AAC
    private val codec = MediaCodec.createEncoderByType(if (opus) MediaFormat.MIMETYPE_AUDIO_OPUS else MediaFormat.MIMETYPE_AUDIO_AAC)
    private val running = AtomicBoolean(true)
    private var thread: Thread? = null

    init {
        val f = MediaFormat.createAudioFormat(codec.codecInfo.supportedTypes[0], rate, channels).apply {
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            if (!opus) setInteger(MediaFormat.KEY_AAC_PROFILE, MediaCodecInfo.CodecProfileLevel.AACObjectLC)
        }
        codec.configure(f, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        codec.start()
    }

    @SuppressLint("MissingPermission") // 呼び出し側で RECORD_AUDIO を確かめる
    fun start() {
        val min = AudioRecord.getMinBufferSize(rate, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT)
        val rec = AudioRecord(MediaRecorder.AudioSource.CAMCORDER, rate, AudioFormat.CHANNEL_IN_MONO,
            AudioFormat.ENCODING_PCM_16BIT, maxOf(min, 8192))
        thread = Thread({
            rec.startRecording()
            val info = MediaCodec.BufferInfo()
            while (running.get()) {
                val i = codec.dequeueInputBuffer(10_000)
                if (i >= 0) {
                    val buf = codec.getInputBuffer(i)!!
                    val n = rec.read(buf, buf.remaining())
                    codec.queueInputBuffer(i, 0, maxOf(n, 0), System.nanoTime() / 1000, 0)
                }
                drain(info)
            }
            rec.stop()
            rec.release()
        }, "senca-audio").also { it.start() }
    }

    private fun drain(info: MediaCodec.BufferInfo) {
        while (true) {
            val o = codec.dequeueOutputBuffer(info, 0)
            if (o < 0) return
            val buf = codec.getOutputBuffer(o)
            if (buf != null && info.size > 0) {
                runCatching {
                    if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0) {
                        val cfg = ByteArray(info.size)
                        buf.position(info.offset)
                        buf.get(cfg)
                        sender.setAudioConfig(codecId, rate, channels, if (opus) opusHead(cfg) else cfg)
                    } else {
                        sender.pushAudio(codecId, SencaSender.FORMAT_RAW, buf, info.offset, info.size, info.presentationTimeUs)
                    }
                }
            }
            codec.releaseOutputBuffer(o, false)
        }
    }

    /**
     * Android の Opus 符号化器の設定は "AOPUSHDR" などの印で区切った独自の形で、中に OpusHead（19 バイト）がある。
     * 見つからなければ null（SDK が作る）。
     */
    private fun opusHead(cfg: ByteArray): ByteArray? {
        val magic = "OpusHead".toByteArray()
        for (i in 0..cfg.size - 19) {
            if (cfg.copyOfRange(i, i + 8).contentEquals(magic)) return cfg.copyOfRange(i, i + 19)
        }
        return null
    }

    fun release() {
        running.set(false)
        thread?.join(1000)
        runCatching { codec.stop() }
        codec.release()
    }
}
