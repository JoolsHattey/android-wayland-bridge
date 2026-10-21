package dev.awb.window

import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager

/** Names shared by the compositor (dev.awb) and the window activities. */
object Window {
    const val HOST_PACKAGE = "dev.awb"

    /** The compositor's session service ([IToplevelSession]). */
    const val ACTION_SESSION = "dev.awb.action.TOPLEVEL_SESSION"

    /** Implemented by the [ToplevelActivity] of every stub app. */
    const val ACTION_TOPLEVEL = "dev.awb.action.TOPLEVEL"

    /** Activity meta-data: the desktop file id of a stub's Linux app. */
    const val META_APP_ID = "dev.awb.app_id"

    const val EXTRA_WIN = "win"
    const val EXTRA_TITLE = "title"

    /** The window wants server-side decorations: an opaque caption. */
    const val EXTRA_SSD = "ssd"

    const val TAG = "awb"

    /** Clip label of text a Linux app copied, so windows don't hand it back. */
    const val CLIP_LABEL = "Linux app"

    /** Clip label of a Linux app's drag: its data stays with the compositor. */
    const val DRAG_LABEL = "dev.awb.drag"

    /** A Linux app's drag with no text for Android apps. */
    const val DRAG_MIME = "application/x-dev.awb-drag"

    fun bindSession(context: Context, connection: ServiceConnection): Boolean =
        context.bindService(
            Intent(ACTION_SESSION).setPackage(HOST_PACKAGE),
            connection,
            Context.BIND_AUTO_CREATE or Context.BIND_IMPORTANT,
        )

    /** The Linux app named by [activity]'s meta-data, if any. */
    fun appId(context: Context, activity: android.content.ComponentName): String? =
        context.packageManager.getActivityInfo(activity, PackageManager.ComponentInfoFlags.of(PackageManager.GET_META_DATA.toLong()))
            .metaData?.getString(META_APP_ID)
}
