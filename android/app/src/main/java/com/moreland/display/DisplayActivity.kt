// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.app.Activity
import android.content.pm.ActivityInfo
import android.graphics.Color
import android.os.Bundle
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.FrameLayout
import android.widget.TextView

/**
 * Fullscreen host for the decoded stream.
 *
 * The stream is tied to the surface lifecycle rather than the activity's:
 * a surface can be destroyed and recreated without the activity restarting,
 * and decoding into a dead surface is a crash.
 */
class DisplayActivity : Activity(), SurfaceHolder.Callback {

    private lateinit var surfaceView: SurfaceView
    private lateinit var status: TextView
    private var stream: VideoStream? = null
    private var input: InputForwarder? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setBackgroundDrawableResource(android.R.color.black)

        val root = FrameLayout(this).apply { setBackgroundColor(Color.BLACK) }

        status = TextView(this).apply {
            text = "Waiting for host…\n\nConnect the tablet and start the daemon."
            setTextColor(Color.parseColor("#888888"))
            textSize = 16f
            val pad = (24 * resources.displayMetrics.density).toInt()
            setPadding(pad, pad, pad, pad)
        }
        surfaceView = SurfaceView(this)

        root.addView(surfaceView, FrameLayout.LayoutParams(MATCH, MATCH))
        root.addView(status, FrameLayout.LayoutParams(MATCH, WRAP))
        setContentView(root)

        stream = VideoStream(::followStreamShape).also { s ->
            s.start()
            input = InputForwarder { s.send(it) }
        }
        surfaceView.holder.addCallback(this)
        goImmersive()
    }

    /**
     * Turn the window to match the stream, or the surface stretches it to fit.
     *
     * Hyprland 0.57 hands a rotated output to capture already upright, so
     * `--transform 1` arrives as a 1200x1920 stream. Older versions send the
     * landscape frame, which never reaches the portrait branch. The sensor
     * decides which way up, so portrait holds whichever way the tablet is
     * stood, even with auto-rotate locked.
     */
    private fun followStreamShape(portrait: Boolean) = runOnUiThread {
        requestedOrientation = if (portrait) {
            ActivityInfo.SCREEN_ORIENTATION_SENSOR_PORTRAIT
        } else {
            ActivityInfo.SCREEN_ORIENTATION_USER_LANDSCAPE
        }
    }

    // Touch and pen go to the host whatever view they land on, so the status
    // text or a flashing system bar can never swallow them.
    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        // By default Android batches touch samples to the display's refresh, so
        // an event can wait up to a frame before this code even sees it. Ask
        // for each sample as it arrives; this lasts until the finger lifts.
        if (ev.actionMasked == MotionEvent.ACTION_DOWN) {
            surfaceView.requestUnbufferedDispatch(ev)
        }
        input?.forward(ev, surfaceView.width, surfaceView.height)
        return super.dispatchTouchEvent(ev)
    }

    // A pen hovering above the glass arrives as a generic motion event.
    override fun dispatchGenericMotionEvent(ev: MotionEvent): Boolean {
        input?.forward(ev, surfaceView.width, surfaceView.height)
        return super.dispatchGenericMotionEvent(ev)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) goImmersive()
    }

    @Suppress("DEPRECATION")
    private fun goImmersive() {
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                or View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_FULLSCREEN
                or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
            )
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        // The stream outlives individual surfaces. Only the render target is
        // swapped here — creating a stream per surface races two instances for
        // the single process-wide socket name.
        stream?.setSurface(holder.surface)
        status.postDelayed(::refreshStatus, 1000)
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        stream?.setSurface(holder.surface)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        stream?.setSurface(null)
    }

    override fun onDestroy() {
        stream?.stop()
        stream = null
        super.onDestroy()
    }

    private fun refreshStatus() {
        val stream = this.stream ?: return
        if (stream.framesDecoded > 0) {
            status.visibility = View.GONE
        } else {
            stream.lastError?.let { status.text = "Waiting for host…\n\nLast error: $it" }
            status.postDelayed(::refreshStatus, 1000)
        }
    }

    private companion object {
        const val MATCH = FrameLayout.LayoutParams.MATCH_PARENT
        const val WRAP = FrameLayout.LayoutParams.WRAP_CONTENT
    }
}
