package dev.niri.androidmonitor

import android.app.Activity
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.util.Log
import android.view.Gravity
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView
import java.io.BufferedInputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.net.Socket
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong
import kotlin.concurrent.thread

class MainActivity : Activity(), SurfaceHolder.Callback {
    private lateinit var surfaceView: SurfaceView
    private lateinit var statusView: TextView
    private var receiver: StreamReceiver? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setDecorFitsSystemWindows(false)

        surfaceView = SurfaceView(this).also { view ->
            view.holder.addCallback(this)
            view.setOnTouchListener { touched, event ->
                val action = when (event.actionMasked) {
                    MotionEvent.ACTION_DOWN -> TOUCH_DOWN
                    MotionEvent.ACTION_MOVE -> TOUCH_MOVE
                    MotionEvent.ACTION_UP -> TOUCH_UP
                    MotionEvent.ACTION_CANCEL -> TOUCH_CANCEL
                    else -> return@setOnTouchListener true
                }
                val index = event.actionIndex.coerceAtMost(event.pointerCount - 1)
                val x = (event.getX(index) / touched.width.coerceAtLeast(1)).coerceIn(0f, 1f)
                val y = (event.getY(index) / touched.height.coerceAtLeast(1)).coerceIn(0f, 1f)
                receiver?.sendTouch(action, x, y, event.eventTime.toInt())
                true
            }
        }
        statusView = TextView(this).also {
            it.setTextColor(0xffeeeeee.toInt())
            it.setBackgroundColor(0x88000000.toInt())
            it.setPadding(24, 16, 24, 16)
            it.text = "Waiting for surface…"
        }
        setContentView(FrameLayout(this).also { root ->
            root.setBackgroundColor(0xff000000.toInt())
            root.addView(surfaceView, FrameLayout.LayoutParams(-1, -1))
            root.addView(
                statusView,
                FrameLayout.LayoutParams(-2, -2, Gravity.TOP or Gravity.START),
            )
        })
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        startReceiver(holder.surface)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) = Unit

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        stopReceiver()
    }

    override fun onDestroy() {
        stopReceiver()
        super.onDestroy()
    }

    private fun startReceiver(surface: Surface) {
        stopReceiver()
        receiver = StreamReceiver(surface) { message ->
            runOnUiThread {
                statusView.text = message
                statusView.visibility = if (message.startsWith("Streaming")) TextView.GONE else TextView.VISIBLE
            }
        }.also { it.start() }
    }

    private fun stopReceiver() {
        receiver?.close()
        receiver = null
    }

    companion object {
        private const val TOUCH_DOWN = 0
        private const val TOUCH_MOVE = 1
        private const val TOUCH_UP = 2
        private const val TOUCH_CANCEL = 3
    }
}

private class StreamReceiver(
    private val surface: Surface,
    private val status: (String) -> Unit,
) : Thread("niri-stream-receiver") {
    private val running = AtomicBoolean(true)
    private val receiveTimes = ConcurrentHashMap<Long, Long>()
    private val decodedFrames = AtomicLong()
    private val decodeNanoseconds = AtomicLong()
    private val controlQueue = ArrayBlockingQueue<ControlPacket>(64)
    private var socket: Socket? = null
    private var codec: MediaCodec? = null
    @Volatile private var controlOutput: DataOutputStream? = null
    @Volatile private var touchEnabled = false

    override fun run() {
        val controlSender = thread(name = "niri-control-sender") {
            while (running.get()) {
                val packet = try {
                    controlQueue.poll(100, TimeUnit.MILLISECONDS)
                } catch (_: InterruptedException) {
                    break
                } ?: continue
                writeControl(packet)
            }
        }
        try {
            while (running.get()) {
                try {
                    status("Connecting over ADB…")
                    Socket("127.0.0.1", PORT).use { connected ->
                        socket = connected
                        connected.tcpNoDelay = true
                        controlOutput = DataOutputStream(connected.getOutputStream())
                        receive(DataInputStream(BufferedInputStream(connected.getInputStream(), 256 * 1024)))
                    }
                } catch (error: Exception) {
                    if (running.get()) {
                        status("Disconnected: ${error.message}\nRetrying…")
                        sleep(500)
                    }
                } finally {
                    touchEnabled = false
                    controlQueue.clear()
                    controlOutput = null
                    socket = null
                    stopCodec()
                }
            }
        } finally {
            controlSender.interrupt()
            controlSender.join()
        }
    }

    fun close() {
        running.set(false)
        socket?.close()
        interrupt()
        stopCodec()
    }

    fun sendTouch(action: Int, x: Float, y: Float, eventTimeMs: Int) {
        if (!touchEnabled) return
        enqueueControl(
            ControlPacket(CONTROL_TOUCH, action, x.toRawBits(), y.toRawBits(), eventTimeMs),
        )
    }

    private fun sendStats(decodeMs: Float, effectiveFps: Float, decoded: Int) {
        enqueueControl(
            ControlPacket(CONTROL_STATS, 0, decodeMs.toRawBits(), effectiveFps.toRawBits(), decoded),
        )
    }

    private fun enqueueControl(packet: ControlPacket) {
        if (!controlQueue.offer(packet)) {
            // If the USB writer ever falls behind, discard an old move instead
            // of adding input latency. A 64-event queue is normally empty.
            controlQueue.poll()
            controlQueue.offer(packet)
        }
    }

    private fun writeControl(packet: ControlPacket) {
        val output = controlOutput ?: return
        runCatching {
            synchronized(output) {
                output.writeByte(packet.type)
                output.writeByte(packet.action)
                output.writeShort(0)
                output.writeInt(packet.first)
                output.writeInt(packet.second)
                output.writeInt(packet.extra)
                output.flush()
            }
        }.onFailure {
            Log.w(TAG, "control message send failed", it)
            touchEnabled = false
            controlQueue.clear()
            socket?.close()
        }
    }

    private fun receive(input: DataInputStream) {
        val magic = ByteArray(4).also { input.readFully(it) }
        require(magic.contentEquals(byteArrayOf('N'.code.toByte(), 'A'.code.toByte(), 'M'.code.toByte(), 'D'.code.toByte()))) {
            "bad protocol magic"
        }
        val version = input.readUnsignedShort()
        require(version == PROTOCOL_VERSION) { "unsupported protocol version $version" }
        val streamFlags = input.readUnsignedShort()
        touchEnabled = streamFlags and STREAM_FLAG_TOUCH != 0
        val width = input.readInt()
        val height = input.readInt()
        val fps = input.readInt() / 1000f

        val inputBuffers = LinkedBlockingQueue<Int>()
        val selected = createDecoder(width, height, fps, inputBuffers)
        status("Streaming ${width}×${height} @ ${"%.2f".format(fps)}\n$selected")

        runCatching {
            surface.setFrameRate(fps, Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
        }.onFailure { Log.w(TAG, "could not set surface frame rate", it) }

        var receivedFrames = 0L
        var receivedBytes = 0L
        var previousDecoded = decodedFrames.get()
        var previousDecodeNs = decodeNanoseconds.get()
        var statsStarted = SystemClock.elapsedRealtimeNanos()
        var previousSequence: Long? = null

        while (running.get()) {
            val length = input.readInt()
            require(length in 1..MAX_ACCESS_UNIT_SIZE) { "invalid access unit length $length" }
            val ptsUs = input.readLong()
            val sequence = input.readLong()
            input.readInt() // key-frame flag; MediaCodec parses the Annex-B stream itself.
            val data = ByteArray(length).also { input.readFully(it) }
            val receivedAt = SystemClock.elapsedRealtimeNanos()

            previousSequence?.let { previous ->
                if (sequence != previous + 1) {
                    Log.w(TAG, "sequence gap: expected ${previous + 1}, received $sequence")
                }
            }
            previousSequence = sequence
            receiveTimes[ptsUs] = receivedAt

            val index = inputBuffers.take()
            val buffer = codec?.getInputBuffer(index) ?: error("decoder input buffer unavailable")
            require(buffer.capacity() >= length) {
                "decoder input buffer ${buffer.capacity()} is smaller than access unit $length"
            }
            buffer.clear()
            buffer.put(data)
            codec?.queueInputBuffer(index, 0, length, ptsUs, 0)
            receivedFrames++
            receivedBytes += length

            val now = SystemClock.elapsedRealtimeNanos()
            if (now - statsStarted >= 2_000_000_000L) {
                val seconds = (now - statsStarted) / 1_000_000_000.0
                val decodedNow = decodedFrames.get()
                val decodeNsNow = decodeNanoseconds.get()
                val decodedWindow = decodedNow - previousDecoded
                val avgDecodeMs = if (decodedWindow > 0) {
                    (decodeNsNow - previousDecodeNs) / decodedWindow / 1_000_000.0
                } else {
                    0.0
                }
                Log.i(
                    TAG,
                    "stream: %.1f fps, %.2f Mbit/s, receive-to-decode %.2f ms, decoded=%d".format(
                        receivedFrames / seconds,
                        receivedBytes * 8.0 / seconds / 1_000_000.0,
                        avgDecodeMs,
                        decodedWindow,
                    ),
                )
                if (decodedWindow > 0) {
                    sendStats(avgDecodeMs.toFloat(), (receivedFrames / seconds).toFloat(), decodedWindow.toInt())
                }
                statsStarted = now
                receivedFrames = 0
                receivedBytes = 0
                previousDecoded = decodedNow
                previousDecodeNs = decodeNsNow
            }
        }
    }

    private fun createDecoder(
        width: Int,
        height: Int,
        fps: Float,
        inputBuffers: LinkedBlockingQueue<Int>,
    ): String {
        receiveTimes.clear()
        decodedFrames.set(0)
        decodeNanoseconds.set(0)
        val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height).apply {
            setFloat(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, MAX_ACCESS_UNIT_SIZE)
            if (Build.VERSION.SDK_INT >= 30) {
                setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
        }
        val candidates = MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
            .filter { info ->
                !info.isEncoder && info.supportedTypes.any { it.equals(MediaFormat.MIMETYPE_VIDEO_AVC, true) } &&
                    runCatching { info.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC).isFormatSupported(format) }
                        .getOrDefault(false)
            }
            .sortedByDescending { it.isHardwareAccelerated }
        val codecInfo = candidates.firstOrNull()
            ?: error("no H.264 decoder supports ${width}×${height} @ $fps")

        val capabilities = codecInfo.getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
        val lowLatency = capabilities.isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
        if (!lowLatency) {
            format.removeKey(MediaFormat.KEY_LOW_LATENCY)
        }

        val decoder = MediaCodec.createByCodecName(codecInfo.name)
        decoder.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                if (running.get()) inputBuffers.offer(index)
            }

            override fun onOutputBufferAvailable(
                codec: MediaCodec,
                index: Int,
                info: MediaCodec.BufferInfo,
            ) {
                receiveTimes.remove(info.presentationTimeUs)?.let { receivedAt ->
                    decodeNanoseconds.addAndGet(SystemClock.elapsedRealtimeNanos() - receivedAt)
                }
                decodedFrames.incrementAndGet()
                codec.releaseOutputBuffer(index, true)
            }

            override fun onError(codec: MediaCodec, error: MediaCodec.CodecException) {
                status("Decoder error: ${error.diagnosticInfo}")
                socket?.close()
            }

            override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) = Unit
        })
        decoder.configure(format, surface, null, 0)
        decoder.start()
        decoder.setVideoScalingMode(MediaCodec.VIDEO_SCALING_MODE_SCALE_TO_FIT)
        codec = decoder
        return "${codecInfo.name}, hardware=${codecInfo.isHardwareAccelerated}, lowLatency=$lowLatency, touch=$touchEnabled"
    }

    @Synchronized
    private fun stopCodec() {
        receiveTimes.clear()
        val decoder = codec ?: return
        codec = null
        runCatching { decoder.stop() }
        decoder.release()
    }

    companion object {
        private const val TAG = "NiriMonitor"
        private const val PORT = 57421
        private const val PROTOCOL_VERSION = 2
        private const val STREAM_FLAG_TOUCH = 1
        private const val CONTROL_TOUCH = 1
        private const val CONTROL_STATS = 2
        private const val MAX_ACCESS_UNIT_SIZE = 16 * 1024 * 1024
    }
}

private data class ControlPacket(
    val type: Int,
    val action: Int,
    val first: Int,
    val second: Int,
    val extra: Int,
)
