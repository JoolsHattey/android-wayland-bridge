package dev.awb

import android.app.ActivityOptions
import android.app.Application
import android.content.Intent
import android.content.res.Configuration
import android.graphics.Bitmap
import android.graphics.Rect
import android.hardware.display.DisplayManager
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.Display
import android.view.PointerIcon
import android.view.WindowManager
import java.io.File
import kotlin.math.roundToInt

/**
 * Starts the compositor with the process and turns its callbacks into
 * Android windows: one [ToplevelActivity] (own task) per Wayland toplevel.
 */
class AwbApp : Application(), NativeCallbacks {
    private val main = Handler(Looper.getMainLooper())
    val activities = HashMap<Int, ToplevelActivity>()

    /** Windows the compositor has mapped in this process. */
    val mapped = HashSet<Int>()
    private val pendingCursor = HashMap<Int, PointerIcon?>()
    private var cascade = 0

    lateinit var socketPath: String
        private set

    /** Scale (in 120ths, like wp_fractional_scale) of each display, as reported to the compositor. */
    private val scales = HashMap<Int, Int>()

    /** Display of the last focused window: where new windows open. */
    var lastDisplay: Int? = null

    private val displayListener = object : DisplayManager.DisplayListener {
        override fun onDisplayAdded(id: Int) = reportDisplay(id)
        override fun onDisplayChanged(id: Int) = reportDisplay(id)
        override fun onDisplayRemoved(id: Int) {
            scales.remove(id)
            Native.displayRemoved(id)
        }
    }

    override fun onCreate() {
        super.onCreate()
        instance = this
        socketPath = File(filesDir, "wayland-0").path
        val ok = Native.start(socketPath, this)
        Log.i(TAG, "compositor start=$ok socket=$socketPath")
        val dm = getSystemService(DisplayManager::class.java)
        dm.displays.forEach { reportDisplay(it.displayId) }
        dm.registerDisplayListener(displayListener, main)
        publishAppearance(resources.configuration)
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // Night mode and wallpaper (accent) changes both arrive here.
        publishAppearance(newConfig)
    }

    private fun publishAppearance(config: Configuration) {
        val dark = config.isNightModeActive
        // Material You's accent; libadwaita derives its own shades from it.
        val accent = runCatching { getColor(android.R.color.system_accent1_500) }.getOrDefault(0)
        Native.appearance(dark, accent)
    }

    /**
     * Wayland scale of a display: its Android density (so a GTK logical px is
     * an Android dp, as in Chrome), unless files/awb.properties sets `scale`.
     */
    fun scale120(displayId: Int): Int = scales[displayId] ?: 120

    private fun reportDisplay(id: Int) {
        val display = getSystemService(DisplayManager::class.java).getDisplay(id) ?: return
        val ctx = createDisplayContext(display)
        val metrics = ctx.resources.displayMetrics
        // Window metrics of a plain display context describe the default display; use the mode.
        val bounds = Rect(0, 0, display.mode.physicalWidth, display.mode.physicalHeight)
        val override = setting("scale", "").toFloatOrNull()
        val scale = ((override ?: (metrics.densityDpi / 160f)) * 120).roundToInt().coerceIn(60, 960)
        scales[id] = scale
        val mm = { px: Int, dpi: Float -> if (dpi > 0) (px / dpi * 25.4f).roundToInt() else 0 }
        Native.display(
            id, bounds.width(), bounds.height(), scale, (display.refreshRate * 1000).roundToInt(),
            mm(bounds.width(), metrics.xdpi), mm(bounds.height(), metrics.ydpi), display.name ?: "Display $id",
        )
    }

    /** Settings read from files/awb.properties (written over adb while experimenting). */
    fun setting(name: String, default: String): String =
        runCatching {
            File(filesDir, "awb.properties").readLines()
                .map { it.split('=', limit = 2) }
                .firstOrNull { it.size == 2 && it[0].trim() == name }?.get(1)?.trim()
        }.getOrNull() ?: default

    override fun onMap(win: Int, width: Int, height: Int, title: String, appId: String) = main.post {
        // The compositor sized the client for the same display (its default
        // follows window focus too), so the first frame needs no rescale.
        val displayId = lastDisplay ?: Display.DEFAULT_DISPLAY
        val dm = getSystemService(DisplayManager::class.java)
        val target = dm.getDisplay(displayId) ?: dm.getDisplay(Display.DEFAULT_DISPLAY)
        val display = Rect(0, 0, target.mode.physicalWidth, target.mode.physicalHeight)
        val scale = scale120(target.displayId) / 120f
        val w = (width * scale).roundToInt().coerceIn(200, display.width())
        val h = (height * scale).roundToInt().coerceIn(150, display.height())
        val step = (48 * scale).roundToInt() * (cascade++ % 6)
        val left = ((display.width() - w) / 2 + step).coerceAtMost(display.width() - w)
        val top = ((display.height() - h) / 3 + step).coerceAtMost(display.height() - h)
        val options = ActivityOptions.makeBasic()
            .setLaunchDisplayId(target.displayId)
            .setLaunchBounds(Rect(left, top, left + w, top + h))
        val intent = Intent(this, ToplevelActivity::class.java)
            .putExtra(ToplevelActivity.EXTRA_WIN, win)
            .putExtra(ToplevelActivity.EXTRA_TITLE, title.ifEmpty { appId })
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_MULTIPLE_TASK or Intent.FLAG_ACTIVITY_NEW_DOCUMENT)
        Log.i(TAG, "map $win ${w}x$h '$title' ($appId)")
        mapped += win
        startActivity(intent, options.toBundle())
    }.let {}

    override fun onUnmap(win: Int) = main.post {
        Log.i(TAG, "unmap $win")
        mapped -= win
        activities.remove(win)?.closeFromClient()
    }.let {}

    override fun onTitle(win: Int, title: String) = main.post { activities[win]?.setWindowTitle(title) }.let {}

    override fun onMinimize(win: Int) = main.post { activities[win]?.moveTaskToBack(true) }.let {}

    override fun onMoveRequest(win: Int) = main.post { activities[win]?.onMoveRequest() }.let {}

    override fun onCaptionLight(win: Int, light: Boolean) = main.post { activities[win]?.setCaptionLight(light) }.let {}

    override fun onExclusion(win: Int, rects: IntArray) = main.post { activities[win]?.setHeaderExclusion(rects) }.let {}

    /**
     * Writes files/gtk.css (imported by the chroot's GTK config, see
     * scripts/awb-run.sh) so GTK header bars leave room for the system's
     * caption rects. GTK's own window buttons are kept but made invisible:
     * they sit under Android's controls and size libadwaita's edge padding.
     */
    fun writeGtkCss(start: Int, end: Int, height: Int, scale120: Int) {
        val scale = scale120 / 120f
        val px = { v: Int -> (v / scale).roundToInt() }
        val css = """
            /* Generated by Wayland Bridge from the Android caption insets. */
            windowcontrols.start { min-width: ${px(start)}px; }
            windowcontrols.end { min-width: ${px(end)}px; }
            windowcontrols button, windowcontrols image { opacity: 0; }
            headerbar { min-height: ${px(height)}px; }
        """.trimIndent() + "\n"
        val file = File(filesDir, "gtk.css")
        if (runCatching { file.readText() }.getOrNull() != css) {
            file.writeText(css)
            file.setReadable(true, false)
        }
    }

    override fun onCursor(win: Int, argb: IntArray?, width: Int, height: Int, hotX: Int, hotY: Int, scale: Float) {
        val icon = if (argb == null || width <= 0 || height <= 0) {
            PointerIcon.getSystemIcon(this, PointerIcon.TYPE_NULL)
        } else {
            var bmp = Bitmap.createBitmap(argb, width, height, Bitmap.Config.ARGB_8888)
            val (w, h) = (width * scale).roundToInt().coerceAtLeast(1) to (height * scale).roundToInt().coerceAtLeast(1)
            if (w != width || h != height) bmp = Bitmap.createScaledBitmap(bmp, w, h, true)
            PointerIcon.create(bmp, (hotX * scale).coerceIn(0f, w - 1f), (hotY * scale).coerceIn(0f, h - 1f))
        }
        main.post {
            val a = activities[win]
            if (a != null) a.setPointer(icon) else pendingCursor[win] = icon
        }
    }

    fun register(win: Int, activity: ToplevelActivity) {
        activities[win] = activity
        pendingCursor.remove(win)?.let { activity.setPointer(it) }
    }

    companion object {
        const val TAG = "awb"
        lateinit var instance: AwbApp
            private set
    }
}
