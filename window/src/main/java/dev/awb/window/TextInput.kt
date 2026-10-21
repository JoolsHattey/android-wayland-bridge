package dev.awb.window

import android.graphics.Matrix
import android.os.RemoteException
import android.text.InputType
import android.text.TextUtils
import android.view.KeyEvent
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.CursorAnchorInfo
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager

/**
 * The focused Linux text field (zwp_text_input_v3), as the compositor
 * reports it. [text] is the surrounding text with [cursor] and [anchor] in
 * UTF-16 units (null when the app sends none); [rect] is the cursor in view
 * px (x, y, w, h).
 */
data class TextField(
    val purpose: Int,
    val hint: Int,
    val text: String?,
    val cursor: Int,
    val anchor: Int,
    val rect: IntArray?,
) {
    // Content types from text-input-unstable-v3.
    private fun has(h: Int) = hint and h != 0

    fun fill(out: EditorInfo) {
        out.inputType = when (purpose) {
            DIGITS, NUMBER -> InputType.TYPE_CLASS_NUMBER or
                (if (purpose == NUMBER) InputType.TYPE_NUMBER_FLAG_DECIMAL or InputType.TYPE_NUMBER_FLAG_SIGNED else 0)
            PHONE -> InputType.TYPE_CLASS_PHONE
            PIN -> InputType.TYPE_CLASS_NUMBER or InputType.TYPE_NUMBER_VARIATION_PASSWORD
            DATE -> InputType.TYPE_CLASS_DATETIME or InputType.TYPE_DATETIME_VARIATION_DATE
            TIME -> InputType.TYPE_CLASS_DATETIME or InputType.TYPE_DATETIME_VARIATION_TIME
            DATETIME -> InputType.TYPE_CLASS_DATETIME
            else -> InputType.TYPE_CLASS_TEXT or when {
                purpose == PASSWORD || has(HIDDEN_TEXT) -> InputType.TYPE_TEXT_VARIATION_PASSWORD
                purpose == URL -> InputType.TYPE_TEXT_VARIATION_URI
                purpose == EMAIL -> InputType.TYPE_TEXT_VARIATION_EMAIL_ADDRESS
                purpose == NAME -> InputType.TYPE_TEXT_VARIATION_PERSON_NAME
                // A terminal gets keys as typed: no composing or suggestions.
                purpose == TERMINAL -> InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
                else -> 0
            } or (if (has(MULTILINE)) InputType.TYPE_TEXT_FLAG_MULTI_LINE else 0) or
                (if (has(AUTO_CAPITALIZATION)) InputType.TYPE_TEXT_FLAG_CAP_SENTENCES else 0) or
                (if (has(SPELLCHECK)) InputType.TYPE_TEXT_FLAG_AUTO_CORRECT else 0) or
                (if (has(COMPLETION)) InputType.TYPE_TEXT_FLAG_AUTO_COMPLETE else 0) or
                (if (has(SENSITIVE_DATA)) InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS else 0)
        }
        out.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            (if (has(MULTILINE)) EditorInfo.IME_ACTION_NONE else EditorInfo.IME_ACTION_DONE) or
            (if (has(SENSITIVE_DATA) || purpose == PASSWORD || purpose == PIN) EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING else 0)
        if (text != null) {
            out.initialSelStart = minOf(cursor, anchor)
            out.initialSelEnd = maxOf(cursor, anchor)
            out.setInitialSurroundingText(text)
        }
    }

    override fun equals(other: Any?) = other is TextField && purpose == other.purpose && hint == other.hint &&
        text == other.text && cursor == other.cursor && anchor == other.anchor && rect.contentEquals(other.rect)

    override fun hashCode() = arrayOf(purpose, hint, text, cursor, anchor, rect?.contentHashCode()).contentHashCode()

    companion object {
        const val NAME = 7
        const val DIGITS = 2
        const val NUMBER = 3
        const val PHONE = 4
        const val URL = 5
        const val EMAIL = 6
        const val PASSWORD = 8
        const val PIN = 9
        const val DATE = 10
        const val TIME = 11
        const val DATETIME = 12
        const val TERMINAL = 13
        const val COMPLETION = 0x1
        const val SPELLCHECK = 0x2
        const val AUTO_CAPITALIZATION = 0x4
        const val HIDDEN_TEXT = 0x40
        const val SENSITIVE_DATA = 0x80
        const val MULTILINE = 0x200
    }
}

/**
 * The IME's side of a Linux text field. The IME sees a virtual editor: the
 * app's surrounding text with the composing text at the cursor (the app
 * leaves composing text out). Edits go to the compositor, which turns them
 * into text-input events; the app's next surrounding text reports back.
 */
class TextInputConnection(private val view: WaylandView, private val session: IToplevelSession, private val win: Int) :
    BaseInputConnection(view, false) {
    /** The IME's composing text, shown by the app as preedit. */
    var composing = ""
        private set
    private var cursorUpdates = 0

    private val field get() = view.textField
    private val before get() = field?.text?.let { it.substring(0, minOf(it.length, field!!.cursor.coerceAtLeast(0))) }.orEmpty()
    private val after get() = field?.text?.let { it.substring(minOf(it.length, field!!.cursor.coerceAtLeast(0))) }.orEmpty()

    /** kind/a/b as in IToplevelSession.ime. */
    private fun send(kind: Int, text: String?, a: Int = 0, b: Int = 0): Boolean {
        try {
            session.ime(win, kind, text, a, b)
        } catch (_: RemoteException) {
        }
        return true
    }

    override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
        composing = ""
        return send(0, text.toString())
    }

    override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
        composing = text.toString()
        return send(1, composing, composing.length)
    }

    override fun finishComposingText(): Boolean {
        if (composing.isEmpty()) return true
        val text = composing
        composing = ""
        return send(0, text)
    }

    /** Only a region that ends at the cursor can become composing text again. */
    override fun setComposingRegion(start: Int, end: Int): Boolean {
        val cursor = before.length
        if (composing.isNotEmpty() || end != cursor || start !in 0 until end) return false
        composing = before.substring(start)
        return send(3, composing, end - start)
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean =
        send(2, null, beforeLength.coerceAtLeast(0), afterLength.coerceAtLeast(0))

    override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean {
        val b = before.let { s -> s.length - s.offsetByCodePoints(s.length, -minOf(beforeLength, s.codePointCount(0, s.length))) }
        val a = after.let { s -> s.offsetByCodePoints(0, minOf(afterLength, s.codePointCount(0, s.length))) }
        return deleteSurroundingText(b, a)
    }

    override fun getTextBeforeCursor(n: Int, flags: Int): CharSequence = (before + composing).takeLast(n.coerceAtLeast(0))

    override fun getTextAfterCursor(n: Int, flags: Int): CharSequence = after.take(n.coerceAtLeast(0))

    override fun getSelectedText(flags: Int): CharSequence? {
        val f = field ?: return null
        val text = f.text ?: return null
        val (s, e) = minOf(f.cursor, f.anchor) to maxOf(f.cursor, f.anchor)
        return if (s < e && e <= text.length) text.substring(s, e) else null
    }

    override fun getCursorCapsMode(reqModes: Int): Int {
        val text = before + composing
        return TextUtils.getCapsMode(text, text.length, reqModes)
    }

    /** Cursor moves become arrow keys: the protocol cannot place the cursor. */
    override fun setSelection(start: Int, end: Int): Boolean {
        if (start != end || composing.isNotEmpty()) return false
        val delta = start - before.length
        val key = if (delta < 0) KeyEvent.KEYCODE_DPAD_LEFT else KeyEvent.KEYCODE_DPAD_RIGHT
        repeat(minOf(kotlin.math.abs(delta), 256)) {
            sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, key))
            sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, key))
        }
        return true
    }

    override fun performEditorAction(editorAction: Int): Boolean {
        finishComposingText()
        sendKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER))
        sendKeyEvent(KeyEvent(KeyEvent.ACTION_UP, KeyEvent.KEYCODE_ENTER))
        return true
    }

    override fun requestCursorUpdates(cursorUpdateMode: Int): Boolean {
        cursorUpdates = cursorUpdateMode
        if (cursorUpdateMode and InputConnection.CURSOR_UPDATE_IMMEDIATE != 0) sendCursor()
        return true
    }

    /** Called when the field changed: the IME learns the new selection and cursor. */
    fun fieldChanged(imm: InputMethodManager) {
        val f = field ?: return
        if (f.text != null) {
            val sel = minOf(f.cursor, f.anchor) + composing.length
            val end = maxOf(f.cursor, f.anchor) + composing.length
            val (cs, ce) = if (composing.isEmpty()) -1 to -1 else f.cursor to f.cursor + composing.length
            imm.updateSelection(view, sel, end, cs, ce)
        }
        if (cursorUpdates and InputConnection.CURSOR_UPDATE_MONITOR != 0) sendCursor()
    }

    private fun sendCursor() {
        val r = field?.rect ?: return
        val location = IntArray(2).also { view.getLocationOnScreen(it) }
        val info = CursorAnchorInfo.Builder()
            .setMatrix(Matrix().apply { setTranslate(location[0].toFloat(), location[1].toFloat()) })
            .setInsertionMarkerLocation(r[0].toFloat(), r[1].toFloat(), (r[1] + r[3]).toFloat(), (r[1] + r[3]).toFloat(), CursorAnchorInfo.FLAG_HAS_VISIBLE_REGION)
            .build()
        view.context.getSystemService(InputMethodManager::class.java).updateCursorAnchorInfo(view, info)
    }
}
