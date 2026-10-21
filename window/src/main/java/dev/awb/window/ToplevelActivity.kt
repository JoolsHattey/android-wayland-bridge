package dev.awb.window

import android.app.Activity
import android.app.ActivityManager
import android.content.ComponentName
import android.content.ServiceConnection
import android.content.res.Configuration
import android.graphics.Bitmap
import android.graphics.Rect
import android.os.Bundle
import android.os.IBinder
import android.os.RemoteException
import android.util.Log
import android.view.KeyEvent
import android.view.PointerIcon
import android.view.WindowInsets
import android.view.WindowInsetsController

/**
 * The Android window of one Wayland toplevel. The client's surface fills the
 * whole window, including the caption bar, which is made transparent so the
 * app's own header bar shows through with the system controls on top.
 *
 * Runs in dev.awb, or in the stub app of the window's Linux app so that the
 * window belongs to that app (taskbar, recents). Either way it reaches the
 * compositor through [IToplevelSession].
 */
class ToplevelActivity : Activity() {
    private var win = -1
    private var closedByClient = false
    private var session: IToplevelSession? = null
    private var view: WaylandView? = null

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val s = IToplevelSession.Stub.asInterface(binder)
            // Tasks outlive the compositor; one restored after a restart has no client.
            val attached = try {
                s.attach(win, host, display.displayId)
            } catch (_: RemoteException) {
                false
            }
            if (!attached) {
                closedByClient = true
                finishAndRemoveTask()
                return
            }
            session = s
            displayId = display.displayId
            val v = WaylandView(this@ToplevelActivity, s, win)
            view = v
            setContentView(v)
            v.requestFocus()
            // Insets that arrived before the session went unreported.
            window.decorView.requestApplyInsets()
        }

        /** The compositor died, and with it the client's window. */
        override fun onServiceDisconnected(name: ComponentName) {
            session = null
            closedByClient = true
            finishAndRemoveTask()
        }
    }

    /** Compositor requests, which arrive on a binder thread (or this one, in dev.awb). */
    private val host = object : IToplevelHost.Stub() {
        override fun setTitle(title: String) = runOnUiThread { setWindowTitle(title) }
        override fun setCaptionLight(light: Boolean) = runOnUiThread { applyCaptionLight(light) }
        override fun setExclusion(rects: IntArray) = runOnUiThread { applyExclusion(rects) }
        override fun setPointer(bitmap: Bitmap?, hotX: Float, hotY: Float) {
            val icon = if (bitmap == null) {
                PointerIcon.getSystemIcon(this@ToplevelActivity, PointerIcon.TYPE_NULL)
            } else {
                PointerIcon.create(bitmap, hotX, hotY)
            }
            runOnUiThread { view?.let { it.icon = icon; it.pointerIcon = icon } }
        }
        override fun minimize() = runOnUiThread { moveTaskToBack(true) }
        override fun moveRequest() = Log.i(Window.TAG, "win $win: client requested an interactive move").let {}
        override fun closeFromClient() = runOnUiThread {
            closedByClient = true
            finishAndRemoveTask()
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        win = intent.getIntExtra(Window.EXTRA_WIN, -1)
        if (win < 0) {
            finishAndRemoveTask()
            return
        }
        setWindowTitle(intent.getStringExtra(Window.EXTRA_TITLE).orEmpty())
        window.insetsController?.setSystemBarsAppearance(
            WindowInsetsController.APPEARANCE_TRANSPARENT_CAPTION_BAR_BACKGROUND,
            WindowInsetsController.APPEARANCE_TRANSPARENT_CAPTION_BAR_BACKGROUND,
        )
        window.decorView.setOnApplyWindowInsetsListener { v, insets ->
            applyCaption(insets)
            v.onApplyWindowInsets(insets)
        }
        if (!Window.bindSession(this, connection)) {
            Log.e(Window.TAG, "win $win: cannot bind the compositor")
            finishAndRemoveTask()
        }
    }

    private fun applyCaption(insets: WindowInsets) {
        val s = session ?: return
        val captionTop = insets.getInsets(WindowInsets.Type.captionBar()).top
        val all = insets.getBoundingRects(WindowInsets.Type.captionBar())
        val width = window.decorView.width
        // A maximised window's caption starts below the status bar, which shows
        // up as a full-width rect at the top.
        val top = maxOf(
            insets.getInsets(WindowInsets.Type.statusBars()).top,
            all.filter { it.top == 0 && it.left == 0 && it.width() >= width && it.height() < captionTop }
                .maxOfOrNull { it.bottom } ?: 0,
        )
        val height = (captionTop - top).coerceAtLeast(0)
        val controls = all.filter { it.bottom > top && it.width() < width }
            .map { Rect(it.left, maxOf(0, it.top - top), it.right, it.bottom - top) }
        Log.i(Window.TAG, "win $win caption top=$top height=$height controls=$controls")
        val flat = IntArray(controls.size * 4)
        controls.forEachIndexed { i, r ->
            flat[i * 4] = r.left; flat[i * 4 + 1] = r.top; flat[i * 4 + 2] = r.width(); flat[i * 4 + 3] = r.height()
        }
        try {
            s.caption(win, top, height, flat, width)
        } catch (_: RemoteException) {
        }
    }

    private fun applyExclusion(rects: IntArray) {
        val list = (0 until rects.size / 4).map { i ->
            Rect(rects[i * 4], rects[i * 4 + 1], rects[i * 4] + rects[i * 4 + 2], rects[i * 4 + 1] + rects[i * 4 + 3])
        }
        view?.systemGestureExclusionRects = list
    }

    private fun setWindowTitle(title: String) {
        this.title = title
        setTaskDescription(ActivityManager.TaskDescription.Builder().setLabel(title).build())
    }

    private fun applyCaptionLight(light: Boolean) {
        window.insetsController?.setSystemBarsAppearance(
            if (light) WindowInsetsController.APPEARANCE_LIGHT_CAPTION_BARS else 0,
            WindowInsetsController.APPEARANCE_LIGHT_CAPTION_BARS,
        )
    }

    private var displayId = -1

    /** Tells the compositor which display (and so which scale) the window is on. */
    private fun reportDisplay() {
        val s = session ?: return
        val id = display.displayId
        if (id != displayId) {
            displayId = id
            try {
                s.windowDisplay(win, id)
            } catch (_: RemoteException) {
            }
        }
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // Moving the window to another display arrives as a configuration change.
        reportDisplay()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        try {
            session?.focus(win, hasFocus)
        } catch (_: RemoteException) {
        }
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        val s = session
        val code = if (event.scanCode != 0) event.scanCode else Keys.evdevFromKeycode(event.keyCode)
        if (code == 0 || s == null) return super.dispatchKeyEvent(event)
        try {
            when (event.action) {
                // Clients run their own key repeat (wl_keyboard.repeat_info).
                KeyEvent.ACTION_DOWN -> if (event.repeatCount == 0) s.key(win, code, true, event.eventTime)
                KeyEvent.ACTION_UP -> s.key(win, code, false, event.eventTime)
            }
        } catch (_: RemoteException) {
        }
        return true
    }

    override fun onDestroy() {
        super.onDestroy()
        if (win < 0) return
        val s = session
        if (s != null && isFinishing && !closedByClient) {
            try {
                s.close(win)
            } catch (_: RemoteException) {
            }
        }
        runCatching { unbindService(connection) }
    }
}
