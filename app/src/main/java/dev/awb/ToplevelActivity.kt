package dev.awb

import android.app.Activity
import android.app.ActivityManager
import android.content.res.Configuration
import android.graphics.Rect
import android.os.Bundle
import android.util.Log
import android.view.KeyEvent
import android.view.PointerIcon
import android.view.WindowInsets
import android.view.WindowInsetsController

/**
 * The Android window of one Wayland toplevel. The client's surface fills the
 * whole window, including the caption bar, which is made transparent so the
 * app's own header bar shows through with the system controls on top.
 */
class ToplevelActivity : Activity() {
    private var win = -1
    private var closedByClient = false
    private lateinit var view: WaylandView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        win = intent.getIntExtra(EXTRA_WIN, -1)
        val app = AwbApp.instance
        // Tasks outlive the process; one restored after a restart has no client.
        if (win < 0 || win !in app.mapped) {
            win = -1
            finishAndRemoveTask()
            return
        }
        setWindowTitle(intent.getStringExtra(EXTRA_TITLE).orEmpty())
        // Before the surface exists, so the first configure uses this display's scale.
        reportDisplay()
        window.insetsController?.setSystemBarsAppearance(
            WindowInsetsController.APPEARANCE_TRANSPARENT_CAPTION_BAR_BACKGROUND,
            WindowInsetsController.APPEARANCE_TRANSPARENT_CAPTION_BAR_BACKGROUND,
        )
        view = WaylandView(this, win)
        setContentView(view)
        window.decorView.setOnApplyWindowInsetsListener { v, insets ->
            applyCaption(insets)
            v.onApplyWindowInsets(insets)
        }
        app.register(win, this)
    }

    private fun applyCaption(insets: WindowInsets) {
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
        Log.i(AwbApp.TAG, "win $win caption top=$top height=$height controls=$controls")
        val flat = IntArray(controls.size * 4)
        controls.forEachIndexed { i, r ->
            flat[i * 4] = r.left; flat[i * 4 + 1] = r.top; flat[i * 4 + 2] = r.width(); flat[i * 4 + 3] = r.height()
        }
        // How the header is shared between app and system (see docs/DESIGN.md):
        //  auto    - the compositor excludes whatever is drawn in the header;
        //            empty header stays draggable by the system (default)
        //  caption - the whole caption is the app's (clicks only, no drag)
        //  none    - nothing excluded; the system takes every caption click
        val mode = AwbApp.instance.setting("exclusion", "auto")
        Native.caption(win, top, height, flat, mode == "auto")
        captionBand = Rect(0, top, width, top + height)
        if (mode != "auto") {
            view.post {
                view.systemGestureExclusionRects = if (mode == "caption" && height > 0) listOf(captionBand) else emptyList()
            }
        }
        if (top == 0 && height > 0) {
            val start = controls.filter { it.left == 0 }.maxOfOrNull { it.right } ?: 0
            val end = controls.filter { it.right >= width }.maxOfOrNull { width - it.left } ?: 0
            AwbApp.instance.writeGtkCss(start, end, height, AwbApp.instance.scale120(displayId))
        }
    }

    private var captionBand = Rect()

    fun setHeaderExclusion(rects: IntArray) {
        val list = (0 until rects.size / 4).map { i ->
            Rect(rects[i * 4], rects[i * 4 + 1], rects[i * 4] + rects[i * 4 + 2], rects[i * 4 + 1] + rects[i * 4 + 3])
        }
        view.systemGestureExclusionRects = list
    }

    fun setWindowTitle(title: String) {
        this.title = title
        setTaskDescription(ActivityManager.TaskDescription.Builder().setLabel(title).build())
    }

    fun setCaptionLight(light: Boolean) {
        window.insetsController?.setSystemBarsAppearance(
            if (light) WindowInsetsController.APPEARANCE_LIGHT_CAPTION_BARS else 0,
            WindowInsetsController.APPEARANCE_LIGHT_CAPTION_BARS,
        )
    }

    fun setPointer(icon: PointerIcon) {
        view.icon = icon
        view.pointerIcon = icon
    }

    fun onMoveRequest() {
        Log.i(AwbApp.TAG, "win $win: client requested an interactive move")
    }

    fun closeFromClient() {
        closedByClient = true
        finishAndRemoveTask()
    }

    private var displayId = -1

    /** Tells the compositor which display (and so which scale) the window is on. */
    private fun reportDisplay() {
        val id = display.displayId
        if (id != displayId) {
            displayId = id
            Native.windowDisplay(win, id)
        }
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // Moving the window to another display arrives as a configuration change.
        if (win >= 0) reportDisplay()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) AwbApp.instance.lastDisplay = displayId
        Native.focus(win, hasFocus)
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        val code = if (event.scanCode != 0) event.scanCode else Keys.evdevFromKeycode(event.keyCode)
        if (code == 0 || win < 0) return super.dispatchKeyEvent(event)
        when (event.action) {
            // Clients run their own key repeat (wl_keyboard.repeat_info).
            KeyEvent.ACTION_DOWN -> if (event.repeatCount == 0) Native.key(win, code, true, event.eventTime)
            KeyEvent.ACTION_UP -> Native.key(win, code, false, event.eventTime)
        }
        return true
    }

    override fun onDestroy() {
        super.onDestroy()
        if (win < 0) return
        val app = AwbApp.instance
        if (app.activities[win] === this) app.activities.remove(win)
        if (isFinishing && !closedByClient) Native.close(win)
    }

    companion object {
        const val EXTRA_WIN = "win"
        const val EXTRA_TITLE = "title"
    }
}
