// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import java.io.DataInputStream
import java.io.EOFException
import java.io.IOException

/**
 * Mirror of `crates/protocol`.
 *
 * Every multi-byte field on the wire is big-endian, which is precisely why
 * [DataInputStream] can be used directly here — no byte-swapping anywhere.
 */
object Protocol {
    const val SOCKET_NAME = "moreland"

    private const val MAGIC = 0x4D524C44 // "MRLD"
    const val VERSION = 1

    const val STREAM_HEADER_LEN = 16
    const val FRAME_HEADER_LEN = 16
    const val ACK_LEN = 8

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

    fun encodeAck(ptsNs: Long): ByteArray = byteArrayOf(
        (ptsNs ushr 56).toByte(),
        (ptsNs ushr 48).toByte(),
        (ptsNs ushr 40).toByte(),
        (ptsNs ushr 32).toByte(),
        (ptsNs ushr 24).toByte(),
        (ptsNs ushr 16).toByte(),
        (ptsNs ushr 8).toByte(),
        ptsNs.toByte(),
    )
}
