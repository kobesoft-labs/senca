package jp.co.kobesoft.senca.sender

import android.content.Context
import android.hardware.camera2.CameraCharacteristics
import android.os.SystemClock
import android.util.Size
import androidx.annotation.OptIn
import androidx.camera.camera2.interop.Camera2CameraInfo
import androidx.camera.camera2.interop.ExperimentalCamera2Interop
import androidx.camera.core.Camera
import androidx.camera.core.CameraSelector
import androidx.camera.core.ConcurrentCamera.SingleCameraConfig
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.Preview
import androidx.camera.core.UseCaseGroup
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.core.resolutionselector.ResolutionSelector
import androidx.camera.core.resolutionselector.ResolutionStrategy
import androidx.camera.view.PreviewView
import androidx.core.content.ContextCompat
import androidx.lifecycle.LifecycleOwner
import java.util.concurrent.Executors

/**
 * CameraX でカメラを開き、ImageAnalysis（YUV）を符号化器へ、Preview を画面へ流す。
 * 前後のカメラを同時に使える機種（CameraX の concurrent camera）では、2 台をそれぞれの符号化器へ流す（R9-8）。
 * 2 本目は別の送り先（2 つ目の設定）へ送る。SDK の 1 つの送り手は映像 1 本なので。
 */
class CameraController(private val ctx: Context, private val owner: LifecycleOwner) {
    private val exec = Executors.newFixedThreadPool(2)
    private var provider: ProcessCameraProvider? = null
    var camera: Camera? = null
        private set

    fun open(onReady: (ProcessCameraProvider) -> Unit) {
        val f = ProcessCameraProvider.getInstance(ctx)
        f.addListener({ provider = f.get(); onReady(f.get()) }, ContextCompat.getMainExecutor(ctx))
    }

    /** 前後同時に使えるか。 */
    fun concurrentSupported(): Boolean = provider?.availableConcurrentCameraInfos?.isNotEmpty() == true

    private fun analysis(width: Int, height: Int, encoder: (Int, Int) -> VideoEncoder, clockOffsetNs: () -> Long): ImageAnalysis {
        var enc: VideoEncoder? = null
        return ImageAnalysis.Builder()
            .setOutputImageFormat(ImageAnalysis.OUTPUT_IMAGE_FORMAT_YUV_420_888)
            .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
            .setResolutionSelector(ResolutionSelector.Builder().setResolutionStrategy(
                ResolutionStrategy(Size(width, height), ResolutionStrategy.FALLBACK_RULE_CLOSEST_LOWER_THEN_HIGHER)).build())
            .build().also { a ->
                a.setAnalyzer(exec) { img ->
                    // 符号化器は実際の画の大きさで作る
                    val e = enc ?: encoder(img.width, img.height).also { enc = it }
                    e.queueFrame(TimeShiftedImage(img, clockOffsetNs()))
                }
            }
    }

    /** カメラ 1 台。 */
    fun bindSingle(view: PreviewView, back: Boolean, width: Int, height: Int, encoder: (Int, Int) -> VideoEncoder) {
        val p = provider ?: return
        p.unbindAll()
        val preview = Preview.Builder().build().also { it.surfaceProvider = view.surfaceProvider }
        val sel = if (back) CameraSelector.DEFAULT_BACK_CAMERA else CameraSelector.DEFAULT_FRONT_CAMERA
        var offset = 0L
        camera = p.bindToLifecycle(owner, sel, preview, analysis(width, height, encoder) { offset })
        offset = clockOffset(camera!!)
    }

    /** 前後同時（対応する機種だけ）。画面には後ろのカメラを出す。 */
    fun bindDual(view: PreviewView, width: Int, height: Int, back: (Int, Int) -> VideoEncoder, front: (Int, Int) -> VideoEncoder): Boolean {
        val p = provider ?: return false
        val pair = p.availableConcurrentCameraInfos.firstOrNull() ?: return false
        p.unbindAll()
        var offBack = 0L
        var offFront = 0L
        val configs = pair.map { info ->
            val isBack = info.lensFacing == CameraSelector.LENS_FACING_BACK
            val group = UseCaseGroup.Builder()
                .addUseCase(analysis(width, height, if (isBack) back else front) { if (isBack) offBack else offFront })
            if (isBack) group.addUseCase(Preview.Builder().build().also { it.surfaceProvider = view.surfaceProvider })
            SingleCameraConfig(info.cameraSelector, group.build(), owner)
        }
        val cams = p.bindToLifecycle(configs).cameras
        cams.forEach { c ->
            if (c.cameraInfo.lensFacing == CameraSelector.LENS_FACING_BACK) { camera = c; offBack = clockOffset(c) }
            else offFront = clockOffset(c)
        }
        return true
    }

    /** カメラの時刻源が REALTIME なら、MONOTONIC（音声の時計）へ合わせる差。 */
    @OptIn(ExperimentalCamera2Interop::class)
    private fun clockOffset(c: Camera): Long {
        val src = Camera2CameraInfo.from(c.cameraInfo).getCameraCharacteristic(CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE)
        return if (src == CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE_REALTIME) {
            System.nanoTime() - SystemClock.elapsedRealtimeNanos()
        } else 0L
    }

    /** カメラを放す（もう一度 bind できる）。 */
    fun unbind() {
        provider?.unbindAll()
        camera = null
    }

    fun close() {
        provider?.unbindAll()
        camera = null
        exec.shutdown()
    }
}

/** 時刻をずらした ImageProxy（中身はそのまま）。 */
private class TimeShiftedImage(private val img: androidx.camera.core.ImageProxy, private val offsetNs: Long) :
    androidx.camera.core.ImageProxy by img {
    override fun getImageInfo(): androidx.camera.core.ImageInfo = object : androidx.camera.core.ImageInfo by img.imageInfo {
        override fun getTimestamp(): Long = img.imageInfo.timestamp + offsetNs
    }
}
