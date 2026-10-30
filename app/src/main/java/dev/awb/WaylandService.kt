package dev.awb

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Handler
import android.os.IBinder
import android.os.Looper

/**
 * Keeps the process (and so the compositor) alive while no window is open,
 * and runs the Debian VM ([VmLauncher]): its notification shows the VM's
 * state, with a start or stop button. The actions also work from adb:
 * `am start-foreground-service --user 10 -n dev.awb/.WaylandService -a dev.awb.action.START_VM`.
 */
class WaylandService : Service() {
    private val main = Handler(Looper.getMainLooper())
    private val update = { main.post { notifyState() }.let {} }

    override fun onCreate() {
        super.onCreate()
        getSystemService(NotificationManager::class.java)
            .createNotificationChannel(NotificationChannel(CHANNEL, "Compositor", NotificationManager.IMPORTANCE_MIN))
        VmLauncher.observe(update)
        VmImage.observe(update)
    }

    override fun onDestroy() {
        VmLauncher.unobserve(update)
        VmImage.unobserve(update)
        super.onDestroy()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForeground(1, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        when (intent?.action) {
            ACTION_START_VM -> VmLauncher.startAsync(this)
            ACTION_STOP_VM -> VmLauncher.stop()
        }
        return START_STICKY
    }

    private fun notifyState() = getSystemService(NotificationManager::class.java).notify(1, notification())

    private fun notification(): Notification {
        val image = VmImage.state
        val text = when {
            image is VmImage.State.Working -> "Debian image: ${progress(image)}"
            else -> "Debian VM: ${vmState(this)}"
        }
        val builder = Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.sym_def_app_icon)
            .setContentTitle("Wayland compositor running")
            .setContentText(text)
            .setContentIntent(PendingIntent.getActivity(this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE))
        val (label, action) = if (VmLauncher.state == VmLauncher.State.STOPPED) "Start VM" to ACTION_START_VM else "Stop VM" to ACTION_STOP_VM
        if (VmImage.installed(this)) {
            val pending = PendingIntent.getForegroundService(this, 1, vmIntent(this, action), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
            builder.addAction(Notification.Action.Builder(null, label, pending).build())
        }
        return builder.build()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val CHANNEL = "compositor"
        const val ACTION_START_VM = "dev.awb.action.START_VM"
        const val ACTION_STOP_VM = "dev.awb.action.STOP_VM"

        fun vmIntent(context: Context, action: String): Intent = Intent(context, WaylandService::class.java).setAction(action)

        /** The VM's state in a few words, for the notification and the main page. */
        fun vmState(context: Context): String = when (VmLauncher.state) {
            VmLauncher.State.STOPPED -> when {
                !VmImage.installed(context) -> "no image installed"
                else -> "stopped" + (VmLauncher.lastExit?.let { " ($it)" } ?: "")
            }
            VmLauncher.State.STARTING -> "starting"
            VmLauncher.State.RUNNING -> "booting (waiting for the guest's awb-display)"
            VmLauncher.State.READY -> "running"
        }

        fun progress(s: VmImage.State.Working): String {
            val mb = { b: Long -> b shr 20 }
            return if (s.total > 0) "${s.what} ${mb(s.done)} of ${mb(s.total)} MB (${s.done * 100 / s.total}%)" else s.what
        }
    }
}
