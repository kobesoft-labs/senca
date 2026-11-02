package jp.co.kobesoft.senca.sender

import java.nio.ByteBuffer

/** SDK の C ABI（senca.h）への JNI。直接使わず [SencaSender] を使う。 */
internal object SencaNative {
    init {
        System.loadLibrary("senca_jni")
    }

    external fun abiVersion(): Int
    external fun create(config: String, listener: SenderListener?): Long
    external fun start(h: Long)
    external fun setVideoConfig(h: Long, codec: Int, record: ByteArray)
    external fun setAudioConfig(h: Long, codec: Int, sampleRate: Int, channels: Int, extra: ByteArray?)
    external fun pushVideo(h: Long, codec: Int, format: Int, buf: ByteBuffer, off: Int, size: Int, ptsUs: Long, dtsUs: Long, keyframe: Int): Int
    external fun pushAudio(h: Long, codec: Int, format: Int, buf: ByteBuffer, off: Int, size: Int, ptsUs: Long): Int
    external fun stats(h: Long): LongArray
    external fun stop(h: Long)
    external fun destroy(h: Long)
    external fun normalizeUri(input: String): String
    external fun viscaParse(packet: ByteArray, len: Int): IntArray?
    external fun viscaReply(kind: Int, socket: Int, errorCode: Int, data: ByteArray?, seq: Int): ByteArray
}
