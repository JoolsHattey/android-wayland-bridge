package dev.awb

import android.view.Surface

/** JNI entry points into libawb (native/src/lib.rs). Safe to call from the UI thread. */
object Native {
    init {
        System.loadLibrary("awb")
    }

    @JvmStatic external fun start(socketPath: String, callbacks: NativeCallbacks): Boolean

    /** An Android display appeared or changed. [scale120] is its scale in 120ths (density 1.5 = 180). */
    @JvmStatic external fun display(
        id: Int, width: Int, height: Int, scale120: Int, refreshMhz: Int, widthMm: Int, heightMm: Int, name: String,
    )

    @JvmStatic external fun displayRemoved(id: Int)

    /**
     * Android's appearance, served to Linux apps as the portal's
     * org.freedesktop.appearance settings. [accent] is ARGB; 0 = none.
     */
    @JvmStatic external fun appearance(dark: Boolean, accent: Int)

    /** Window [win] is now on Android display [display]. */
    @JvmStatic external fun windowDisplay(win: Int, display: Int)

    /** Attaches (or with null, detaches) the Android surface of window [win]. */
    @JvmStatic external fun attachSurface(win: Int, surface: Surface?)

    @JvmStatic external fun resize(win: Int, width: Int, height: Int)

    @JvmStatic external fun focus(win: Int, focused: Boolean)

    /** [action]: 0 = move/buttons, 1 = scroll, 2 = pointer left the window. */
    @JvmStatic external fun pointer(win: Int, action: Int, x: Float, y: Float, buttons: Int, vscroll: Float, hscroll: Float, time: Long)

    /** [code] is a Linux evdev key code. */
    @JvmStatic external fun key(win: Int, code: Int, down: Boolean, time: Long)

    @JvmStatic external fun close(win: Int)

    /**
     * Caption geometry in px: [top] rows belong to the system (status bar when
     * maximised), then a band of [height] with the system's own rects (app
     * chip, window controls; x, y, w, h flattened, relative to the band).
     * With [autoExclusion] the compositor reports which header areas the app
     * needs via [NativeCallbacks.onExclusion].
     */
    @JvmStatic external fun caption(win: Int, top: Int, height: Int, rects: IntArray, autoExclusion: Boolean)
}

/** Called on the compositor thread; implementations hop to the main thread. */
interface NativeCallbacks {
    /** [width] x [height] are logical px; multiply by the target display's scale. */
    fun onMap(win: Int, width: Int, height: Int, title: String, appId: String)
    fun onUnmap(win: Int)
    fun onTitle(win: Int, title: String)
    fun onMinimize(win: Int)
    fun onMoveRequest(win: Int)
    fun onCaptionLight(win: Int, light: Boolean)
    fun onExclusion(win: Int, rects: IntArray)
    /** [scale] maps image px to display px. */
    fun onCursor(win: Int, argb: IntArray?, width: Int, height: Int, hotX: Int, hotY: Int, scale: Float)
}
