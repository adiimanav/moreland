// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.media.MediaCodec
import android.media.MediaFormat
import android.net.LocalServerSocket
import android.net.LocalSocket
import android.os.Build
import android.util.Log
import android.view.Surface
import java.io.DataInputStream
import java.io.IOException
import java.io.OutputStream
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * Accepts one host connection at a time on an abstract Unix socket, decodes the
 * H.264 stream, and renders to whatever surface is currently attached.
 *
 * The host reaches this socket via `adb forward tcp:27183 localabstract:<name>`,
 * so nothing here touches Android's TCP stack or `netd`. The daemon picks a
 * fresh name per session, and only adbd's own connections are accepted.
 *
 * **Exactly one instance may exist per process.** The socket name is a
 * process-wide resource: a second instance cannot bind it and will spin on
 * `Address already in use` while the first — possibly holding a stale surface —
 * keeps answering connections. So the surface is a *property* of this object,
 * swapped as the view's surface comes and goes, rather than a constructor
 * argument that would tie the stream's lifetime to the surface's.
 */
class VideoStream(
    private val socketName: String,
    private val onStreamShape: (portrait: Boolean) -> Unit,
) {

    private companion object {
        const val TAG = "Moreland"
        const val INPUT_TIMEOUT_MS = 2000L
        const val SURFACE_WAIT_MS = 5000L
        const val MAX_OUTBOX = 512
        const val MAX_STAMPS = 256
    }

    @Volatile private var running = false
    @Volatile private var surface: Surface? = null
    /** The live decoder, so its output surface can be swapped in place. */
    @Volatile private var codec: MediaCodec? = null
    private var serverSocket: LocalServerSocket? = null
    private var client: LocalSocket? = null
    private var thread: Thread? = null
    private var ackThread: Thread? = null

    private val freeInputs = LinkedBlockingQueue<Int>()
    /**
     * Everything bound for the host: acks and input events. One queue and one
     * writer thread, so the two can never interleave mid-message on the socket.
     */
    private val outbox = LinkedBlockingQueue<ByteArray>()

    /** Clock readings for a frame on its way through the decoder. */
    private class Stamp(val receivedNs: Long, val queuedNs: Long) {
        @Volatile var readyNs = 0L
    }

    /** In-flight frames by presentation timestamp (microseconds). */
    private val stamps = ConcurrentHashMap<Long, Stamp>()

    /** When the previous frame reached the screen, for the interval between. */
    @Volatile private var lastRenderedNs = 0L

    /** Frame rate of the current stream, or 0 before one starts. */
    @Volatile private var streamFps = 0

    @Volatile var framesDecoded: Long = 0; private set
    @Volatile var lastError: String? = null; private set

    /**
     * Attach or detach the render target. Safe to call at any time.
     *
     * Surfaces churn far more than you would expect — immersive-sticky mode
     * recreates them whenever the user touches the screen and the system bars
     * flash in and out. Tearing the session down for that meant a single tap on
     * the tablet killed the stream, so instead the decoder's output surface is
     * swapped in place with `setOutputSurface`, and the session survives.
     *
     * Detaching does **not** close the socket. Frames still arrive and are
     * decoded; they are simply discarded instead of rendered until a surface
     * comes back.
     */
    fun setSurface(surface: Surface?) {
        this.surface = surface
        if (surface != null) requestFrameRate(surface, streamFps)
        val codec = this.codec ?: return
        if (surface != null && surface.isValid) {
            runCatching { codec.setOutputSurface(surface) }
                .onFailure { Log.w(TAG, "setOutputSurface failed: ${it.message}") }
        }
    }

    /**
     * Tell the system this surface shows a fixed-rate source, so it can run the
     * panel at a matching refresh. A 60 fps stream on a 90 Hz panel holds
     * frames for an uneven one or two refreshes each, which looks like
     * micro-stutter; at 60 Hz every frame lasts exactly one refresh.
     */
    private fun requestFrameRate(surface: Surface, fps: Int) {
        if (fps <= 0 || !surface.isValid || Build.VERSION.SDK_INT < Build.VERSION_CODES.R) return
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                surface.setFrameRate(
                    fps.toFloat(),
                    Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE,
                    Surface.CHANGE_FRAME_RATE_ALWAYS,
                )
            } else {
                surface.setFrameRate(fps.toFloat(), Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE)
            }
        }.onFailure { Log.w(TAG, "setFrameRate failed: ${it.message}") }
    }

    fun start() {
        if (running) return
        running = true
        thread = Thread(::acceptLoop, "moreland-stream").apply { start() }
    }

    fun stop() {
        running = false
        runCatching { client?.close() }
        runCatching { serverSocket?.close() }
        thread?.interrupt()
        ackThread?.interrupt()
        thread = null
        ackThread = null
    }

    private fun acceptLoop() {
        while (running) {
            try {
                LocalServerSocket(socketName).use { server ->
                    serverSocket = server
                    Log.i(TAG, "listening on localabstract:$socketName")
                    while (running) {
                        val socket = server.accept()
                        val uid = runCatching { socket.peerCredentials.uid }.getOrDefault(-1)
                        if (!Protocol.isTrustedPeer(uid)) {
                            Log.w(TAG, "refusing connection from uid $uid; only adb may connect")
                            runCatching { socket.close() }
                            continue
                        }
                        Log.i(TAG, "host connected")
                        client = socket
                        runCatching { session(socket) }
                            .onFailure {
                                lastError = it.message ?: it.javaClass.simpleName
                                Log.w(TAG, "session ended: $it", it)
                            }
                        runCatching { socket.close() }
                        client = null
                    }
                }
            } catch (e: Exception) {
                if (!running) return
                lastError = e.message
                Log.w(TAG, "accept loop error: ${e.message}")
                runCatching { Thread.sleep(500) }
            } finally {
                serverSocket = null
            }
        }
        Log.i(TAG, "accept loop exited")
    }

    /** Wait briefly for a usable surface; the host may connect before the view is ready. */
    private fun awaitSurface(): Surface {
        val deadline = System.currentTimeMillis() + SURFACE_WAIT_MS
        while (System.currentTimeMillis() < deadline) {
            val current = surface
            if (current != null && current.isValid) return current
            Thread.sleep(50)
        }
        throw IOException(
            "no valid surface after ${SURFACE_WAIT_MS}ms — " +
                "is the screen on and this app in the foreground?"
        )
    }

    private fun session(socket: LocalSocket) {
        val input = DataInputStream(socket.inputStream.buffered(256 * 1024))
        val output = socket.outputStream

        val header = Protocol.readStreamHeader(input)
        Log.i(TAG, "stream ${header.width}x${header.height}@${header.framerate} ${header.mime}")
        onStreamShape(header.height > header.width)

        val target = awaitSurface()
        streamFps = header.framerate
        requestFrameRate(target, streamFps)
        freeInputs.clear()
        outbox.clear()
        stamps.clear()
        lastRenderedNs = 0L

        // The operating-rate hint helps latency but some decoders refuse to
        // start with it at high size and rate combinations (the Exynos one does
        // at 2560x1600@90), so fall back to plain realtime priority.
        val codec = try {
            configureCodec(header, target, operatingRate = true)
        } catch (e: MediaCodec.CodecException) {
            Log.w(TAG, "decoder refused the operating-rate hint ($e); retrying without it")
            configureCodec(header, target, operatingRate = false)
        }
        this.codec = codec
        startAckWriter(output)

        try {
            var scratch = ByteArray(256 * 1024)
            while (running) {
                val frame = Protocol.readFrameHeader(input)
                if (frame.length > scratch.size) {
                    scratch = ByteArray(frame.length)
                }
                Protocol.readFully(input, scratch, frame.length)
                val receivedNs = System.nanoTime()

                val index = freeInputs.poll(INPUT_TIMEOUT_MS, TimeUnit.MILLISECONDS)
                if (index == null) {
                    Log.w(TAG, "decoder starved of input buffers; dropping frame")
                    continue
                }
                val inputBuffer = codec.getInputBuffer(index) ?: continue
                inputBuffer.clear()
                inputBuffer.put(scratch, 0, frame.length)

                val flags = if (frame.keyframe) MediaCodec.BUFFER_FLAG_KEY_FRAME else 0
                // MediaCodec timestamps are microseconds; the wire uses nanoseconds.
                val ptsUs = frame.ptsNs / 1000
                // Stamp before queueing: the decoder may finish before this
                // thread gets another instruction in.
                stamps[ptsUs] = Stamp(receivedNs, System.nanoTime())
                codec.queueInputBuffer(index, 0, frame.length, ptsUs, flags)
                // Frames the decoder swallows (and, rarely, drops) never
                // render, so never leave their stamps to pile up.
                if (stamps.size > MAX_STAMPS) stamps.clear()
            }
        } finally {
            this.codec = null
            runCatching { codec.stop() }
            runCatching { codec.release() }
            ackThread?.interrupt()
            ackThread = null
        }
    }

    private fun configureCodec(
        header: Protocol.StreamHeader,
        target: Surface,
        operatingRate: Boolean,
    ): MediaCodec {
        val format = MediaFormat.createVideoFormat(header.mime, header.width, header.height)

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        // This device exposes OMX.qcom decoders and declares no `low-latency`
        // feature, so KEY_LOW_LATENCY may be ignored. The Qualcomm vendor
        // extension is the fallback; unknown vendor keys are ignored rather
        // than rejected, so setting it costs nothing.
        runCatching { format.setInteger("vendor.qti-ext-dec-low-latency.enable", 1) }
        // Samsung's Exynos decoders (c2.exynos.*) have their own spelling of the
        // same switch, and ignore the Qualcomm one.
        runCatching { format.setInteger("vendor.rtc-ext-dec-low-latency.enable", 1) }
        // Real-time priority, and an operating rate so the codec sizes its
        // pipeline for this frame rate instead of batching for throughput.
        runCatching { format.setInteger(MediaFormat.KEY_PRIORITY, 0) }
        if (operatingRate) {
            runCatching { format.setInteger(MediaFormat.KEY_OPERATING_RATE, header.framerate) }
        }

        val codec = MediaCodec.createDecoderByType(header.mime)
        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(codec: MediaCodec, index: Int) {
                freeInputs.offer(index)
            }

            override fun onOutputBufferAvailable(
                codec: MediaCodec,
                index: Int,
                info: MediaCodec.BufferInfo,
            ) {
                // Render only while a surface is attached; otherwise release
                // without rendering so the buffer is recycled rather than the
                // session failing. Render first, account after; the
                // acknowledgement is queued so this callback never blocks.
                val render = surface?.isValid == true
                stamps[info.presentationTimeUs]?.readyNs = System.nanoTime()
                runCatching { codec.releaseOutputBuffer(index, render) }
                if (render) {
                    framesDecoded++
                    outbox.offer(Protocol.encodeAck(info.presentationTimeUs * 1000))
                }
            }

            override fun onError(codec: MediaCodec, e: MediaCodec.CodecException) {
                lastError = e.message
                Log.e(TAG, "codec error: ${e.message}", e)
            }

            override fun onOutputFormatChanged(codec: MediaCodec, format: MediaFormat) {
                Log.i(TAG, "output format: $format")
            }
        })

        try {
            // Called when a frame actually reaches the screen, with the clock
        // reading from that moment. This is the only honest "displayed" time:
        // releasing the buffer merely schedules it for the next refresh.
        codec.setOnFrameRenderedListener({ _, ptsUs, renderedNs ->
            // `renderedNs` is when the frame reached the screen, on the
            // System.nanoTime() clock the stamps use. The callback itself comes
            // later, batched: about 21 ms after the frame on the Pad 6 at
            // 120 fps, so timing the callback overstates the wait by that much.
            val stamp = stamps.remove(ptsUs) ?: return@setOnFrameRenderedListener
            val previous = lastRenderedNs
            lastRenderedNs = renderedNs
            outbox.offer(
                Protocol.encodeTiming(
                    ptsUs = ptsUs,
                    queueWaitUs = (stamp.queuedNs - stamp.receivedNs) / 1000,
                    decodeUs = (stamp.readyNs - stamp.queuedNs) / 1000,
                    displayWaitUs = ((renderedNs - stamp.readyNs) / 1000).coerceAtLeast(0),
                    renderIntervalUs = if (previous == 0L) 0 else (renderedNs - previous) / 1000,
                )
            )
        }, null)

        codec.configure(format, target, null, 0)
            codec.start()
        } catch (e: Exception) {
            runCatching { codec.release() }
            throw e
        }
        Log.i(TAG, "decoder started: ${codec.name}")
        return codec
    }

    /**
     * Queue a message for the host. Called from the UI thread, so it never
     * blocks; if the link has stalled the oldest input is dropped rather than
     * letting stale touches build up behind it.
     */
    fun send(message: ByteArray) {
        if (client == null) return
        if (outbox.size > MAX_OUTBOX) outbox.poll()
        outbox.offer(message)
    }

    /**
     * Writes acks and input on a dedicated thread so a slow or blocked
     * write can never stall a decoder callback.
     */
    private fun startAckWriter(output: OutputStream) {
        ackThread = Thread({
            try {
                while (running && !Thread.currentThread().isInterrupted) {
                    val message = outbox.poll(500, TimeUnit.MILLISECONDS) ?: continue
                    output.write(message)
                    output.flush()
                }
            } catch (e: Exception) {
                Log.w(TAG, "ack writer stopped: ${e.message}")
            }
        }, "moreland-ack").apply { start() }
    }
}
