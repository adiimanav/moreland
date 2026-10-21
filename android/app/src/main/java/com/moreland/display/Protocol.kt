// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import java.io.DataInputStream
import java.io.EOFException
import java.io.IOException
import java.nio.ByteBuffer

/**
 * Mirror of `crates/protocol`.
 *
 * Every multi-byte field on the wire is big-endian, which is precisely why
 * [DataInputStream] can be used directly here — no byte-swapping anywhere.
 */
object Protocol {
    const val SOCKET_NAME = "moreland"

    private const val MAGIC = 0x4D524C44 // "MRLD"
    const val VERSION = 2

    const val STREAM_HEADER_LEN = 16
    const val FRAME_HEADER_LEN = 16
    const val ACK_LEN = 8
    const val POINTER_LEN = 28

    /** Device -> host message types. */
    const val MSG_ACK = 0x01
    const val MSG_POINTER = 0x02

    const val ACTION_DOWN = 0
    const val ACTION_UP = 1
    const val ACTION_MOVE = 2
    const val ACTION_CANCEL = 3
    const val ACTION_HOVER_MOVE = 4
    const val ACTION_HOVER_EXIT = 5

    const val TOOL_FINGER = 0
    const val TOOL_PEN = 1
    const val TOOL_ERASER = 2

    const val BUTTON_STYLUS_PRIMARY = 1
    const val BUTTON_STYLUS_SECONDARY = 2

    /** Set on the last pointer of one MotionEvent. */
    const val POINTER_FLAG_LAST = 1

    const val FLAG_KEYFRAME = 0x01

    /** Refuse absurd frame lengths rather than attempting the allocation. */
    const val MAX_FRAME_BYTES = 8 * 1024 * 1024

    data class StreamHeader(
        val width: Int,
        val height: Int,
        val framerate: Int,
        val mime: String,
    )

    data class FrameHeader(
        val length: Int,
        val ptsNs: Long,
        val keyframe: Boolean,
    )

    @Throws(IOException::class)
    fun readStreamHeader(input: DataInputStream): StreamHeader {
        val magic = input.readInt()
        if (magic != MAGIC) {
            throw IOException("bad magic 0x%08x".format(magic))
        }
        val version = input.readUnsignedShort()
        if (version != VERSION) {
            throw IOException("unsupported protocol version $version (expected $VERSION)")
        }
        val width = input.readUnsignedShort()
        val height = input.readUnsignedShort()
        val framerate = input.readUnsignedShort()
        val codec = input.readUnsignedByte()
        input.skipBytes(3) // reserved

        val mime = when (codec) {
            0 -> "video/avc"
            1 -> "video/hevc"
            else -> throw IOException("unknown codec id $codec")
        }
        return StreamHeader(width, height, framerate, mime)
    }

    @Throws(IOException::class)
    fun readFrameHeader(input: DataInputStream): FrameHeader {
        val length = input.readInt()
        if (length < 0 || length > MAX_FRAME_BYTES) {
            throw IOException("implausible frame length $length")
        }
        val ptsNs = input.readLong()
        val flags = input.readUnsignedByte()
        input.skipBytes(3) // reserved
        return FrameHeader(length, ptsNs, flags and FLAG_KEYFRAME != 0)
    }

    @Throws(IOException::class)
    fun readFully(input: DataInputStream, buffer: ByteArray, length: Int) {
        var read = 0
        while (read < length) {
            val n = input.read(buffer, read, length - read)
            if (n < 0) throw EOFException("stream closed after $read of $length bytes")
            read += n
        }
    }

    fun encodeAck(ptsNs: Long): ByteArray {
        val buf = ByteBuffer.allocate(1 + ACK_LEN) // big-endian by default
        buf.put(MSG_ACK.toByte())
        buf.putLong(ptsNs)
        return buf.array()
    }

    /**
     * One pointer of one touch or pen event. Positions are fractions of the
     * streamed surface, so the host needs no idea of this screen's pixel size.
     */
    fun encodePointer(
        action: Int,
        tool: Int,
        pointerId: Int,
        buttons: Int,
        flags: Int,
        x: Float,
        y: Float,
        pressure: Float,
        tilt: Float,
        orientation: Float,
    ): ByteArray {
        val buf = ByteBuffer.allocate(1 + POINTER_LEN)
        buf.put(MSG_POINTER.toByte())
        buf.put(action.toByte())
        buf.put(tool.toByte())
        buf.put(pointerId.toByte())
        buf.put(buttons.toByte())
        buf.put(flags.toByte())
        buf.put(0).put(0).put(0) // reserved
        buf.putFloat(x)
        buf.putFloat(y)
        buf.putFloat(pressure)
        buf.putFloat(tilt)
        buf.putFloat(orientation)
        return buf.array()
    }
}
