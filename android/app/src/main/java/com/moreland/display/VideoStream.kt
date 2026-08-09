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
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/**
 * Accepts one host connection at a time on an abstract Unix socket, decodes the
 * H.264 stream, and renders to whatever surface is currently attached.
 *
 * The host reaches this socket via `adb forward tcp:27183 localabstract:moreland`,
 * so nothing here touches Android's TCP stack or `netd`.
 *
 * **Exactly one instance may exist per process.** The socket name is a
 * process-wide resource: a second instance cannot bind it and will spin on
 * `Address already in use` while the first — possibly holding a stale surface —
 * keeps answering connections. So the surface is a *property* of this object,
 * swapped as the view's surface comes and goes, rather than a constructor
 * argument that would tie the stream's lifetime to the surface's.
 */
class VideoStream {

    private companion object {
        const val TAG = "Moreland"
        const val INPUT_TIMEOUT_MS = 2000L
        const val SURFACE_WAIT_MS = 5000L
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
    private val pendingAcks = LinkedBlockingQueue<Long>()

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
        val codec = this.codec ?: return
        if (surface != null && surface.isValid) {
            runCatching { codec.setOutputSurface(surface) }
                .onFailure { Log.w(TAG, "setOutputSurface failed: ${it.message}") }
        }
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
                LocalServerSocket(Protocol.SOCKET_NAME).use { server ->
                    serverSocket = server
                    Log.i(TAG, "listening on localabstract:${Protocol.SOCKET_NAME}")
                    while (running) {
                        val socket = server.accept()
                        Log.i(TAG, "host connected")
                        client = socket
                        runCatching { session(socket) }
                            .onFailure {
                                lastError = it.message
                                Log.w(TAG, "session ended: ${it.message}")
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

        val target = awaitSurface()
        freeInputs.clear()
        pendingAcks.clear()

        val codec = configureCodec(header, target)
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
                codec.queueInputBuffer(index, 0, frame.length, frame.ptsNs / 1000, flags)
            }
        } finally {
            this.codec = null
            runCatching { codec.stop() }
            runCatching { codec.release() }
            ackThread?.interrupt()
            ackThread = null
        }
    }

    private fun configureCodec(header: Protocol.StreamHeader, target: Surface): MediaCodec {
        val format = MediaFormat.createVideoFormat(header.mime, header.width, header.height)

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
        }
        // This device exposes OMX.qcom decoders and declares no `low-latency`
        // feature, so KEY_LOW_LATENCY may be ignored. The Qualcomm vendor
        // extension is the fallback; unknown vendor keys are ignored rather
        // than rejected, so setting it costs nothing.
        runCatching { format.setInteger("vendor.qti-ext-dec-low-latency.enable", 1) }

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
                runCatching { codec.releaseOutputBuffer(index, render) }
                if (render) {
                    framesDecoded++
                    pendingAcks.offer(info.presentationTimeUs * 1000)
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

        codec.configure(format, target, null, 0)
        codec.start()
        Log.i(TAG, "decoder started: ${codec.name}")
        return codec
    }

    /**
     * Acknowledges rendered frames on a dedicated thread so a slow or blocked
     * write can never stall a decoder callback.
     */
    private fun startAckWriter(output: OutputStream) {
        ackThread = Thread({
            try {
                while (running && !Thread.currentThread().isInterrupted) {
                    val pts = pendingAcks.poll(500, TimeUnit.MILLISECONDS) ?: continue
                    output.write(Protocol.encodeAck(pts))
                    output.flush()
                }
            } catch (e: Exception) {
                Log.w(TAG, "ack writer stopped: ${e.message}")
            }
        }, "moreland-ack").apply { start() }
    }
}
