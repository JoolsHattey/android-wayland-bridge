package dev.awb.window

import android.app.Activity
import android.content.ComponentName
import android.content.ServiceConnection
import android.os.Bundle
import android.os.IBinder
import android.os.RemoteException
import android.util.Log

/**
 * A stub app's launcher icon: asks the compositor to start the Linux app.
 * Its windows then open as this app's [ToplevelActivity] tasks.
 */
class LaunchActivity : Activity() {
    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val appId = Window.appId(this@LaunchActivity, componentName)
            Log.i(Window.TAG, "launch $appId")
            try {
                if (appId != null) IToplevelSession.Stub.asInterface(binder).launch(appId)
            } catch (_: RemoteException) {
            }
            finish()
        }

        override fun onServiceDisconnected(name: ComponentName) = finish()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (!Window.bindSession(this, connection)) {
            Log.e(Window.TAG, "cannot bind the compositor; is Wayland Bridge installed?")
            finish()
        }
    }

    override fun onDestroy() {
        super.onDestroy()
        runCatching { unbindService(connection) }
    }
}
