package dev.awb.window;

import android.os.Bundle;
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
     * [maximized]: the window spans the display.
     */
    oneway void caption(int win, int top, int height, in int[] rects, int width, boolean maximized);

    /** Android's clipboard text, read by the focused window (only it may read it). */
    oneway void clipboard(String text);

    /**
     * An Android drag event on [win]'s view (DragEvent action, view px).
     * [local]: the drag is a Linux app's own; [result]: an ENDED drag was
     * dropped somewhere; [text]: a dropped foreign clip's text.
     */
    oneway void drag(int win, int action, float x, float y, boolean local, boolean result, in String[] mimes, @nullable String text);

    /**
     * An IME edit for [win]'s text field: [kind] 0 = commit [text], 1 =
     * composing [text] with the cursor at [a], 2 = delete [a] units before
     * the cursor and [b] after, 3 = the [a] units before the cursor become
     * composing [text] again.
     */
    oneway void ime(int win, int kind, @nullable String text, int a, int b);

    /** Whether tasks can be moved (the root helper is running, see awb-launcher.sh). */
    boolean canMoveTasks();

    /** Moves or resizes task [taskId] to these display px (through the root helper). */
    oneway void moveTask(int taskId, int left, int top, int right, int bottom);

    /** The user closed the window. */
    oneway void close(int win);

    /** Starts the Linux app with desktop file id [appId] (from a stub's launcher icon). */
    oneway void launch(String appId);

    /**
     * Opens [uri] with the Linux app [appId]: a link in one of the schemes its
     * stub declares (claude://..., say), from a browser.
     */
    oneway void open(String appId, String uri);

    // Added later, and kept last, so that stubs built before keep their
    // transaction codes.

    /**
     * Android waits for [win]'s frame at [width] x [height] px. Blocks until
     * the compositor has it: a frame as [IToplevelHost.present] takes, to
     * show with the window's synced frame; null to go on without (not
     * direct mode, or the client was too slow). Needs [ownsLayer].
     */
    @nullable Bundle redraw(int win, int width, int height);

    /**
     * The frame with token [token] ([IToplevelHost.present], [redraw]) is
     * on screen, so the compositor can release what it replaced.
     */
    oneway void presented(int win, long token);

    /**
     * [win]'s activity owns its direct layer: direct-mode frames come
     * through [IToplevelHost.present] and [redraw]. Called right after
     * [attach], before the surface exists.
     */
    void ownsLayer(int win);

    /**
     * Pointer samples of one MotionEvent, oldest first (its history, then
     * the event itself): x, y, dx, dy per sample in [samples] (view px; NaN
     * x and y: no position, as under pointer capture; NaN dx and dy: no
     * relative motion), their event times in ns in [times]. [buttons]
     * (MotionEvent button state) applies from the last sample on.
     */
    oneway void motion(int win, int buttons, in float[] samples, in long[] times);

    /**
     * A scroll at [x], [y] (view px; NaN: where the pointer is), in Wayland's
     * direction (positive: down, right). [source] 0 = wheel (notches,
     * fractional for high-resolution wheels), 1 = touchpad fingers (px;
     * [stop] when they lift), 2 = continuous (notches). [time] in ns.
     */
    oneway void scroll(int win, float x, float y, float vscroll, float hscroll, int source, boolean stop, long time);

    /** Android's pointer capture of [win]'s view began or ended ([IToplevelHost.setPointerCapture]). */
    oneway void pointerCapture(int win, boolean captured);
}
