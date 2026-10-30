package dev.awb

import android.content.Context
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.ParcelFileDescriptor
import android.util.Log
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.InputStream
import java.io.OutputStream

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

    fun start(context: Context, connect: (Int) -> ParcelFileDescriptor) {
        if (running) return
        running = true
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
        spare(sock, connect)
    }

    fun stop() {
        running = false
        waypipe?.destroy()
        waypipe = null
    }

    /**
     * Opens a spare connection to awb-display (retrying until the guest is
     * up) and waits on it; when a Linux app starts, its first bytes arrive,
     * and the spare becomes that app's connection while a new spare opens.
     */
    private fun spare(sock: File, connect: (Int) -> ParcelFileDescriptor): Unit = Thread({
        var vm: ParcelFileDescriptor? = null
        while (running && vm == null) {
            vm = runCatching { connect(PORT) }.getOrNull() ?: run { Thread.sleep(1000); null }
        }
        vm ?: return@Thread
        val input = FileInputStream(vm.fileDescriptor)
        val output = FileOutputStream(vm.fileDescriptor)
        val buffer = ByteArray(1 shl 16)
        val n = runCatching { input.read(buffer) }.getOrDefault(-1)
        spare(sock, connect)
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
        val close = {
            runCatching { vm.close() }
            runCatching { local.close() }
            Unit
        }
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
