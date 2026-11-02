package jp.co.kobesoft.senca.sender

import androidx.camera.core.Camera
import androidx.camera.core.FocusMeteringAction
import androidx.camera.core.SurfaceOrientedMeteringPointFactory
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress

/**
 * VISCA over IP の受け口（UDP 52381、REQ-009 カメラ制御）。操作盤・スイッチャーからのズーム・フォーカス・
 * プリセットをこの端末のカメラに当てる。パケットの解釈と返事の組み立ては SDK（C ABI）で行う。
 *
 * できる操作: 電源（無視して ACK）、ズーム（連続・位置）、オートフォーカス（ワンプッシュ）、プリセット（ズームの値）、
 * ズーム位置の問い合わせ。他（手動フォーカス・露出のモード・パン/チルト）は「実行できない」（41）を返す
 * （黙って無視しない、REQ-008 R8-13）。パン・チルトの切り抜きによる模擬は後続。
 */
class ViscaReceiver(private val camera: () -> Camera?, private val port: Int = 52381) {
    private var socket: DatagramSocket? = null
    private val presets = HashMap<Int, Float>()
    @Volatile private var zoomStep = 0f

    fun start() {
        val s = DatagramSocket(null).apply { reuseAddress = true; bind(InetSocketAddress(port)) }
        socket = s
        Thread({ loop(s) }, "senca-visca").start()
        Thread({ zoomLoop() }, "senca-visca-zoom").start()
    }

    fun stop() {
        socket?.close()
        socket = null
    }

    private fun loop(s: DatagramSocket) {
        val buf = ByteArray(64)
        while (!s.isClosed) {
            val p = DatagramPacket(buf, buf.size)
            runCatching { s.receive(p) }.onFailure { return }
            val v = SencaNative.viscaParse(p.data, p.length) ?: continue // 壊れたパケットは返事をしない
            val seq = v[0]
            fun reply(kind: Int, code: Int = 0, data: ByteArray? = null) {
                val out = SencaNative.viscaReply(kind, 1, code, data, seq)
                runCatching { s.send(DatagramPacket(out, out.size, p.socketAddress)) }
            }
            val done = apply(v[1], v[2], v[3])
            when {
                done == null -> reply(REPLY_ERROR, NOT_EXECUTABLE)
                done.isEmpty() -> { reply(REPLY_ACK); reply(REPLY_COMPLETION) }
                else -> reply(REPLY_COMPLETION, data = done) // 問い合わせは ACK なしで値を返す
            }
        }
    }

    /** 実行できたら空の配列（問い合わせなら値）、できなければ null。 */
    private fun apply(kind: Int, a: Int, b: Int): ByteArray? {
        val cam = camera() ?: return null
        val zs = cam.cameraInfo.zoomState.value ?: return null
        when (kind) {
            1 -> Unit // 電源
            2 -> zoomStep = 0f
            3 -> zoomStep = 0.02f * (if (a < 0) 3 else a + 1)
            4 -> zoomStep = -0.02f * (if (a < 0) 3 else a + 1)
            5 -> cam.cameraControl.setLinearZoom((a / 16384f).coerceIn(0f, 1f))
            10, 12 -> {
                val pt = SurfaceOrientedMeteringPointFactory(1f, 1f).createPoint(.5f, .5f)
                cam.cameraControl.startFocusAndMetering(FocusMeteringAction.Builder(pt).build())
            }
            15 -> presets.remove(a)
            16 -> presets[a] = zs.linearZoom
            17 -> cam.cameraControl.setLinearZoom(presets[a] ?: return null)
            20 -> {
                val pos = (zs.linearZoom * 16384).toInt()
                return byteArrayOf((pos shr 12 and 15).toByte(), (pos shr 8 and 15).toByte(), (pos shr 4 and 15).toByte(), (pos and 15).toByte())
            }
            else -> return null
        }
        return ByteArray(0)
    }

    /** 連続ズーム（Tele/Wide を Stop まで続ける）。 */
    private fun zoomLoop() {
        while (socket != null) {
            val step = zoomStep
            if (step != 0f) {
                camera()?.let { c ->
                    val z = c.cameraInfo.zoomState.value?.linearZoom ?: 0f
                    c.cameraControl.setLinearZoom((z + step).coerceIn(0f, 1f))
                }
            }
            Thread.sleep(50)
        }
    }

    private companion object {
        const val REPLY_ACK = 1
        const val REPLY_COMPLETION = 2
        const val REPLY_ERROR = 3
        const val NOT_EXECUTABLE = 0x41
    }
}
