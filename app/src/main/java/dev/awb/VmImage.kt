package dev.awb

import android.content.Context
import android.util.Log
import java.io.File
import java.io.FilterInputStream
import java.io.InputStream
import java.io.RandomAccessFile
import java.net.HttpURLConnection
import java.net.URL
import java.nio.ByteBuffer
import java.util.concurrent.CopyOnWriteArraySet
import java.util.zip.GZIPInputStream

/**
 * The VM's Debian image in files/vm: the Linux Terminal's, from the URL the
 * Terminal installs from (ImageArchive.HOST_URL in its APK, per ABI). The
 * archive (images.tar.gz, ~620 MB) holds vmlinuz, initrd.img, root_part
 * (the ext4 root, a bare partition), vm_config.json and build_id; the
 * Terminal's cidata.iso is in its APK, and we write our own ([VmCiData]).
 *
 * The download resumes (HTTP Range, checked with If-Range against the
 * ETag) and goes to files/images.tar.gz.part; gzip's CRC and the required
 * entries validate it. It unpacks to files/vm.new, renamed to files/vm when
 * complete, so files/vm is either absent or whole.
 */
object VmImage {
    private const val TAG = "awb-vm"
    const val URL = "https://dl.google.com/android/ferrochrome/5100000/x86_64/images.tar.gz"
    private val REQUIRED = listOf("vmlinuz", "initrd.img", "root_part")

    /** root_part's size after install: the guest grows its file system to it (sparse until used). */
    private const val ROOT_BYTES = 32L shl 30

    sealed interface State {
        data object Missing : State
        data object Installed : State
        data class Working(val what: String, val done: Long, val total: Long) : State
        data class Failed(val error: String) : State
    }

    @Volatile var state: State = State.Missing
        private set
    private val listeners = CopyOnWriteArraySet<() -> Unit>()

    fun observe(listener: () -> Unit) = listeners.add(listener)
    fun unobserve(listener: () -> Unit) = listeners.remove(listener)

    fun dir(context: Context) = File(context.filesDir, "vm")
    fun installed(context: Context) = REQUIRED.all { File(dir(context), it).isFile }
    fun buildId(context: Context): String? = runCatching { File(dir(context), "build_id").readText().trim() }.getOrNull()

    /** Whether we installed the image (and made root_part larger for its file system to grow into). */
    fun grows(context: Context) = File(dir(context), GROW_MARKER).isFile
    private const val GROW_MARKER = "awb-grow-root"

    fun refresh(context: Context) {
        if (state !is State.Working) set(if (installed(context)) State.Installed else state.takeIf { it is State.Failed } ?: State.Missing)
    }

    private fun set(s: State) {
        state = s
        listeners.forEach { it() }
    }

    /** Downloads and unpacks the image on a thread of its own, unless installed or under way. */
    @Synchronized fun install(context: Context) {
        if (state is State.Working || installed(context)) return refresh(context)
        set(State.Working("Starting download", 0, 0))
        Thread({
            runCatching {
                // A whole download left by a process killed while unpacking.
                // Deleted either way: a bad one is downloaded again.
                val archive = File(context.filesDir, "images.tar.gz").takeIf { it.isFile } ?: download(context)
                try {
                    unpack(context, archive)
                } finally {
                    archive.delete()
                    File(context.filesDir, "vm.new").deleteRecursively()
                }
                set(State.Installed)
                Log.i(TAG, "image installed: ${buildId(context)}")
            }.onFailure {
                Log.e(TAG, "image install failed", it)
                set(State.Failed(it.message ?: it.toString()))
            }
        }, "awb-vm-install").start()
    }

    private fun download(context: Context): File {
        val part = File(context.filesDir, "images.tar.gz.part")
        val etagFile = File(context.filesDir, "images.tar.gz.etag")
        val have = if (part.isFile) part.length() else 0L
        val conn = URL(URL).openConnection() as HttpURLConnection
        conn.connectTimeout = 30_000
        conn.readTimeout = 60_000
        val etag = runCatching { etagFile.readText() }.getOrNull()
        if (have > 0 && etag != null) {
            conn.setRequestProperty("Range", "bytes=$have-")
            conn.setRequestProperty("If-Range", etag)
        }
        val resumed = conn.responseCode == HttpURLConnection.HTTP_PARTIAL
        if (!resumed && conn.responseCode != HttpURLConnection.HTTP_OK) error("download: HTTP ${conn.responseCode}")
        conn.getHeaderField("ETag")?.let { etagFile.writeText(it) }
        val start = if (resumed) have else 0L
        val total = start + conn.contentLengthLong
        Log.i(TAG, "downloading $URL from $start of $total")
        RandomAccessFile(part, "rw").use { out ->
            out.setLength(start)
            out.seek(start)
            conn.inputStream.use { input ->
                val buffer = ByteArray(1 shl 16)
                var done = start
                var shown = 0L
                while (true) {
                    val n = input.read(buffer)
                    if (n < 0) break
                    out.write(buffer, 0, n)
                    done += n
                    if (done - shown > (4 shl 20)) {
                        shown = done
                        set(State.Working("Downloading", done, total))
                    }
                }
                if (conn.contentLengthLong >= 0 && done != total) error("download cut short at $done of $total bytes")
            }
        }
        val archive = File(context.filesDir, "images.tar.gz")
        if (!part.renameTo(archive)) error("cannot rename ${part.path}")
        etagFile.delete()
        return archive
    }

    /**
     * Unpacks the flat tar (ustar headers) into files/vm.new: regular files
     * only, names without a path, zero blocks left as holes so root_part
     * stays sparse. GZIPInputStream checks the CRC at the end.
     */
    private fun unpack(context: Context, archive: File) {
        val tmp = File(context.filesDir, "vm.new")
        tmp.deleteRecursively()
        tmp.mkdirs()
        val counted = Counting(archive.inputStream())
        val total = archive.length()
        GZIPInputStream(counted, 1 shl 16).use { tar ->
            val header = ByteArray(512)
            val buffer = ByteArray(1 shl 16)
            var shown = 0L
            while (true) {
                if (!readFully(tar, header, 512)) error("archive ends early")
                if (header.all { it == 0.toByte() }) break
                val name = String(header, 0, 100).substringBefore('\u0000').removePrefix("./")
                val size = octal(header, 124, 12)
                val type = header[156].toInt().toChar()
                val ok = (type == '0' || type == '\u0000') && name.isNotEmpty() && '/' !in name && name != ".."
                if (!ok) Log.w(TAG, "skipping tar entry '$name' (type $type)")
                val out = if (ok) RandomAccessFile(File(tmp, name), "rw").also { it.setLength(0) } else null
                var left = size
                var pos = 0L
                while (left > 0) {
                    val n = tar.read(buffer, 0, minOf(left, buffer.size.toLong()).toInt())
                    if (n < 0) error("archive ends early in $name")
                    out?.let { if (!zero(buffer, n)) it.channel.write(ByteBuffer.wrap(buffer, 0, n), pos) }
                    pos += n
                    left -= n
                    if (counted.count - shown > (4 shl 20)) {
                        shown = counted.count
                        set(State.Working("Unpacking", counted.count, total))
                    }
                }
                out?.setLength(size)
                out?.close()
                val padding = (512 - size % 512) % 512
                if (!readFully(tar, header, padding.toInt())) error("archive ends early after $name")
            }
            // Drain to the gzip trailer, so its CRC is checked.
            while (tar.read(buffer) >= 0) Unit
        }
        for (name in REQUIRED + "build_id") if (!File(tmp, name).isFile) error("archive has no $name")
        RandomAccessFile(File(tmp, "root_part"), "rw").use { if (it.length() < ROOT_BYTES) it.setLength(ROOT_BYTES) }
        File(tmp, GROW_MARKER).createNewFile()
        // Only logs, say, or a broken image: kept aside, not deleted.
        val dir = dir(context)
        if (dir.exists()) dir.renameTo(File(context.filesDir, "vm.old-${System.currentTimeMillis()}"))
        if (!tmp.renameTo(dir)) error("cannot rename ${tmp.path}")
    }

    private fun zero(b: ByteArray, n: Int): Boolean {
        for (i in 0 until n) if (b[i] != 0.toByte()) return false
        return true
    }

    private fun readFully(input: InputStream, b: ByteArray, n: Int): Boolean {
        var off = 0
        while (off < n) {
            val r = input.read(b, off, n - off)
            if (r < 0) return false
            off += r
        }
        return true
    }

    /** A tar number: octal text, or base-256 when the top bit is set (sizes ≥ 8 GiB). */
    private fun octal(h: ByteArray, off: Int, len: Int): Long {
        if (h[off].toInt() and 0x80 != 0) {
            var v = (h[off].toLong() and 0x7f)
            for (i in off + 1 until off + len) v = (v shl 8) or (h[i].toLong() and 0xff)
            return v
        }
        return String(h, off, len).trim { it == ' ' || it == '\u0000' }.ifEmpty { "0" }.toLong(8)
    }

    private class Counting(input: InputStream) : FilterInputStream(input) {
        var count = 0L
        override fun read(): Int = super.read().also { if (it >= 0) count++ }
        override fun read(b: ByteArray, off: Int, len: Int): Int = super.read(b, off, len).also { if (it > 0) count += it }
    }
}
