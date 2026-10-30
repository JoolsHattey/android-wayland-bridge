package dev.awb

import android.content.Context
import java.io.File
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.MessageDigest

/**
 * The VM's cloud-init NoCloud seed: an ISO 9660 disk labelled `cidata`,
 * written before each start in place of the Terminal's own (which sets up
 * the Terminal's guest agent, not ours). user-data creates the droid user and
 * runs provision.sh from the disk, which installs the APK's guest side
 * (assets/vm: awb-display, awb-run, the unit) and waypipe. Its instance-id is a
 * hash of the content, so cloud-init provisions again whenever the APK's
 * guest side changes, and only then.
 *
 * `vm_ssh_key` in files/awb.properties (a public key line) adds a key for
 * droid; the image has sshd on port 22.
 */
object VmCiData {
    private const val SECTOR = 2048

    fun write(context: Context, out: File) {
        val files = sortedMapOf<String, ByteArray>()
        for (name in context.assets.list("vm").orEmpty()) {
            files[name] = context.assets.open("vm/$name").use { it.readBytes() }
        }
        files["network-config"] = NETWORK_CONFIG.toByteArray()
        val grow = VmImage.grows(context)
        files["user-data"] = userData(AwbApp.instance.setting("vm_ssh_key", ""), grow).toByteArray()
        val digest = MessageDigest.getInstance("SHA-256")
        for ((name, data) in files) digest.update(name.toByteArray() + 0 + data)
        val id = digest.digest().take(8).joinToString("") { "%02x".format(it) }
        files["meta-data"] = "instance-id: awb/$id\nlocal-hostname: awb-debian\n".toByteArray()
        val tmp = File(out.path + ".tmp")
        tmp.writeBytes(iso("cidata", files))
        tmp.renameTo(out)
    }

    private fun userData(sshKey: String, grow: Boolean) = """
        #cloud-config
        # Written by dev.awb (VmCiData).
        users:
          - name: droid
            gecos: Wayland Bridge
            uid: 1000
            no_user_group: true
            groups: [sudo, video, render]
            sudo: "ALL=(ALL) NOPASSWD:ALL"
            shell: /bin/bash
            lock_passwd: false
    """.trimIndent() + (if (sshKey.isNotBlank()) "\n    ssh_authorized_keys: [\"${sshKey.trim()}\"]" else "") + """

        # root_part is a bare partition: no table to grow, only the file
        # system, and that only in images VmImage made larger for it (an
        # image the Terminal made has its own sizing).
        growpart: {mode: "off"}
        resize_rootfs: $grow
        ssh_deletekeys: false
        runcmd:
          - [sh, -c, "mkdir -p /run/awb-cidata && mount -o ro /dev/disk/by-label/cidata /run/awb-cidata && sh /run/awb-cidata/provision.sh; umount /run/awb-cidata"]
    """.trimIndent() + "\n"

    /** The Terminal's: DHCP on every en* link (else systemd-networkd-wait-online hangs). */
    private val NETWORK_CONFIG = """
        network:
          version: 2
          ethernets:
            all-en:
              match: {name: "en*"}
              dhcp4: true
              dhcp6: true
    """.trimIndent() + "\n"

    /**
     * A minimal ISO 9660 image: one root directory of plain files, no Rock
     * Ridge or Joliet. Linux shows the names as written (lower case, without
     * the `;1` version), which is all cloud-init needs.
     */
    private fun iso(label: String, files: Map<String, ByteArray>): ByteArray {
        val names = files.keys.sorted()
        val rootSector = 20
        var next = rootSector + 1
        val extents = names.associateWith { name ->
            next.also { next += (files.getValue(name).size + SECTOR - 1) / SECTOR }
        }
        val buf = ByteBuffer.allocate(next * SECTOR).order(ByteOrder.LITTLE_ENDIAN)

        val root = ByteBuffer.allocate(SECTOR).order(ByteOrder.LITTLE_ENDIAN)
        record(root, rootSector, SECTOR, true, byteArrayOf(0))
        record(root, rootSector, SECTOR, true, byteArrayOf(1))
        for (name in names) record(root, extents.getValue(name), files.getValue(name).size, false, "$name;1".toByteArray())
        check(root.position() <= SECTOR) { "too many cidata files" }

        // Primary volume descriptor.
        buf.position(16 * SECTOR)
        buf.put(1).put("CD001".toByteArray()).put(1).put(0)
        buf.put(pad("", 32)).put(pad(label, 32)).put(ByteArray(8))
        both32(buf, next)
        buf.put(ByteArray(32))
        both16(buf, 1); both16(buf, 1); both16(buf, SECTOR)
        both32(buf, 10) // path table size
        buf.putInt(18).putInt(0)
        buf.order(ByteOrder.BIG_ENDIAN).putInt(19).putInt(0).order(ByteOrder.LITTLE_ENDIAN)
        record(buf, rootSector, SECTOR, true, byteArrayOf(0))
        buf.put(pad("", 128 * 4 + 37 * 3))
        repeat(4) { buf.put(pad("", 16, '0')).put(0) } // dates: unset
        buf.put(1)
        // Terminator.
        buf.position(17 * SECTOR)
        buf.put(255.toByte()).put("CD001".toByteArray()).put(1)
        // Path tables (little-, then big-endian), just the root.
        for ((sector, order) in listOf(18 to ByteOrder.LITTLE_ENDIAN, 19 to ByteOrder.BIG_ENDIAN)) {
            buf.position(sector * SECTOR)
            buf.put(1).put(0).order(order).putInt(rootSector).putShort(1).put(0).put(0)
            buf.order(ByteOrder.LITTLE_ENDIAN)
        }
        buf.position(rootSector * SECTOR)
        buf.put(root.array())
        for (name in names) {
            buf.position(extents.getValue(name) * SECTOR)
            buf.put(files.getValue(name))
        }
        return buf.array()
    }

    private fun record(buf: ByteBuffer, sector: Int, size: Int, dir: Boolean, name: ByteArray) {
        val length = 33 + name.size + (if (name.size % 2 == 0) 1 else 0)
        buf.put(length.toByte()).put(0)
        both32(buf, sector)
        both32(buf, size)
        buf.put(byteArrayOf(126, 1, 1, 0, 0, 0, 0)) // 2026-01-01
        buf.put((if (dir) 2 else 0).toByte()).put(0).put(0)
        both16(buf, 1)
        buf.put(name.size.toByte()).put(name)
        if (name.size % 2 == 0) buf.put(0)
    }

    private fun both32(buf: ByteBuffer, v: Int) {
        buf.order(ByteOrder.LITTLE_ENDIAN).putInt(v).order(ByteOrder.BIG_ENDIAN).putInt(v).order(ByteOrder.LITTLE_ENDIAN)
    }

    private fun both16(buf: ByteBuffer, v: Int) {
        buf.order(ByteOrder.LITTLE_ENDIAN).putShort(v.toShort()).order(ByteOrder.BIG_ENDIAN).putShort(v.toShort())
            .order(ByteOrder.LITTLE_ENDIAN)
    }

    private fun pad(s: String, n: Int, c: Char = ' ') = s.padEnd(n, c).toByteArray()
}
