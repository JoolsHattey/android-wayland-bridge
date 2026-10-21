package dev.awb

import android.app.Activity
import android.os.Bundle
import android.widget.TextView

/** Status page; opening it starts the compositor and keeps it alive. */
class MainActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        startForegroundService(android.content.Intent(this, WaylandService::class.java))
        val app = AwbApp.instance
        setContentView(TextView(this).apply {
            setPadding(48, 48, 48, 48)
            textSize = 16f
            text = """
                Wayland compositor running.

                Socket: ${app.socketPath}
                Scale here: ${app.scale120(display.displayId) / 120f}

                In the Debian chroot: scripts/awb-run.sh <app>
                Linux app icons: scripts/install-stub.sh <desktop id>
            """.trimIndent()
        })
    }
}
