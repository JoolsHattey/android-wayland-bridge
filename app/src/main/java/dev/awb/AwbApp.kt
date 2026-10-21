package dev.awb

import android.app.ActivityOptions
import android.app.Application
import android.content.ComponentName
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.Bitmap
import android.graphics.Rect
import android.hardware.display.DisplayManager
import android.os.Handler
import android.os.Looper
import android.os.RemoteException
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import android.util.Log
import android.view.Display
import android.view.Surface
import dev.awb.window.IToplevelHost
import dev.awb.window.IToplevelSession
import dev.awb.window.ToplevelActivity
import dev.awb.window.Window
import java.io.File
import java.util.concurrent.FutureTask
import kotlin.math.roundToInt

/**
 * Starts the compositor with the process and turns its callbacks into
 * Android windows: one [ToplevelActivity] (own task) per Wayland toplevel,
 * in the stub app of its Linux app when one is installed, else in dev.awb.
 * The activities reach the compositor through [session].
 */
class AwbApp : Application(), NativeCallbacks {
    private val main = Handler(Looper.getMainLooper())

    /** The activity of each attached window. Main thread only, like the rest of the state. */
    private val hosts = HashMap<Int, IToplevelHost>()

    /** Windows the compositor has mapped in this process. */
    private val mapped = HashSet<Int>()

    /** Android display of each attached window. */
    private val windowDisplays = HashMap<Int, Int>()
    private val pendingCursor = HashMap<Int, Cursor>()
    private var cascade = 0

    private class Cursor(val bitmap: Bitmap?, val hotX: Float, val hotY: Float)

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
        createLaunchFifo()
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
        val activity = stubFor(appId) ?: ComponentName(this, ToplevelActivity::class.java)
        val intent = Intent().setComponent(activity)
            .putExtra(Window.EXTRA_WIN, win)
            .putExtra(Window.EXTRA_TITLE, title.ifEmpty { appId })
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_MULTIPLE_TASK or Intent.FLAG_ACTIVITY_NEW_DOCUMENT)
        Log.i(TAG, "map $win ${w}x$h '$title' ($appId) in ${activity.packageName}")
        mapped += win
        startActivity(intent, options.toBundle())
    }.let {}

    /**
     * The installed stub app for Linux app [appId]: an exported
     * [Window.ACTION_TOPLEVEL] activity whose meta-data names it.
     */
    private fun stubFor(appId: String): ComponentName? {
        if (appId.isEmpty()) return null
        val flags = PackageManager.ResolveInfoFlags.of(PackageManager.GET_META_DATA.toLong())
        return packageManager.queryIntentActivities(Intent(Window.ACTION_TOPLEVEL), flags)
            .map { it.activityInfo }
            .firstOrNull { it.metaData?.getString(Window.META_APP_ID) == appId }
            ?.let { ComponentName(it.packageName, it.name) }
    }

    override fun onUnmap(win: Int) = main.post {
        Log.i(TAG, "unmap $win")
        mapped -= win
        windowDisplays -= win
        hosts.remove(win)?.let { h -> remote { h.closeFromClient() } }
    }.let {}

    /** Calls [win]'s activity, if attached, on the main thread. */
    private fun host(win: Int, f: (IToplevelHost) -> Unit) = main.post { hosts[win]?.let { h -> remote { f(h) } } }.let {}

    override fun onTitle(win: Int, title: String) = host(win) { it.setTitle(title) }

    override fun onMinimize(win: Int) = host(win) { it.minimize() }

    override fun onMoveRequest(win: Int) = host(win) { it.moveRequest() }

    override fun onCaptionLight(win: Int, light: Boolean) = host(win) { it.setCaptionLight(light) }

    override fun onExclusion(win: Int, rects: IntArray) = host(win) { it.setExclusion(rects) }

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
        val cursor = if (argb == null || width <= 0 || height <= 0) {
            Cursor(null, 0f, 0f)
        } else {
            var bmp = Bitmap.createBitmap(argb, width, height, Bitmap.Config.ARGB_8888)
            val (w, h) = (width * scale).roundToInt().coerceAtLeast(1) to (height * scale).roundToInt().coerceAtLeast(1)
            if (w != width || h != height) bmp = Bitmap.createScaledBitmap(bmp, w, h, true)
            Cursor(bmp, (hotX * scale).coerceIn(0f, w - 1f), (hotY * scale).coerceIn(0f, h - 1f))
        }
        main.post {
            val h = hosts[win]
            if (h != null) remote { h.setPointer(cursor.bitmap, cursor.hotX, cursor.hotY) } else pendingCursor[win] = cursor
        }
    }

    /** Ignores a dead activity's process; its death notice cleans up. */
    private inline fun remote(f: () -> Unit) {
        try {
            f()
        } catch (e: RemoteException) {
            Log.w(TAG, "window activity gone: $e")
        }
    }

    /** Runs [f] on the main thread and waits for it (binder threads; in-process calls already are on it). */
    private fun <T> onMain(f: () -> T): T {
        if (Looper.myLooper() == main.looper) return f()
        val task = FutureTask(f)
        main.post(task)
        return task.get()
    }

    /**
     * The compositor as the window activities see it (served by
     * [ToplevelService]). Input goes straight to the compositor thread;
     * anything touching window state hops to the main thread.
     */
    val session = object : IToplevelSession.Stub() {
        override fun attach(win: Int, host: IToplevelHost, displayId: Int): Boolean = onMain {
            if (win !in mapped) return@onMain false
            hosts[win] = host
            // Before the surface exists, so the first configure uses this display's scale.
            windowDisplays[win] = displayId
            Native.windowDisplay(win, displayId)
            host.asBinder().linkToDeath({ main.post { died(win, host) } }, 0)
            pendingCursor.remove(win)?.let { c -> remote { host.setPointer(c.bitmap, c.hotX, c.hotY) } }
            true
        }

        override fun attachSurface(win: Int, surface: Surface?) = Native.attachSurface(win, surface)

        override fun resize(win: Int, width: Int, height: Int) = Native.resize(win, width, height)

        override fun windowDisplay(win: Int, displayId: Int) {
            main.post { if (win in hosts) windowDisplays[win] = displayId }
            Native.windowDisplay(win, displayId)
        }

        override fun focus(win: Int, focused: Boolean) {
            if (focused) main.post { windowDisplays[win]?.let { lastDisplay = it } }
            Native.focus(win, focused)
        }

        override fun pointer(win: Int, action: Int, x: Float, y: Float, buttons: Int, vscroll: Float, hscroll: Float, time: Long) =
            Native.pointer(win, action, x, y, buttons, vscroll, hscroll, time)

        override fun key(win: Int, code: Int, down: Boolean, time: Long) = Native.key(win, code, down, time)

        override fun caption(win: Int, top: Int, height: Int, rects: IntArray, width: Int) = main.post {
            val host = hosts[win] ?: return@post
            // How the header is shared between app and system (see docs/DESIGN.md):
            //  auto    - the compositor excludes whatever is drawn in the header;
            //            empty header stays draggable by the system (default)
            //  caption - the whole caption is the app's (clicks only, no drag)
            //  none    - nothing excluded; the system takes every caption click
            val mode = setting("exclusion", "auto")
            Native.caption(win, top, height, rects, mode == "auto")
            if (mode != "auto") {
                remote { host.setExclusion(if (mode == "caption" && height > 0) intArrayOf(0, top, width, height) else IntArray(0)) }
            }
            if (top == 0 && height > 0) {
                val controls = (0 until rects.size / 4).map { i -> rects[i * 4] to rects[i * 4] + rects[i * 4 + 2] }
                val start = controls.filter { it.first == 0 }.maxOfOrNull { it.second } ?: 0
                val end = controls.filter { it.second >= width }.maxOfOrNull { width - it.first } ?: 0
                writeGtkCss(start, end, height, scale120(windowDisplays[win] ?: Display.DEFAULT_DISPLAY))
            }
        }.let {}

        override fun close(win: Int) = main.post {
            if (hosts.remove(win) != null) Native.close(win)
        }.let {}

        override fun launch(appId: String) = launchApp(appId, null)

        override fun open(appId: String, uri: String) = launchApp(appId, uri)
    }

    /** [win]'s activity process died (a crashed or killed stub): the window goes too. */
    private fun died(win: Int, host: IToplevelHost) {
        if (hosts[win]?.asBinder() !== host.asBinder()) return
        Log.w(TAG, "win $win: activity process died")
        hosts.remove(win)
        Native.attachSurface(win, null)
        Native.close(win)
    }

    private val launchFifo get() = File(filesDir, "launch")

    /**
     * The channels to the root side (scripts/awb-launcher.sh), created here so
     * that they carry this app's file label and owner: `launch` takes launch
     * requests from here, `open` takes links from the chroot's apps (written
     * by awb-open, see awb-run.sh) for Android to open.
     */
    private fun createLaunchFifo() {
        for (name in listOf("launch", "open")) {
            val path = File(filesDir, name).path
            try {
                if (!OsConstants.S_ISFIFO(Os.lstat(path).st_mode)) {
                    Os.remove(path)
                    Os.mkfifo(path, "600".toInt(8))
                }
            } catch (_: ErrnoException) {
                runCatching { Os.mkfifo(path, "600".toInt(8)) }.onFailure { Log.e(TAG, "mkfifo $path: $it") }
            }
        }
    }

    /**
     * Hands desktop file id [appId] to the root launcher, which runs it in the
     * chroot, with [uri] as its argument if given (a link from a browser).
     */
    private fun launchApp(appId: String, uri: String?) {
        if (!appId.matches(Regex("[A-Za-z0-9._-]+"))) return
        // One request per line, the link as one word: no whitespace or
        // control characters (a well-formed URI has none).
        if (uri != null && !uri.matches(Regex("[A-Za-z][A-Za-z0-9+.-]*:[\\x21-\\x7e]*"))) {
            Log.w(TAG, "launch $appId: malformed link")
            return
        }
        try {
            // Non-blocking: with no reader (launcher not running) this fails at once.
            val fd = Os.open(launchFifo.path, OsConstants.O_WRONLY or OsConstants.O_NONBLOCK, 0)
            try {
                val line = (if (uri != null) "$appId $uri\n" else "$appId\n").toByteArray()
                Os.write(fd, line, 0, line.size)
            } finally {
                Os.close(fd)
            }
            Log.i(TAG, "launch $appId${if (uri != null) " with a link" else ""}")
        } catch (e: ErrnoException) {
            Log.e(TAG, "launch $appId: root launcher not running ($e)")
            main.post {
                android.widget.Toast.makeText(this, "Linux app launcher is not running (scripts/awb-launcher.sh)", android.widget.Toast.LENGTH_LONG).show()
            }
        }
    }

    companion object {
        const val TAG = "awb"
        lateinit var instance: AwbApp
            private set
    }
}
