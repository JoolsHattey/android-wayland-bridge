package dev.awb

import android.app.Activity
import android.os.Bundle
import android.view.Display
import android.widget.TextView
import dev.awb.window.Window

/** The installed stubs, one line each, flagging those whose manifest is outdated. */
private fun stubs(app: AwbApp): String =
    app.stubs().sortedBy { it.first }.joinToString("") { (id, _, format) ->
        "\n  $id" + if (format < Window.STUB_FORMAT) " (outdated: run scripts/sync-stubs.sh)" else ""
    }.ifEmpty { "\n  none" }

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
                Scale here: ${app.scale120(display?.displayId ?: Display.DEFAULT_DISPLAY) / 120f}

                In the Debian chroot: scripts/awb-run.sh <app>
                Linux app icons: scripts/install-stub.sh <desktop id>, or
                scripts/sync-stubs.sh (run by deploy.sh) for the apps used here.

                Stub apps:
            """.trimIndent() + stubs(app)
        })
    }
}
