package dev.awb

import android.content.Context
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.ParcelFileDescriptor
import android.system.Os
import android.system.OsConstants
import android.util.Log
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.InputStream
import java.io.OutputStream
import java.util.concurrent.ConcurrentHashMap

/**
 * Carries the VM's Wayland apps to the compositor over vsock, for devices
 * where crosvm may not connect to our socket (SELinux keeps crosvm's
 * cross-domain channel for the Linux Terminal). In the guest, each app runs
 * under `waypipe server` (vm/awb-run) and awb-display pairs it with
 * a spare vsock connection from here; we then hand that connection to
 * `waypipe client` (the APK's libwaypipe.so), which talks to the compositor.
 *
 * We connect to the guest, not the other way round: Android drops
 * connections from the VM's network to apps, and an app cannot listen on
 * vsock.
 */
object VmDisplay {
    private const val TAG = "awb-vm"
    const val PORT = 7005

    private var waypipe: Process? = null
    @Volatile private var running = false
    /** Bumped on each start, so spares of an earlier VM give up. */
    @Volatile private var generation = 0
    private var onConnected: () -> Unit = {}

    /** [onConnected] runs whenever a spare connects: the guest's awb-display is up. */
    fun start(context: Context, onConnected: () -> Unit, connect: (Int) -> ParcelFileDescriptor) {
        if (running) return
        running = true
        this.onConnected = onConnected
        generation++
        val sock = File(context.filesDir, "waypipe.sock")
        sock.delete()
        waypipe = ProcessBuilder(
            File(context.applicationInfo.nativeLibraryDir, "libwaypipe.so").path,
            "--compress", "none", "--socket", sock.path, "client",
        ).apply {
            environment()["WAYLAND_DISPLAY"] = AwbApp.instance.socketPath
            redirectErrorStream(true)
            redirectOutput(File(context.filesDir, "vm/waypipe.log"))
        }.start()
        spare(sock, connect, generation)
    }

    /**
     * Closes every relay too: when the VM dies its vsock connections may not
     * see EOF, and each waypipe client child (orphaned by [Process.destroy])
     * would keep its app's windows open.
     */
    fun stop() {
        running = false
        waypipe?.destroy()
        waypipe = null
        open.forEach { it() }
    }

    /** Closers of the open spares and relays. */
    private val open = ConcurrentHashMap.newKeySet<() -> Unit>()

    /** Shuts a socket down (waking threads blocked reading it), then closes it. */
    private fun shut(fd: ParcelFileDescriptor) {
        runCatching { Os.shutdown(fd.fileDescriptor, OsConstants.SHUT_RDWR) }
        runCatching { fd.close() }
    }

    /**
     * Opens a spare connection to awb-display (retrying until the guest is
     * up) and waits on it; when a Linux app starts, its first bytes arrive,
     * and the spare becomes that app's connection while a new spare opens.
     */
    private fun spare(sock: File, connect: (Int) -> ParcelFileDescriptor, gen: Int): Unit = Thread({
        var vm: ParcelFileDescriptor? = null
        while (running && gen == generation && vm == null) {
            vm = runCatching { connect(PORT) }.getOrNull() ?: run { Thread.sleep(1000); null }
        }
        vm ?: return@Thread
        val closeSpare = { shut(vm) }
        open += closeSpare
        if (!running || gen != generation) return@Thread closeSpare()
        onConnected()
        val input = FileInputStream(vm.fileDescriptor)
        val output = FileOutputStream(vm.fileDescriptor)
        val buffer = ByteArray(1 shl 16)
        val n = runCatching { input.read(buffer) }.getOrDefault(-1)
        open -= closeSpare
        if (!running || gen != generation) return@Thread closeSpare()
        spare(sock, connect, gen)
        if (n <= 0) {
            vm.close()
            return@Thread
        }
        val local = LocalSocket()
        runCatching {
            local.connect(LocalSocketAddress(sock.path, LocalSocketAddress.Namespace.FILESYSTEM))
            local.outputStream.write(buffer, 0, n)
        }.onFailure {
            Log.w(TAG, "waypipe client not reachable", it)
            vm.close()
            local.close()
            return@Thread
        }
        lateinit var close: () -> Unit
        close = {
            open -= close
            shut(vm)
            runCatching { local.shutdownInput() }
            runCatching { local.shutdownOutput() }
            runCatching { local.close() }
        }
        open += close
        pump(input, local.outputStream, close)
        pump(local.inputStream, output, close)
    }, "awb-vm-spare").apply { isDaemon = true }.start()

    private fun pump(input: InputStream, output: OutputStream, close: () -> Unit) = Thread {
        val buffer = ByteArray(1 shl 16)
        runCatching {
            while (true) {
                val n = input.read(buffer)
                if (n < 0) break
                output.write(buffer, 0, n)
            }
        }
        close()
    }.apply { isDaemon = true }.start()
}
