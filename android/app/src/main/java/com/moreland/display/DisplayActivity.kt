// SPDX-License-Identifier: Apache-2.0

package com.moreland.display

import android.app.Activity
import android.graphics.Color
import android.os.Bundle
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

        stream = VideoStream().also { it.start() }
        surfaceView.holder.addCallback(this)
        goImmersive()
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
