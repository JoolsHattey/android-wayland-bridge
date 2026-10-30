package dev.awb

import android.content.Context
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.ParcelFileDescriptor
import android.system.Os
import android.system.OsConstants
import android.util.Log
import java.io.File
import java.lang.reflect.Proxy
import java.util.concurrent.Executors

/**
 * Runs a Debian VM through Android's Virtualization Framework, with its
 * Wayland apps on our compositor: over vsock and waypipe ([VmDisplay]), or
 * with `vm_transport=crossdomain` in files/awb.properties, through crosvm's
 * cross-domain channel ([crossDomain]). Needs no root, only adb: the VM permissions are
 * development permissions (`pm grant`), and the AVF classes are system
 * APIs, called by reflection (`settings put global hidden_api_policy 1`).
 *
 * The image (vmlinuz, initrd.img, root_part, cidata.iso) is the Terminal's
 * Debian image, in files/vm.
 */
object VmLauncher {
    private const val TAG = "awb-vm"
    private const val NAME = "awb-debian"
    private const val PKG = "android.system.virtualmachine."

    /** O_PATH: virtmgr opens the socket from the fd's /proc path. Not in OsConstants. */
    private const val O_PATH = 0x200000

    private var vm: Any? = null

    fun start(context: Context) {
        if (vm != null) return
        val dir = File(context.filesDir, "vm")
        val crossDomain = AwbApp.instance.setting("vm_transport", "vsock") == "crossdomain"
        val root = cls("VirtualMachineCustomImageConfig\$Disk")
            .getDeclaredConstructor(Boolean::class.java, String::class.java, String::class.java)
            .apply { isAccessible = true }
            .newInstance(true, null, null)
            .call(
                "addPartition",
                new(
                    "VirtualMachineCustomImageConfig\$Partition",
                    "ROOT", File(dir, "root_part").path, true, "FD202E72-0241-4C3B-953D-FFE506AE6CFC",
                ),
            )
        val cidata = static("VirtualMachineCustomImageConfig\$Disk", "RODisk", File(dir, "cidata.iso").path)
        var image = new("VirtualMachineCustomImageConfig\$Builder")
            .call("setName", NAME)
            .call("setKernelPath", File(dir, "vmlinuz").path)
            .call("setInitrdPath", File(dir, "initrd.img").path)
            .call("addDisk", root)
            .call("addDisk", cidata)
            .call("useNetwork", true)
        // Without a GPU, virtmgr runs crosvm_minimal, which has no network.
        image = if (crossDomain) crossDomain(image) else image.call("setGpuConfig", gpu(null))
        for (p in listOf(
            "root=/dev/vda1", "ds=nocloud", "8250.nr_uarts=4", "console=ttyS0", "hostname=awb-debian",
        )) image = image.call("addParam", p)

        val config = new("VirtualMachineConfig\$Builder", context)
            .call("setCustomImageConfig", image.call("build"))
            .call("setProtectedVm", false)
            .call("setMemoryBytes", 4096L shl 20)
            .call("setCpuTopology", 1) // CPU_TOPOLOGY_MATCH_HOST
            .call("setDebugLevel", 1) // DEBUG_LEVEL_FULL
            .call("setVmOutputCaptured", true)
            .call("build")

        val vmm = context.getSystemService(cls("VirtualMachineManager"))!!
        runCatching { vmm.call("delete", NAME) } // Each start takes the current config.
        val machine = vmm.call("create", NAME, config)
        machine.call("setCallback", Executors.newSingleThreadExecutor(), callback())
        machine.call("run")
        vm = machine
        val connect = { port: Int -> machine.call("connectVsock", port.toLong()) as ParcelFileDescriptor }
        if (!crossDomain) VmDisplay.start(context, connect)
        VmAgent.start(context, connect)
        Log.i(TAG, "started; status ${machine.call("getStatus")}")
        capture(machine.call("getConsoleOutput") as java.io.InputStream, File(dir, "console.log"))
        capture(machine.call("getLogOutput") as java.io.InputStream, File(dir, "log.txt"))
    }

    /**
     * crosvm's cross-domain channel, as the Linux Terminal uses: the guest's
     * sommelier (started by the `wayland_bridge=true` parameter) reaches our
     * socket through the VM's virtio-gpu. Fastest, but SELinux lets crosvm
     * connect only to the Terminal's socket, so only on permissive devices.
     */
    private fun crossDomain(image: Any): Any {
        awaitCompositor()
        val socket = Os.open(AwbApp.instance.socketPath, O_PATH or OsConstants.O_CLOEXEC, 0)
        val socketFd = ParcelFileDescriptor.dup(socket).also { Os.close(socket) }
        // Unused (crosvm also shows it as a window), but without a display
        // the guest gets no cross-domain context.
        val display = new("VirtualMachineCustomImageConfig\$DisplayConfig\$Builder")
            .call("setWidth", 1280)
            .call("setHeight", 720)
            .call("setHorizontalDpi", 160)
            .call("setVerticalDpi", 160)
            .call("setRefreshRate", 60)
            .call("build")
        return image.call("setGpuConfig", gpu(socketFd)).call("setDisplayConfig", display).call("addParam", "wayland_bridge=true")
    }

    private fun gpu(waylandSocket: ParcelFileDescriptor?): Any {
        var gpu = new("VirtualMachineCustomImageConfig\$GpuConfig\$Builder")
            .call("setContextTypes", arrayOf("cross-domain"))
            .call("setRendererUseEgl", true)
            .call("setRendererUseGles", true)
            .call("setRendererUseSurfaceless", true)
        if (waylandSocket != null) gpu = gpu.call("setWaylandSocketFd", waylandSocket)
        return gpu.call("build")
    }

    /**
     * Waits (up to 5 s) for the compositor to listen: it binds its socket on
     * its own thread after [Native.start], replacing the last run's file, and
     * an fd to that stale file would refuse every connection.
     */
    private fun awaitCompositor() {
        val address = LocalSocketAddress(AwbApp.instance.socketPath, LocalSocketAddress.Namespace.FILESYSTEM)
        repeat(50) {
            if (runCatching { LocalSocket().use { it.connect(address) } }.isSuccess) return
            Thread.sleep(100)
        }
        Log.w(TAG, "compositor socket not listening; starting anyway")
    }

    fun stop() {
        vm?.let { runCatching { it.call("stop") } }
        vm = null
        VmDisplay.stop()
        VmAgent.stop()
    }

    private fun callback(): Any {
        val iface = cls("VirtualMachineCallback")
        return Proxy.newProxyInstance(iface.classLoader, arrayOf(iface)) { _, method, args ->
            Log.i(TAG, "${method.name}(${args?.drop(1)?.joinToString()})")
            if (method.name == "onStopped" || method.name == "onDied") {
                vm = null
                // Their connections are to this machine; the next start brings new ones.
                VmDisplay.stop()
                VmAgent.stop()
            }
            null
        }
    }

    private fun capture(input: java.io.InputStream, file: File) = Thread {
        file.outputStream().use { out -> runCatching { input.copyTo(out, 4096) } }
    }.apply { isDaemon = true }.start()

    private fun cls(name: String): Class<*> = Class.forName(PKG + name)

    private fun new(name: String, vararg args: Any?): Any {
        val ctor = cls(name).declaredConstructors.first { it.parameterCount == args.size && accepts(it.parameterTypes, args) }
        ctor.isAccessible = true
        return ctor.newInstance(*args)
    }

    private fun static(name: String, method: String, vararg args: Any?): Any? =
        invoke(cls(name), null, method, args)

    private fun Any.call(method: String, vararg args: Any?): Any =
        invoke(javaClass, this, method, args) ?: this

    private fun invoke(c: Class<*>, target: Any?, name: String, args: Array<out Any?>): Any? {
        val m = c.methods.first { it.name == name && it.parameterCount == args.size && accepts(it.parameterTypes, args) }
        return m.invoke(target, *args)
    }

    private fun accepts(types: Array<Class<*>>, args: Array<out Any?>) = types.indices.all { i ->
        val a = args[i] ?: return@all !types[i].isPrimitive
        val t = when (types[i]) {
            Boolean::class.javaPrimitiveType -> java.lang.Boolean::class.java
            Int::class.javaPrimitiveType -> java.lang.Integer::class.java
            Long::class.javaPrimitiveType -> java.lang.Long::class.java
            else -> types[i]
        }
        t.isInstance(a)
    }
}
