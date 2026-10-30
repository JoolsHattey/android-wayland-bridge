package dev.awb

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.util.Log
import java.io.BufferedInputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.InputStream
import java.io.OutputStream
import java.security.MessageDigest
import java.util.concurrent.Executors

/**
 * The control channel to the VM's guest agent (vm/awb-display, agent.rs,
 * which has the protocol): gives the VM's apps what the chroot's get from
 * the root side. Android's dark mode and accent colour go to its settings
 * portal, the caption CSS to its GTK config, links it opens come back for
 * Android's browser, and its app list becomes files/vm-apps, from which the
 * root side (awb-stubs.sh) makes stub apps, `dev.awb.vm.<id>` with app id
 * `vm:<id>` (the compositor gives the VM's windows that prefix), which
 * launch the app through here.
 *
 * As with [VmDisplay], we connect to the guest (it cannot connect to us),
 * retrying while it boots, and keep the connection open.
 */
object VmAgent {
    private const val TAG = "awb-vm"
    const val PORT = 7006

    /** Prefix of the VM's app ids (the chroot's have none). */
    const val PREFIX = "vm:"

    @Volatile private var running = false

    /** Which start the connection loop belongs to: a loop of a stopped VM ends. */
    @Volatile private var generation = 0
    private lateinit var appContext: Context
    private val main = Handler(Looper.getMainLooper())

    /** Writes go through one thread, in order, off the caller's (often main). */
    private val writer = Executors.newSingleThreadExecutor()

    // State the guest gets again on each connection. Writer thread only.
    private var output: OutputStream? = null
    private var appearance: String? = null
    private val files = LinkedHashMap<String, ByteArray>()

    /** Launches asked for before the guest was up. */
    private val pending = ArrayList<String>()

    fun start(context: Context, connect: (Int) -> ParcelFileDescriptor) {
        if (running) return
        running = true
        appContext = context.applicationContext
        // The last caption CSS, until a window reports its caption.
        for (name in listOf("gtk.css", "caption.json")) {
            runCatching { File(context.filesDir, name).readText() }.getOrNull()?.let { file(name, it) }
        }
        val g = ++generation
        Thread({ loop(g, connect) }, "awb-vm-agent").apply { isDaemon = true }.start()
    }

    fun stop() {
        running = false
        writer.execute {
            runCatching { output?.close() }
            output = null
        }
    }

    /** Android's appearance, for the guest's settings portal. [accent] is ARGB; 0 = none. */
    fun appearance(dark: Boolean, accent: Int) {
        val rgb = if (accent == 0) "-" else "%06x".format(accent and 0xffffff)
        val line = "appearance ${if (dark) 1 else 0} $rgb\n"
        writer.execute {
            if (line == appearance) return@execute
            appearance = line
            send(line.toByteArray())
        }
    }

    /** A generated file for the guest's ~/.config/awb (gtk.css, caption.json). */
    fun file(name: String, text: String) {
        val data = text.toByteArray()
        writer.execute {
            if (files[name]?.contentEquals(data) == true) return@execute
            files[name] = data
            send("file $name ${data.size}\n".toByteArray() + data)
        }
    }

    /**
     * Runs the VM's app [id] (a desktop file id), with [uri] as its link.
     * Starts the VM if it isn't running; the launch waits for its agent.
     */
    fun launch(context: Context, id: String, uri: String?) {
        val line = if (uri != null) "launch $id $uri\n" else "launch $id\n"
        if (!running) {
            Log.i(TAG, "launch $id: starting the VM")
            context.startForegroundService(Intent(context, WaylandService::class.java).setAction(WaylandService.ACTION_START_VM))
        }
        writer.execute {
            if (!send(line.toByteArray())) pending += line
        }
    }

    /** Writer thread. False if not connected. */
    private fun send(data: ByteArray): Boolean {
        val out = output ?: return false
        return runCatching { out.write(data) }.onFailure {
            runCatching { out.close() }
            output = null
        }.isSuccess
    }

    private fun loop(g: Int, connect: (Int) -> ParcelFileDescriptor) {
        while (running && generation == g) {
            val pfd = runCatching { connect(PORT) }.getOrNull()
            if (pfd == null) {
                Thread.sleep(1000)
                continue
            }
            val out = FileOutputStream(pfd.fileDescriptor)
            writer.execute {
                output = out
                // What the guest should have: it may be new (a boot) or a restarted agent.
                appearance?.let { send(it.toByteArray()) }
                for ((name, data) in files) send("file $name ${data.size}\n".toByteArray() + data)
                pending.forEach { send(it.toByteArray()) }
                pending.clear()
            }
            Log.i(TAG, "agent connected")
            runCatching { read(BufferedInputStream(FileInputStream(pfd.fileDescriptor), 1 shl 16)) }
                .onFailure { Log.w(TAG, "agent connection: $it") }
            writer.execute { if (output === out) output = null }
            runCatching { pfd.close() }
            Log.i(TAG, "agent disconnected")
            Thread.sleep(1000)
        }
    }

    /** The guest's messages, until it disconnects. */
    private fun read(input: InputStream) {
        var apps: MutableMap<String, Pair<ByteArray, ByteArray>>? = null
        while (true) {
            val words = line(input)?.split(' ') ?: return
            when (words[0]) {
                "hello" -> Log.i(TAG, "agent protocol ${words.getOrNull(1)}")
                "open" -> words.getOrNull(1)?.let(::open)
                "apps" -> when (words.getOrNull(1)) {
                    "begin" -> apps = LinkedHashMap()
                    "end" -> apps?.let(::mirror).also { apps = null }
                }
                "app" -> {
                    require(words.size == 4) { "bad app message" }
                    val info = bytes(input, words[2].toInt())
                    val icon = bytes(input, words[3].toInt())
                    apps?.put(words[1], info to icon)
                }
                else -> Log.w(TAG, "agent: unknown message ${words[0]}")
            }
        }
    }

    private fun line(input: InputStream): String? {
        val buf = ByteArrayOutputStream()
        while (true) {
            val b = input.read()
            if (b < 0) return null
            if (b == '\n'.code) return buf.toString(Charsets.UTF_8)
            require(buf.size() < 4096) { "line too long" }
            buf.write(b)
        }
    }

    private fun bytes(input: InputStream, n: Int): ByteArray {
        require(n in 0..(4 shl 20)) { "bad length $n" }
        val data = ByteArray(n)
        var at = 0
        while (at < n) {
            val k = input.read(data, at, n - at)
            if (k < 0) error("short payload")
            at += k
        }
        return data
    }

    /** A web link from the guest (awb-open), opened by Android's browser. */
    private fun open(url: String) {
        // Only web links, as a VIEW intent's data, like the chroot's (awb-launcher.sh).
        if (!url.matches(Regex("https?://[\\x21-\\x7e]+"))) {
            Log.w(TAG, "agent: not a web link")
            return
        }
        main.post {
            runCatching {
                appContext.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            }.onFailure { Log.e(TAG, "open link: $it") }
        }
    }

    /**
     * The guest's apps as files/vm-apps: per app `<id>.info` (name, schemes
     * and a stamp of both and the icon, as awb-stubs.sh reads them) and
     * `<id>.png`. Only changed files are written, so the root side's sync
     * runs only on changes.
     */
    private fun mirror(apps: Map<String, Pair<ByteArray, ByteArray>>) {
        val dir = File(appContext.filesDir, "vm-apps").apply { mkdirs() }
        val keep = HashSet<String>()
        for ((id, value) in apps) {
            val (info, icon) = value
            val text = info.toString(Charsets.UTF_8)
            // Ids and names end up in package names and shell variables.
            if (!id.matches(Regex("[A-Za-z0-9._-]+")) || text.any { it < ' ' && it != '\n' }) continue
            val stamp = MessageDigest.getInstance("SHA-256").run {
                update(info)
                update(icon)
                digest().take(6).joinToString("") { "%02x".format(it) }
            }
            write(File(dir, "$id.info"), (text + "stamp=$stamp\n").toByteArray())
            keep += "$id.info"
            if (icon.isNotEmpty()) {
                write(File(dir, "$id.png"), icon)
                keep += "$id.png"
            }
        }
        dir.listFiles()?.filter { it.name !in keep }?.forEach { it.delete() }
        Log.i(TAG, "agent: ${apps.size} VM apps")
    }

    private fun write(file: File, data: ByteArray) {
        if (runCatching { file.readBytes() }.getOrNull()?.contentEquals(data) == true) return
        val tmp = File(file.path + ".tmp")
        tmp.writeBytes(data)
        tmp.renameTo(file)
    }
}
