package dev.awb

import android.app.Service
import android.content.Intent
import android.os.IBinder

/**
 * Serves the compositor's session to window activities, in this app and in
 * the stub apps (all signed with our key: see dev.awb.permission.WINDOWS).
 */
class ToplevelService : Service() {
    override fun onBind(intent: Intent?): IBinder = AwbApp.instance.session
}
