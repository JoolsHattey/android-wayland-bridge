package dev.awb

import android.app.Activity
import android.os.Bundle
import android.view.Display
import android.view.View
import android.widget.Button
import android.widget.LinearLayout
import android.widget.ProgressBar
import android.widget.ScrollView
import android.widget.TextView
import dev.awb.window.Window

/** The installed stubs, one line each, flagging those whose manifest is outdated. */
private fun stubs(app: AwbApp): String =
    app.stubs().sortedBy { it.first }.joinToString("") { (id, _, format) ->
        "\n  $id" + if (format < Window.STUB_FORMAT) " (outdated: the root launcher remakes it)" else ""
    }.ifEmpty { "\n  none" }

/**
 * Status page; opening it starts the compositor and keeps it alive. Also
 * installs, starts and stops the Debian VM, or says which adb steps it
 * still needs.
 */
class MainActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var vm: TextView
    private lateinit var progress: ProgressBar
    private lateinit var install: Button
    private lateinit var start: Button
    private lateinit var stop: Button
    private val update = { runOnUiThread { refresh() } }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        startForegroundService(android.content.Intent(this, WaylandService::class.java))
        val pad = (16 * resources.displayMetrics.density).toInt()
        status = TextView(this).apply { textSize = 16f }
        vm = TextView(this).apply { textSize = 16f; setPadding(0, pad, 0, pad / 2) }
        progress = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal).apply { max = 1000 }
        install = Button(this).apply {
            text = "Download the Debian image (620 MB)"
            setOnClickListener { VmImage.install(applicationContext) }
        }
        start = Button(this).apply {
            text = "Start VM"
            setOnClickListener { startForegroundService(WaylandService.vmIntent(context, WaylandService.ACTION_START_VM)) }
        }
        stop = Button(this).apply {
            text = "Stop VM"
            setOnClickListener { startForegroundService(WaylandService.vmIntent(context, WaylandService.ACTION_STOP_VM)) }
        }
        val column = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setPadding(pad * 2, pad * 2, pad * 2, pad * 2)
            for (v in listOf(status, vm, progress, install, start, stop)) addView(v)
        }
        setContentView(ScrollView(this).apply { addView(column) })
    }

    override fun onResume() {
        super.onResume()
        VmImage.observe(update)
        VmLauncher.observe(update)
        VmImage.refresh(this)
        refresh()
    }

    override fun onPause() {
        VmImage.unobserve(update)
        VmLauncher.unobserve(update)
        super.onPause()
    }

    private fun refresh() {
        val app = AwbApp.instance
        status.text = """
            Wayland compositor running.

            Socket: ${app.socketPath}
            Scale here: ${app.scale120(display?.displayId ?: Display.DEFAULT_DISPLAY) / 120f}

            In the Debian chroot: scripts/awb-run.sh <app>
            Linux app icons: made here for each app that opens a window
            or is installed in the chroot (by awb-launcher.sh, as root).

            Stub apps:
        """.trimIndent() + stubs(app)

        val problems = VmLauncher.problems(this)
        val image = VmImage.state
        val installed = VmImage.installed(this)
        val running = VmLauncher.state != VmLauncher.State.STOPPED
        vm.text = buildString {
            append("Debian VM: ${WaylandService.vmState(this@MainActivity)}")
            VmImage.buildId(this@MainActivity)?.let { append("\nImage: $it") }
            when (image) {
                is VmImage.State.Working -> append("\n${WaylandService.progress(image)}")
                is VmImage.State.Failed -> append("\nImage install failed: ${image.error}\n(Download again to resume.)")
                else -> {}
            }
            if (problems.isNotEmpty()) {
                append("\n\nThe VM needs these steps over adb first (no root):\n")
                append(problems.joinToString("\n") { "  $it" })
                append("\nthen reopen this page.")
            }
            if (VmLauncher.state == VmLauncher.State.READY) append("\n\nIn the VM: awb-run <app>")
        }
        progress.visibility = if (image is VmImage.State.Working) View.VISIBLE else View.GONE
        if (image is VmImage.State.Working && image.total > 0) {
            progress.isIndeterminate = false
            progress.progress = (image.done * 1000 / image.total).toInt()
        } else {
            progress.isIndeterminate = true
        }
        install.visibility = if (installed) View.GONE else View.VISIBLE
        install.isEnabled = image !is VmImage.State.Working
        start.visibility = if (installed && !running) View.VISIBLE else View.GONE
        start.isEnabled = problems.isEmpty()
        stop.visibility = if (running) View.VISIBLE else View.GONE
    }
}
