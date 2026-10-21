package dev.awb.window;

import android.graphics.Bitmap;

/** A window's activity, as seen by the compositor. */
oneway interface IToplevelHost {
    void setTitle(String title);

    void setCaptionLight(boolean light);

    /** Header areas the app needs (x, y, w, h flattened): system gesture exclusion. */
    void setExclusion(in int[] rects);

    /** A null [bitmap] hides the pointer. Already scaled to display px. */
    void setPointer(in @nullable Bitmap bitmap, float hotX, float hotY);

    void minimize();

    /**
     * The client asked for an interactive move ([edges] 0) or resize
     * (xdg_toplevel resize edges) while a button is held.
     */
    void moveRequest(int edges);

    /**
     * The client started a drag: start Android's from the pressed view,
     * carrying [text] for Android apps, with [shadow] (display px) under the pointer.
     */
    void startDrag(@nullable String text, in @nullable Bitmap shadow);

    /**
     * The app's focused text field (zwp_text_input_v3), or [enabled] false
     * for none: content [purpose] and [hint], surrounding [text] with
     * [cursor] and [anchor] in UTF-16 units, cursor [rect] in view px.
     */
    void setTextInput(boolean enabled, int purpose, int hint, @nullable String text, int cursor, int anchor, in @nullable int[] rect);

    /**
     * [ssd]: the client wants server-side decorations, so Android's caption
     * is opaque and the client sits below it; else its header shows through.
     */
    void setServerDecorations(boolean ssd);

    /** The client destroyed the toplevel. */
    void closeFromClient();
}
