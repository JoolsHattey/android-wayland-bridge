package dev.awb

import java.io.ByteArrayOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder

/**
 * Android's binary XML, as aapt2 compiles AndroidManifest.xml: read into a
 * tree, edited, written back. Only what a manifest uses (no styled strings).
 *
 * Strings are kept by index: edits append to [strings] (the first ones are
 * the attribute names the resource map gives ids to, so they keep their
 * place), and the pool is written back as UTF-16.
 */
class BinaryXml(bytes: ByteArray) {
    class Namespace(val prefix: Int, val uri: Int, val line: Int)

    class Attr(val ns: Int, val name: Int, var raw: Int, var type: Int, var data: Int)

    class Element(val ns: Int, val name: Int, val line: Int, val attrs: MutableList<Attr>, val idIndex: Int, val classIndex: Int, val styleIndex: Int) {
        val children = mutableListOf<Element>()

        fun copy(): Element = Element(ns, name, line, attrs.map { Attr(it.ns, it.name, it.raw, it.type, it.data) }.toMutableList(), idIndex, classIndex, styleIndex)
            .also { c -> children.mapTo(c.children) { it.copy() } }
    }

    val strings = mutableListOf<String>()
    private var resourceMap = ByteArray(0)
    private val namespaces = mutableListOf<Namespace>()
    lateinit var root: Element
        private set

    init {
        val b = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN)
        require(b.getShort(0).toInt() == RES_XML_TYPE) { "not binary XML" }
        val stack = ArrayDeque<Element>()
        var pos = b.getShort(2).toInt()
        while (pos < bytes.size) {
            val type = b.getShort(pos).toInt() and 0xffff
            val header = b.getShort(pos + 2).toInt()
            val size = b.getInt(pos + 4)
            when (type) {
                RES_STRING_POOL_TYPE -> readStrings(b, pos)
                RES_XML_RESOURCE_MAP_TYPE -> resourceMap = bytes.copyOfRange(pos, pos + size)
                RES_XML_START_NAMESPACE_TYPE -> namespaces += Namespace(b.getInt(pos + header), b.getInt(pos + header + 4), b.getInt(pos + 8))
                RES_XML_END_NAMESPACE_TYPE, RES_XML_CDATA_TYPE -> {}
                RES_XML_START_ELEMENT_TYPE -> {
                    val x = pos + header
                    val attrStart = b.getShort(x + 8).toInt()
                    val attrSize = b.getShort(x + 10).toInt()
                    val attrs = MutableList(b.getShort(x + 12).toInt()) { i ->
                        val a = x + attrStart + i * attrSize
                        Attr(b.getInt(a), b.getInt(a + 4), b.getInt(a + 8), b.get(a + 15).toInt() and 0xff, b.getInt(a + 16))
                    }
                    val e = Element(b.getInt(x), b.getInt(x + 4), b.getInt(pos + 8), attrs, b.getShort(x + 14).toInt(), b.getShort(x + 16).toInt(), b.getShort(x + 18).toInt())
                    if (stack.isEmpty()) root = e else stack.last().children += e
                    stack.addLast(e)
                }
                RES_XML_END_ELEMENT_TYPE -> stack.removeLast()
                else -> error("unknown chunk type 0x${type.toString(16)}")
            }
            pos += size
        }
    }

    private fun readStrings(b: ByteBuffer, pos: Int) {
        val count = b.getInt(pos + 8)
        require(b.getInt(pos + 12) == 0) { "styled strings" }
        val utf8 = b.getInt(pos + 16) and UTF8_FLAG != 0
        val start = pos + b.getInt(pos + 20)
        for (i in 0 until count) {
            var p = start + b.getInt(pos + 28 + i * 4)
            strings += if (utf8) {
                // Length in UTF-16 units, then in bytes; one or two bytes each.
                p += if (b.get(p).toInt() and 0x80 != 0) 2 else 1
                var n = b.get(p).toInt() and 0xff
                if (n and 0x80 != 0) n = (n and 0x7f) shl 8 or (b.get(p + 1).toInt() and 0xff).also { p++ }
                String(b.array(), p + 1, n, Charsets.UTF_8)
            } else {
                var n = b.getShort(p).toInt() and 0xffff
                if (n and 0x8000 != 0) n = (n and 0x7fff) shl 16 or (b.getShort(p + 2).toInt() and 0xffff).also { p += 2 }
                String(b.array(), p + 2, n * 2, Charsets.UTF_16LE)
            }
        }
    }

    /** The index of string [s], added if it is not there yet. */
    fun string(s: String): Int = strings.indexOf(s).takeIf { it >= 0 } ?: strings.size.also { strings += s }

    fun name(e: Element): String = strings[e.name]

    /** [e]'s attribute [name] in the android namespace ([ns] null: no namespace). */
    fun attr(e: Element, name: String, ns: String? = ANDROID): Attr? = e.attrs.firstOrNull { a ->
        strings[a.name] == name && (if (ns == null) a.ns == -1 else a.ns >= 0 && strings[a.ns] == ns)
    }

    /** [a]'s value if it is a string. */
    fun stringValue(a: Attr?): String? = a?.takeIf { it.type == TYPE_STRING }?.let { strings[it.data] }

    /** Makes [a] the string [value] (also when it was a reference or a number). */
    fun setString(a: Attr, value: String) {
        val i = string(value)
        a.raw = i
        a.type = TYPE_STRING
        a.data = i
    }

    /** Every element of the tree, depth first, with its parent. */
    fun walk(f: (parent: Element?, e: Element) -> Unit) {
        fun go(parent: Element?, e: Element) {
            f(parent, e)
            e.children.toList().forEach { go(e, it) }
        }
        go(null, root)
    }

    fun toByteArray(): ByteArray {
        val body = Chunks()
        body.chunk(RES_STRING_POOL_TYPE, 28) {
            val encoded = strings.map { s ->
                val o = Chunks()
                if (s.length > 0x7fff) {
                    o.short(0x8000 or (s.length shr 16))
                    o.short(s.length and 0xffff)
                } else {
                    o.short(s.length)
                }
                o.bytes(s.toByteArray(Charsets.UTF_16LE))
                o.short(0)
                o.out.toByteArray()
            }
            int(strings.size)
            int(0) // styles
            int(0) // flags: UTF-16
            int(28 + 4 * strings.size)
            int(0)
            var offset = 0
            for (e in encoded) {
                int(offset)
                offset += e.size
            }
            encoded.forEach { bytes(it) }
            while (out.size() % 4 != 0) out.write(0)
        }
        body.bytes(resourceMap)
        for (n in namespaces) body.node(RES_XML_START_NAMESPACE_TYPE, n.line) { int(n.prefix); int(n.uri) }
        fun write(e: Element) {
            body.node(RES_XML_START_ELEMENT_TYPE, e.line) {
                int(e.ns)
                int(e.name)
                short(20) // attributeStart
                short(20) // attributeSize
                short(e.attrs.size)
                short(e.idIndex)
                short(e.classIndex)
                short(e.styleIndex)
                for (a in e.attrs) {
                    int(a.ns)
                    int(a.name)
                    int(a.raw)
                    short(8)
                    out.write(0)
                    out.write(a.type)
                    int(a.data)
                }
            }
            e.children.forEach { write(it) }
            body.node(RES_XML_END_ELEMENT_TYPE, e.line) { int(e.ns); int(e.name) }
        }
        write(root)
        for (n in namespaces.asReversed()) body.node(RES_XML_END_NAMESPACE_TYPE, n.line) { int(n.prefix); int(n.uri) }
        return Chunks().apply { chunk(RES_XML_TYPE, 8) { bytes(body.out.toByteArray()) } }.out.toByteArray()
    }

    /** Little-endian chunk writer. */
    private class Chunks {
        val out = ByteArrayOutputStream()

        fun short(v: Int) {
            out.write(v and 0xff)
            out.write(v shr 8 and 0xff)
        }

        fun int(v: Int) {
            short(v and 0xffff)
            short(v ushr 16)
        }

        fun bytes(b: ByteArray) = out.write(b)

        /** A chunk: type, header size, total size, then what [f] writes. */
        fun chunk(type: Int, headerSize: Int, f: Chunks.() -> Unit) {
            val c = Chunks().apply(f).out.toByteArray()
            short(type)
            short(headerSize)
            int(8 + c.size)
            bytes(c)
        }

        /** An XML node chunk: line number and (no) comment first. */
        fun node(type: Int, line: Int, f: Chunks.() -> Unit) = chunk(type, 16) {
            int(line)
            int(-1)
            f()
        }
    }

    companion object {
        const val ANDROID = "http://schemas.android.com/apk/res/android"
        const val TYPE_STRING = 0x03
        private const val RES_STRING_POOL_TYPE = 0x0001
        private const val RES_XML_TYPE = 0x0003
        private const val RES_XML_START_NAMESPACE_TYPE = 0x0100
        private const val RES_XML_END_NAMESPACE_TYPE = 0x0101
        private const val RES_XML_START_ELEMENT_TYPE = 0x0102
        private const val RES_XML_END_ELEMENT_TYPE = 0x0103
        private const val RES_XML_CDATA_TYPE = 0x0104
        private const val RES_XML_RESOURCE_MAP_TYPE = 0x0180
        private const val UTF8_FLAG = 1 shl 8
    }
}
