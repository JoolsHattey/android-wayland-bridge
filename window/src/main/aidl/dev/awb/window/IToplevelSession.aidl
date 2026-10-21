package dev.awb.window;

import android.view.Surface;
import dev.awb.window.IToplevelHost;

/**
 * The compositor (in dev.awb), as seen by a window's activity, which may live
 * in another package: dev.awb itself, or the stub app of one Linux app.
 * Every call names the Wayland toplevel ([win]) it is about.
 */
interface IToplevelSession {
    /**
     * Binds [host] to toplevel [win] on Android display [displayId]. False if
     * the compositor has no such window (a task restored after a restart).
     */
    boolean attach(int win, IToplevelHost host, int displayId);

    /** Synchronous: a null [surface] returns once the compositor stopped drawing. */
    void attachSurface(int win, in @nullable Surface surface);

    oneway void resize(int win, int width, int height);

    oneway void windowDisplay(int win, int displayId);

    oneway void focus(int win, boolean focused);

    /** As Native.pointer: [action] 0 = move/buttons, 1 = scroll, 2 = left. */
    oneway void pointer(int win, int action, float x, float y, int buttons, float vscroll, float hscroll, long time);

    oneway void key(int win, int code, boolean down, long time);

    /**
     * Caption geometry in px (see Native.caption): [rects] are the system's
     * rects in the band, flattened x, y, w, h; [width] is the window's.
     */
    oneway void caption(int win, int top, int height, in int[] rects, int width);

    /** The user closed the window. */
    oneway void close(int win);

    /** Starts the Linux app with desktop file id [appId] (from a stub's launcher icon). */
    oneway void launch(String appId);

    /**
     * Opens [uri] with the Linux app [appId]: a link in one of the schemes its
     * stub declares (claude://..., say), from a browser.
     */
    oneway void open(String appId, String uri);
}
