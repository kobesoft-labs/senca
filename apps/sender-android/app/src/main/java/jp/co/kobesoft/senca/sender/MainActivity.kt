package jp.co.kobesoft.senca.sender

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.camera.view.PreviewView
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * 送信の画面。送り先は文字で入れるか、senca://ingest?… のリンク（QR を読んだ他のアプリ・カメラアプリから）で開く。
 * QR の読み取りはこのアプリには入れていない（README 参照）。
 */
class MainActivity : ComponentActivity() {
    private val config = mutableStateOf("")
    private val permissions = registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {}

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        handleIntent(intent)
        permissions.launch(arrayOf(Manifest.permission.CAMERA, Manifest.permission.RECORD_AUDIO))
        setContent { MaterialTheme { SenderScreen(config) } }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        handleIntent(intent)
    }

    private fun handleIntent(i: Intent?) {
        val uri = i?.data ?: return
        if (uri.scheme == "senca") config.value = uri.toString()
    }

    @Composable
    private fun SenderScreen(config: MutableState<String>) {
        val scope = rememberCoroutineScope()
        val camera = remember { CameraController(this, this) }
        var status by remember { mutableStateOf("待機中") }
        var stats by remember { mutableStateOf("") }
        var config2 by remember { mutableStateOf("") }
        var hevc by remember { mutableStateOf(false) }
        var opus by remember { mutableStateOf(false) }
        var back by remember { mutableStateOf(true) }
        var dual by remember { mutableStateOf(false) }
        var visca by remember { mutableStateOf(false) }
        var session by remember { mutableStateOf<Session?>(null) }
        var dualSupported by remember { mutableStateOf(false) }
        val previewView = remember { PreviewView(this) }

        LaunchedEffect(Unit) { camera.open { dualSupported = camera.concurrentSupported() } }
        LaunchedEffect(session) {
            while (session != null) {
                session?.sender?.runCatching { stats() }?.getOrNull()?.let { s ->
                    stats = "%.2f Mbps・映像 %d・落とした %d・往復 %s・つなぎ直し %d".format(
                        s.bitrateBps / 1e6, s.videoSent, s.videoDropped + s.audioDropped,
                        if (s.rttUs < 0) "—" else "${s.rttUs / 1000} ms", s.reconnects)
                }
                delay(1000)
            }
        }

        Column(Modifier.fillMaxSize().padding(12.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            AndroidView({ previewView }, Modifier.fillMaxWidth().aspectRatio(16f / 9f))
            Text("状態: $status")
            if (stats.isNotEmpty()) Text(stats, style = MaterialTheme.typography.bodySmall)
            OutlinedTextField(config.value, { config.value = it }, Modifier.fillMaxWidth(),
                label = { Text("送り先（senca://ingest?… / srt:// / rtmp://）") }, enabled = session == null)
            Row { Checkbox(hevc, { hevc = it }); Text("H.265"); Checkbox(opus, { opus = it }); Text("Opus") }
            Row { Checkbox(back, { back = it }); Text("背面カメラ"); Checkbox(visca, { visca = it }); Text("VISCA で操作を受ける") }
            if (dualSupported) {
                Row { Checkbox(dual, { dual = it }); Text("前後のカメラを同時に送る") }
                if (dual) OutlinedTextField(config2, { config2 = it }, Modifier.fillMaxWidth(),
                    label = { Text("前面カメラの送り先") }, enabled = session == null)
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(enabled = session == null, onClick = {
                    if (ContextCompat.checkSelfPermission(this@MainActivity, Manifest.permission.CAMERA) != PackageManager.PERMISSION_GRANTED) {
                        status = "カメラ・マイクの利用を許可してください"; return@Button
                    }
                    try {
                        session = Session.start(this@MainActivity, camera, previewView, config.value.trim(),
                            config2.trim().takeIf { dual && it.isNotEmpty() }, hevc, opus, back, visca) { s, reason ->
                            runOnUiThread { status = s.label + (reason?.let { "（$it）" } ?: "") }
                        }
                    } catch (e: SencaException) {
                        status = "設定が正しくありません: ${e.message}"
                    }
                }) { Text("送信開始") }
                Button(enabled = session != null, onClick = {
                    val s = session
                    session = null
                    scope.launch { withContext(Dispatchers.IO) { s?.close() }; status = "停止" }
                }) { Text("停止") }
            }
        }
    }
}

/** 1 回の送信（送り手・符号化器・VISCA の受け口をまとめて持つ）。 */
class Session private constructor(
    val sender: SencaSender,
    private val sender2: SencaSender?,
    private val camera: CameraController,
) : AutoCloseable {
    /** 送り手の番号（0 = 主、1 = 前面カメラ）ごとの符号化器。 */
    private val encoders = HashMap<Int, VideoEncoder>()
    private var audio: AudioEncoder? = null
    private var visca: ViscaReceiver? = null

    override fun close() {
        visca?.stop()
        camera.unbind()
        audio?.release()
        encoders.values.forEach { it.release() }
        sender.close()
        sender2?.close()
    }

    companion object {
        fun start(
            activity: MainActivity, camera: CameraController, view: PreviewView, config: String, config2: String?,
            hevc: Boolean, opus: Boolean, back: Boolean, visca: Boolean, onState: (SenderState, String?) -> Unit,
        ): Session {
            SencaSender.normalize(config) // 先に検査（正しくなければ SencaException）
            var s: Session? = null
            fun listener(i: Int) = SenderListener { type, state, reason ->
                when (type) {
                    SencaSender.EVENT_STATE_CHANGED -> if (i == 0) onState(SenderState.of(state), reason)
                    SencaSender.EVENT_KEYFRAME_REQUESTED -> s?.encoders?.get(i)?.requestKeyframe()
                }
            }
            val main = SencaSender(config, listener(0))
            val second = config2?.let { SencaSender(it, listener(1)) }
            val session = Session(main, second, camera).also { s = it }
            fun enc(target: SencaSender, i: Int) = { w: Int, h: Int ->
                VideoEncoder(target, hevc, w, h, 30, if (h >= 1080) 6_000_000 else 3_000_000).also { session.encoders[i] = it }
            }
            if (second == null || !camera.bindDual(view, 1280, 720, enc(main, 0), enc(second, 1))) {
                camera.bindSingle(view, back, 1280, 720, enc(main, 0))
            }
            session.audio = AudioEncoder(main, opus, if (opus) 64_000 else 128_000).also { it.start() }
            if (visca) session.visca = ViscaReceiver({ camera.camera }).also { it.start() }
            main.start()
            second?.start()
            return session
        }
    }
}
