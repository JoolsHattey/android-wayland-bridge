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

    void moveRequest();

    /** The client destroyed the toplevel. */
    void closeFromClient();
}
