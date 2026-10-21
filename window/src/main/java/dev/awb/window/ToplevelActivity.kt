package dev.awb.window

import android.app.Activity
import android.app.ActivityManager
import android.content.ClipboardManager
import android.content.ComponentName
import android.content.ServiceConnection
import android.content.res.Configuration
import android.graphics.Bitmap
import android.graphics.Rect
import android.os.Bundle
import android.os.IBinder
import android.os.RemoteException
import android.util.Log
import android.view.DragEvent
import android.view.KeyEvent
import android.view.PointerIcon
import android.view.WindowInsets
import android.view.WindowInsetsController
import android.view.WindowManager

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
            // Before the surface exists: direct frames come to our own layer.
            try {
                s.ownsLayer(win)
            } catch (_: RemoteException) {
            }
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
        override fun moveRequest(edges: Int) = runOnUiThread { beginMove(edges) }
        override fun setServerDecorations(ssd: Boolean) = runOnUiThread { applyDecorations(ssd) }
        override fun setTextInput(enabled: Boolean, purpose: Int, hint: Int, text: String?, cursor: Int, anchor: Int, rect: IntArray?) =
            runOnUiThread { view?.setTextField(if (enabled) TextField(purpose, hint, text, cursor, anchor, rect) else null) }
        override fun startDrag(text: String?, shadow: Bitmap?) = runOnUiThread {
            val v = view
            if (v == null || !v.startDrag(text, shadow)) {
                try {
                    session?.drag(win, DragEvent.ACTION_DRAG_ENDED, 0f, 0f, true, false, emptyArray(), null)
                } catch (_: RemoteException) {
                }
            }
        }
        override fun closeFromClient() = runOnUiThread {
            closedByClient = true
            finishAndRemoveTask()
        }
        override fun setPointerCapture(capture: Boolean) = runOnUiThread { view?.setCapture(capture) }
        // Not on the UI thread: a frame must not wait behind it.
        override fun present(frame: Bundle) {
            val v = view
            if (v != null) {
                v.present(frame)
            } else {
                try {
                    session?.presented(win, frame.getLong("k"))
                } catch (_: RemoteException) {
                }
            }
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
        // The app places its own text fields: the window must not pan or
        // resize for the IME (a relayout also stalls direct-mode frames).
        window.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING)
        applyDecorations(intent.getBooleanExtra(Window.EXTRA_SSD, false))
        window.decorView.setOnApplyWindowInsetsListener { v, insets ->
            applyCaption(insets)
            v.onApplyWindowInsets(insets)
        }
        clipboard.addPrimaryClipChangedListener(clipListener)
        if (!Window.bindSession(this, connection)) {
            Log.e(Window.TAG, "win $win: cannot bind the compositor")
            finishAndRemoveTask()
        }
    }

    /** Server-side decorations: Android's own (opaque) caption above the client. */
    private var ssd = false

    /**
     * Client-side decorations show the app's header through a transparent
     * caption; server-side ones keep the caption opaque, and the client is
     * placed below it.
     */
    private fun applyDecorations(serverSide: Boolean) {
        ssd = serverSide
        window.insetsController?.setSystemBarsAppearance(
            if (serverSide) 0 else WindowInsetsController.APPEARANCE_TRANSPARENT_CAPTION_BAR_BACKGROUND,
            WindowInsetsController.APPEARANCE_TRANSPARENT_CAPTION_BAR_BACKGROUND,
        )
        window.decorView.requestApplyInsets()
    }

    private fun applyCaption(insets: WindowInsets) {
        val s = session ?: return
        val captionTop = insets.getInsets(WindowInsets.Type.captionBar()).top
        val all = insets.getBoundingRects(WindowInsets.Type.captionBar())
        // The window's new size: the view may not have been laid out at it yet.
        val bounds = windowManager.currentWindowMetrics.bounds
        val width = bounds.width()
        // With server-side decorations Android lays the view out below its
        // opaque caption, so the client has the whole surface.
        if (ssd) {
            try {
                s.caption(win, 0, 0, IntArray(0), width, false)
            } catch (_: RemoteException) {
            }
            return
        }
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
        // No public API says whether a desktop window is maximised; one
        // spanning the display's width is.
        val maximized = bounds.width() >= windowManager.maximumWindowMetrics.bounds.width()
        try {
            s.caption(win, top, height, flat, width, maximized)
        } catch (_: RemoteException) {
        }
    }

    /**
     * An interactive move or resize the app asked for: apps can't move their
     * own tasks, so the root helper does, following the pressed pointer.
     */
    private fun beginMove(edges: Int) {
        val s = session ?: return
        val v = view ?: return
        val can = try {
            s.canMoveTasks()
        } catch (_: RemoteException) {
            false
        }
        if (!can) {
            Log.i(Window.TAG, "win $win: move/resize requested, but the root helper is not running")
            return
        }
        val task = taskId
        v.beginMove(edges, windowManager.currentWindowMetrics.bounds) { r ->
            try {
                s.moveTask(task, r.left, r.top, r.right, r.bottom)
            } catch (_: RemoteException) {
            }
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
        // Copies made in Android apps meanwhile.
        if (hasFocus) shareClipboard()
    }

    private val clipboard by lazy { getSystemService(ClipboardManager::class.java) }

    /** Only the focused app may read the clipboard (or hear of changes). */
    private val clipListener = ClipboardManager.OnPrimaryClipChangedListener { if (hasWindowFocus()) shareClipboard() }

    /**
     * Hands Android's clipboard text to the compositor, once per clip: each
     * read of another app's clip shows Android's "pasted" notice. Text a
     * Linux app copied stays with its source.
     */
    private fun shareClipboard() {
        val s = session ?: return
        val desc = clipboard.primaryClipDescription ?: return
        if (desc.label == Window.CLIP_LABEL || desc.timestamp == lastClip || !desc.hasMimeType("text/*")) return
        lastClip = desc.timestamp
        val text = clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(this)?.toString() ?: return
        try {
            s.clipboard(text)
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
        clipboard.removePrimaryClipChangedListener(clipListener)
        val s = session
        if (s != null && isFinishing && !closedByClient) {
            try {
                s.close(win)
            } catch (_: RemoteException) {
            }
        }
        runCatching { unbindService(connection) }
    }

    private companion object {
        /** Timestamp of the clip last handed over by this process's windows. */
        var lastClip = 0L
    }
}
