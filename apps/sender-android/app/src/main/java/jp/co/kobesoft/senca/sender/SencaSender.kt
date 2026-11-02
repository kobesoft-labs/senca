package jp.co.kobesoft.senca.sender

import java.nio.ByteBuffer

/** SDK の失敗（[code] は senca.h の SENCA_ERR_*、[message] は理由。秘密は含まない）。 */
class SencaException(val code: Int, message: String) : Exception(message)

/** SDK の送信スレッドから呼ばれる。中で [SencaSender.close] を呼ばないこと。 */
fun interface SenderListener {
    fun onEvent(type: Int, state: Int, reason: String?)
}

/** 状態（senca.h の SENCA_STATE_*）。 */
enum class SenderState(val value: Int, val label: String) {
    IDLE(0, "待機中"), CONNECTING(1, "接続中"), CONNECTED(2, "送信中"),
    RECONNECTING(3, "つなぎ直し中"), STOPPED(4, "停止"), FAILED(5, "失敗");

    companion object {
        fun of(v: Int) = entries.firstOrNull { it.value == v } ?: IDLE
    }
}

data class SenderStats(
    val state: SenderState, val bytesSent: Long, val bitrateBps: Long, val videoSent: Long, val audioSent: Long,
    val videoDropped: Long, val audioDropped: Long, val queueMs: Long, val rttUs: Long, val reconnects: Long,
)

/**
 * 送り手（SDK の senca_sender を包む）。[config] は senca://ingest?…・srt://…・rtmp://…/KEY。
 * push* はどのスレッドから呼んでもよい。使い終わったら [close]。
 */
class SencaSender(config: String, listener: SenderListener? = null) : AutoCloseable {
    private var handle: Long = SencaNative.create(config, listener)

    private fun h(): Long = handle.takeIf { it != 0L } ?: throw IllegalStateException("closed")

    fun start() = SencaNative.start(h())

    fun setVideoConfig(codec: Int, record: ByteArray) = SencaNative.setVideoConfig(h(), codec, record)

    fun setAudioConfig(codec: Int, sampleRate: Int, channels: Int, extra: ByteArray?) =
        SencaNative.setAudioConfig(h(), codec, sampleRate, channels, extra)

    /** [buf] は direct ByteBuffer（MediaCodec の出力）。捨てたら false。 */
    fun pushVideo(codec: Int, format: Int, buf: ByteBuffer, off: Int, size: Int, ptsUs: Long, dtsUs: Long, keyframe: Int): Boolean =
        SencaNative.pushVideo(h(), codec, format, buf, off, size, ptsUs, dtsUs, keyframe) == OK

    fun pushAudio(codec: Int, format: Int, buf: ByteBuffer, off: Int, size: Int, ptsUs: Long): Boolean =
        SencaNative.pushAudio(h(), codec, format, buf, off, size, ptsUs) == OK

    fun stats(): SenderStats {
        val v = SencaNative.stats(h())
        return SenderStats(SenderState.of(v[0].toInt()), v[1], v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9])
    }

    /** 送り残しを送って止める（ブロックする。UI スレッドから呼ばない）。 */
    override fun close() {
        val x = handle
        handle = 0
        if (x != 0L) {
            SencaNative.stop(x)
            SencaNative.destroy(x)
        }
    }

    companion object {
        const val OK = 0
        const val CODEC_H264 = 1
        const val CODEC_H265 = 2
        const val CODEC_AAC = 3
        const val CODEC_OPUS = 4
        const val FORMAT_ANNEXB = 1
        const val FORMAT_LENGTH_PREFIXED = 2
        const val FORMAT_RAW = 3
        const val FORMAT_ADTS = 4
        const val KEYFRAME_AUTO = -1
        const val EVENT_STATE_CHANGED = 1
        const val EVENT_KEYFRAME_REQUESTED = 2

        /** 設定文字列を検査して正規形（senca://…）にする。正しくなければ [SencaException]。 */
        fun normalize(config: String): String = SencaNative.normalizeUri(config)
    }
}
