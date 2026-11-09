// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.view.MotionEvent

/**
 * Turns Android touch and pen events into protocol messages.
 *
 * The host applies one MotionEvent's pointers together, so each pointer carries
 * a flag saying whether it is the last of its event.
 */
class InputForwarder(private val send: (ByteArray) -> Unit) {

    fun forward(event: MotionEvent, width: Int, height: Int) {
        if (width <= 0 || height <= 0) return

        val masked = event.actionMasked
        val actionIndex = event.actionIndex
        val count = event.pointerCount
        var sent = 0

        for (i in 0 until count) {
            val tool = when (event.getToolType(i)) {
                MotionEvent.TOOL_TYPE_STYLUS -> Protocol.TOOL_PEN
                MotionEvent.TOOL_TYPE_ERASER -> Protocol.TOOL_ERASER
                // A mouse already drives the host's own pointer.
                MotionEvent.TOOL_TYPE_MOUSE -> continue
                else -> Protocol.TOOL_FINGER
            }
            val action = actionFor(masked, i, actionIndex) ?: continue

            var buttons = 0
            if (event.buttonState and MotionEvent.BUTTON_STYLUS_PRIMARY != 0) {
                buttons = buttons or Protocol.BUTTON_STYLUS_PRIMARY
            }
            if (event.buttonState and MotionEvent.BUTTON_STYLUS_SECONDARY != 0) {
                buttons = buttons or Protocol.BUTTON_STYLUS_SECONDARY
            }

            val last = i == count - 1
            send(
                Protocol.encodePointer(
                    action = action,
                    tool = tool,
                    pointerId = event.getPointerId(i) and 0xff,
                    buttons = buttons,
                    flags = if (last) Protocol.POINTER_FLAG_LAST else 0,
                    x = (event.getX(i) / width).coerceIn(0f, 1f),
                    y = (event.getY(i) / height).coerceIn(0f, 1f),
                    pressure = event.getPressure(i).coerceIn(0f, 1f),
                    tilt = event.getAxisValue(MotionEvent.AXIS_TILT, i),
                    orientation = event.getAxisValue(MotionEvent.AXIS_ORIENTATION, i),
                )
            )
            sent++
        }

        // If the last pointer was skipped (a mouse), nothing carries the flag
        // and the host would hold the batch until the next event.
        if (sent > 0 && !lastWasFlagged(event)) {
            send(flushMarker(event, width, height))
        }
    }

    /** True when the final pointer of the event was forwarded with the flag. */
    private fun lastWasFlagged(event: MotionEvent): Boolean {
        val last = event.pointerCount - 1
        return event.getToolType(last) != MotionEvent.TOOL_TYPE_MOUSE &&
            actionFor(event.actionMasked, last, event.actionIndex) != null
    }

    /** A harmless extra move for the last pointer, carrying only the flag. */
    private fun flushMarker(event: MotionEvent, width: Int, height: Int): ByteArray {
        val last = event.pointerCount - 1
        return Protocol.encodePointer(
            action = Protocol.ACTION_MOVE,
            tool = Protocol.TOOL_FINGER,
            pointerId = event.getPointerId(last) and 0xff,
            buttons = 0,
            flags = Protocol.POINTER_FLAG_LAST,
            x = (event.getX(last) / width).coerceIn(0f, 1f),
            y = (event.getY(last) / height).coerceIn(0f, 1f),
            pressure = 0f,
            tilt = 0f,
            orientation = 0f,
        )
    }

    /** The protocol action for pointer [index] of an event, or null to skip it. */
    private fun actionFor(masked: Int, index: Int, actionIndex: Int): Int? = when (masked) {
        MotionEvent.ACTION_DOWN -> Protocol.ACTION_DOWN
        MotionEvent.ACTION_POINTER_DOWN ->
            if (index == actionIndex) Protocol.ACTION_DOWN else Protocol.ACTION_MOVE
        MotionEvent.ACTION_UP -> Protocol.ACTION_UP
        MotionEvent.ACTION_POINTER_UP ->
            if (index == actionIndex) Protocol.ACTION_UP else Protocol.ACTION_MOVE
        MotionEvent.ACTION_MOVE -> Protocol.ACTION_MOVE
        MotionEvent.ACTION_CANCEL -> Protocol.ACTION_CANCEL
        MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_MOVE -> Protocol.ACTION_HOVER_MOVE
        MotionEvent.ACTION_HOVER_EXIT -> Protocol.ACTION_HOVER_EXIT
        else -> null
    }
}
