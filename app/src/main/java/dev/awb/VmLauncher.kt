package dev.awb

import android.content.Context
import android.content.pm.PackageManager
import android.net.LocalSocket
import android.net.LocalSocketAddress
import android.os.ParcelFileDescriptor
import android.system.Os
import android.system.OsConstants
import android.util.Log
import java.io.File
import java.lang.reflect.Proxy
import java.util.concurrent.CopyOnWriteArraySet
import java.util.concurrent.Executors

/**
 * Runs a Debian VM through Android's Virtualization Framework, with its
 * Wayland apps on our compositor: over vsock and waypipe ([VmDisplay]), or
 * with `vm_transport=crossdomain` in files/awb.properties, through crosvm's
 * cross-domain channel ([crossDomain]). Needs no root, only adb: the VM permissions are
 * development permissions (`pm grant`), and the AVF classes are system
 * APIs, called by reflection (`settings put global hidden_api_policy 1`).
 *
 * The image (vmlinuz, initrd.img, root_part) is the Terminal's Debian image
 * in files/vm ([VmImage]), with our own cidata disk ([VmCiData]), from which
 * the guest sets itself up. [state] is for the UI and the notification.
 */
object VmLauncher {
    private const val TAG = "awb-vm"
    private const val NAME = "awb-debian"
    private const val PKG = "android.system.virtualmachine."

    /** O_PATH: virtmgr opens the socket from the fd's /proc path. Not in OsConstants. */
    private const val O_PATH = 0x200000

    enum class State { STOPPED, STARTING, RUNNING, READY }

    @Volatile var state = State.STOPPED
        private set
    /** Why the last start failed or the VM last stopped, for the UI. */
    @Volatile var lastExit: String? = null
        private set
    private val listeners = CopyOnWriteArraySet<() -> Unit>()

    fun observe(listener: () -> Unit) = listeners.add(listener)
    fun unobserve(listener: () -> Unit) = listeners.remove(listener)

    private fun set(s: State, exit: String? = lastExit) {
        state = s
        lastExit = exit
        listeners.forEach { it() }
    }

    private var vm: Any? = null

    /**
     * What stops us starting a VM here, as the adb commands that fix it:
     * the permissions are development ones, and the AVF classes system APIs
     * that apps may reach only with hidden API checks off. Empty when none.
     */
    fun problems(context: Context): List<String> {
        val user = android.os.Process.myUid() / 100_000
        val out = mutableListOf<String>()
        if (!context.packageManager.hasSystemFeature("android.software.virtualization_framework")) {
            return listOf("This device has no Android Virtualization Framework.")
        }
        for (p in listOf("MANAGE_VIRTUAL_MACHINE", "USE_CUSTOM_VIRTUAL_MACHINE")) {
            if (context.checkSelfPermission("android.permission.$p") != PackageManager.PERMISSION_GRANTED) {
                out += "adb shell pm grant --user $user ${context.packageName} android.permission.$p"
            }
        }
        val reachable = runCatching {
            cls("VirtualMachineCustomImageConfig\$Builder").methods.any { it.name == "setKernelPath" }
        }.getOrDefault(false)
        if (!reachable) out += "adb shell settings put global hidden_api_policy 1"
        return out
    }

    /** Starts the VM on a thread of its own (it waits for the compositor). */
    fun startAsync(context: Context) {
        if (state != State.STOPPED) return
        set(State.STARTING, null)
        Thread({
            runCatching { start(context) }.onFailure {
                Log.e(TAG, "start failed", it)
                vm = null
                VmDisplay.stop()
                VmAgent.stop()
                set(State.STOPPED, "Start failed: ${(it.cause ?: it).message ?: it}")
            }
        }, "awb-vm-start").start()
    }

    @Synchronized private fun start(context: Context) {
        if (vm != null) return
        problems(context).takeIf { it.isNotEmpty() }?.let { error("needs ${it.joinToString("; ")}") }
        if (!VmImage.installed(context)) error("no VM image installed")
        val dir = VmImage.dir(context)
        val cidataFile = File(dir, "awb-cidata.iso")
        VmCiData.write(context, cidataFile)
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
        val cidata = static("VirtualMachineCustomImageConfig\$Disk", "RODisk", cidataFile.path)
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
        set(State.RUNNING)
        val connect = { port: Int -> machine.call("connectVsock", port.toLong()) as ParcelFileDescriptor }
        if (!crossDomain) VmDisplay.start(context, { if (vm === machine && state != State.READY) set(State.READY) }, connect)
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
        val machine = vm ?: return
        vm = null
        runCatching { machine.call("stop") }
        VmDisplay.stop()
        VmAgent.stop()
        set(State.STOPPED, null)
    }

    /** Also hears the VM stop on its own (guest poweroff, crash, killed crosvm). */
    private fun callback(): Any {
        val iface = cls("VirtualMachineCallback")
        lateinit var proxy: Any
        proxy = Proxy.newProxyInstance(iface.classLoader, arrayOf(iface)) { _, method, args ->
            when (method.name) {
                "equals" -> return@newProxyInstance args?.get(0) === proxy
                "hashCode" -> return@newProxyInstance System.identityHashCode(proxy)
                "toString" -> return@newProxyInstance "awb-vm-callback"
            }
            Log.i(TAG, "${method.name}(${args?.drop(1)?.joinToString()})")
            val machine = args?.getOrNull(0)
            if ((method.name == "onStopped" || method.name == "onDied") && machine != null && machine === vm) {
                vm = null
                // Their connections are to this machine; the next start brings new ones.
                VmDisplay.stop()
                VmAgent.stop()
                set(State.STOPPED, "VM stopped (reason ${args.getOrNull(1)})")
            }
            if (method.name == "onError") lastExit = "VM error ${args?.drop(1)?.joinToString()}"
            null
        }
        return proxy
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
