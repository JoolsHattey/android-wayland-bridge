package dev.awb

import com.android.apksig.ApkSigner
import dev.awb.window.Window
import java.io.File
import java.security.KeyFactory
import java.security.PrivateKey
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.security.spec.PKCS8EncodedKeySpec
import java.util.zip.CRC32
import java.util.zip.ZipEntry
import java.util.zip.ZipFile
import java.util.zip.ZipOutputStream

/**
 * Makes a Linux app's stub APK on the device, with no build tools: from the
 * template stub that dev.awb carries (assets/stub-template.apk, the stub
 * module's template build), by rewriting its binary manifest and swapping
 * in the app's icon, then signing it with the key dev.awb is signed with
 * (the dev.awb.permission.WINDOWS signature permission needs that).
 * awb-stubs.sh runs this as root with app_process, dev.awb's APK as the
 * class path, and the key from a root-only directory:
 *   StubMaker format
 *   StubMaker make <out.apk> <key.pk8> <cert.der> <package> <app id> <label> <version name> <schemes,...> [icon.png]
 * `format` prints the template's stub format (its versionCode).
 */
object StubMaker {
    @JvmStatic
    fun main(args: Array<String>) {
        try {
            when (args.firstOrNull()) {
                "format" -> println(template { format(manifest(it)) })
                "make" -> {
                    require(args.size in 9..10) { "make takes 8 or 9 arguments" }
                    make(File(args[1]), key(File(args[2])), cert(File(args[3])), args[4], args[5], args[6], args[7],
                        args[8].split(',').filter { it.isNotEmpty() }, args.getOrNull(9)?.let(::File))
                }
                else -> error("usage: StubMaker format | make <out.apk> <key.pk8> <cert.der> <package> <app id> <label> <version name> <schemes> [icon.png]")
            }
        } catch (e: Exception) {
            System.err.println("StubMaker: ${e.message ?: e}")
            System.exit(1)
        }
    }

    /** Runs [f] on the template stub, from dev.awb's APK (the class path). */
    private fun <T> template(f: (ZipFile) -> T): T {
        val apk = System.getProperty("java.class.path")!!.split(':').first()
        val file = File.createTempFile("stub-template", ".apk", File("/data/local/tmp"))
        try {
            ZipFile(apk).use { zip ->
                val entry = zip.getEntry("assets/stub-template.apk") ?: error("$apk has no stub template")
                zip.getInputStream(entry).use { input -> file.outputStream().use { input.copyTo(it) } }
            }
            return ZipFile(file).use(f)
        } finally {
            file.delete()
        }
    }

    private fun manifest(zip: ZipFile) = BinaryXml(zip.getInputStream(zip.getEntry(MANIFEST)).readBytes())

    private fun format(xml: BinaryXml): Int =
        xml.root.attrs.first { xml.strings[it.name] == "versionCode" }.data

    private fun make(out: File, key: PrivateKey, cert: X509Certificate, pkg: String, appId: String, label: String,
                     version: String, schemes: List<String>, icon: File?): Unit = template { zip ->
        val xml = manifest(zip)
        val root = xml.root
        xml.setString(xml.attr(root, "package", null)!!, pkg)
        xml.setString(xml.attr(root, "versionName")!!, version)

        val app = root.children.first { xml.name(it) == "application" }
        xml.setString(xml.attr(app, "label")!!, label)
        // The PNG icon slot: the app's icon becomes ic_launcher_png.
        val slot = app.children.first { xml.stringValue(xml.attr(it, "name")) == ICON_SLOT }
        app.children -= slot
        if (icon != null) xml.attr(app, "icon")!!.data = xml.attr(slot, "resource")!!.data

        xml.walk { parent, e ->
            when (xml.name(e)) {
                "meta-data" -> if (xml.stringValue(xml.attr(e, "name")) == Window.META_APP_ID) xml.setString(xml.attr(e, "value")!!, appId)
                "intent-filter" -> {
                    val placeholder = e.children.firstOrNull { xml.stringValue(xml.attr(it, "scheme")) == SCHEME } ?: return@walk
                    // The app's schemes, or no link filter at all.
                    if (schemes.isEmpty()) {
                        parent!!.children -= e
                    } else {
                        val at = e.children.indexOf(placeholder)
                        e.children.removeAt(at)
                        e.children.addAll(at, schemes.map { s -> placeholder.copy().also { xml.setString(xml.attr(it, "scheme")!!, s) } })
                    }
                }
            }
        }

        val unsigned = File(out.path + ".unsigned")
        ZipOutputStream(unsigned.outputStream()).use { zout ->
            fun put(name: String, data: ByteArray, stored: Boolean) {
                val e = ZipEntry(name)
                if (stored) {
                    e.method = ZipEntry.STORED
                    e.size = data.size.toLong()
                    e.compressedSize = data.size.toLong()
                    e.crc = CRC32().apply { update(data) }.value
                }
                zout.putNextEntry(e)
                zout.write(data)
                zout.closeEntry()
            }
            for (e in zip.entries()) {
                // The template's signature goes; apksig makes the new one.
                if (e.name.startsWith("META-INF/")) continue
                val data = when {
                    e.name == MANIFEST -> xml.toByteArray()
                    icon != null && e.name.endsWith("/linux_icon_png.png") -> icon.readBytes()
                    else -> zip.getInputStream(e).readBytes()
                }
                // Stored stays stored (resources.arsc must be); apksig aligns it.
                put(e.name, data, e.method == ZipEntry.STORED)
            }
        }
        ApkSigner.Builder(listOf(ApkSigner.SignerConfig.Builder("awb", key, listOf(cert)).build()))
            .setInputApk(unsigned)
            .setOutputApk(out)
            .setV1SigningEnabled(false)
            .setV2SigningEnabled(true)
            .setV3SigningEnabled(false)
            .setDebuggableApkPermitted(true)
            .build()
            .sign()
        unsigned.delete()
    }

    private fun key(file: File): PrivateKey {
        val spec = PKCS8EncodedKeySpec(file.readBytes())
        return listOf("RSA", "EC").firstNotNullOfOrNull { runCatching { KeyFactory.getInstance(it).generatePrivate(spec) }.getOrNull() }
            ?: error("cannot read the key in $file")
    }

    private fun cert(file: File) =
        file.inputStream().use { CertificateFactory.getInstance("X.509").generateCertificate(it) as X509Certificate }

    private const val MANIFEST = "AndroidManifest.xml"
    private const val ICON_SLOT = "dev.awb.template.icon_png"
    private const val SCHEME = "awb-template-scheme"
}
