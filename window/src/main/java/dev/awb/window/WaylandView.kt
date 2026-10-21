package dev.awb.window

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipDescription
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Point
import android.graphics.Rect
import android.os.RemoteException
import android.view.DragEvent
import android.view.InputDevice
import android.view.MotionEvent
import android.view.PointerIcon
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import kotlin.math.roundToInt

/**
 * The client's pixels and its pointer input. The surface is handed to the
 * compositor, which draws into it from its own process.
 */
@SuppressLint("ViewConstructor")
class WaylandView(context: Context, private val session: IToplevelSession, private val win: Int) :
    SurfaceView(context), SurfaceHolder.Callback {
    var icon: PointerIcon? = null

    init {
        holder.addCallback(this)
        isFocusable = true
        isFocusableInTouchMode = true
    }

    /** Session calls fail only once the compositor died; the activity closes then. */
    private inline fun call(f: () -> Unit) {
        try {
            f()
        } catch (_: RemoteException) {
        }
    }

    override fun surfaceCreated(holder: SurfaceHolder) = call { session.attachSurface(win, holder.surface) }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) =
        call { session.resize(win, width, height) }

    override fun surfaceDestroyed(holder: SurfaceHolder) = call { session.attachSurface(win, null) }

    private fun isMouse(e: MotionEvent) =
        e.isFromSource(InputDevice.SOURCE_MOUSE) || e.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE)

    /** Button state for hover/generic events: only a mouse can have buttons down there. */
    private fun buttons(e: MotionEvent): Int = if (isMouse(e)) e.buttonState else 0

    private fun send(e: MotionEvent, action: Int, buttons: Int = buttons(e)) =
        call { session.pointer(win, action, e.x, e.y, buttons, 0f, 0f, e.eventTime) }

    /** A move or resize in progress: edges, start bounds and pointer (display px). */
    private class Move(val edges: Int, val bounds: Rect, val x: Float, val y: Float, val update: (Rect) -> Unit)

    private var move: Move? = null
    private var pressed = false
    private var rawX = 0f
    private var rawY = 0f

    /**
     * Moves ([edges] 0) or resizes the window with the pointer that is down
     * until it is released, through [update]. Ignored once it is up.
     */
    fun beginMove(edges: Int, bounds: Rect, update: (Rect) -> Unit) {
        if (pressed) move = Move(edges, Rect(bounds), rawX, rawY, update)
    }

    private fun moved(m: Move, e: MotionEvent) {
        val dx = (e.rawX - m.x).roundToInt()
        val dy = (e.rawY - m.y).roundToInt()
        val r = Rect(m.bounds)
        if (m.edges == 0) {
            r.offset(dx, dy)
        } else {
            // xdg_toplevel resize edges.
            val min = (120 * resources.displayMetrics.density).roundToInt()
            if (m.edges and 1 != 0) r.top = minOf(r.top + dy, r.bottom - min)
            if (m.edges and 2 != 0) r.bottom = maxOf(r.bottom + dy, r.top + min)
            if (m.edges and 4 != 0) r.left = minOf(r.left + dx, r.right - min)
            if (m.edges and 8 != 0) r.right = maxOf(r.right + dx, r.left + min)
        }
        m.update(r)
    }

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(e: MotionEvent): Boolean {
        // Touch, stylus and injected mouse events carry no button bits; a
        // pointer that is down counts as the primary button.
        val down = e.actionMasked != MotionEvent.ACTION_UP && e.actionMasked != MotionEvent.ACTION_CANCEL
        pressed = down
        rawX = e.rawX
        rawY = e.rawY
        val m = move
        if (m != null) {
            if (down) {
                moved(m, e)
                return true
            }
            // The release still goes to the app, which pressed.
            move = null
        }
        val state = if (isMouse(e)) e.buttonState else 0
        send(e, 0, if (down && state == 0) 1 else state)
        return true
    }

    override fun onGenericMotionEvent(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_SCROLL -> call {
                session.pointer(
                    win, 1, e.x, e.y, buttons(e),
                    e.getAxisValue(MotionEvent.AXIS_VSCROLL), e.getAxisValue(MotionEvent.AXIS_HSCROLL), e.eventTime,
                )
            }
            // A hover exit also precedes every press; only a real exit leaves.
            MotionEvent.ACTION_HOVER_EXIT ->
                if (e.x < 0 || e.y < 0 || e.x >= width || e.y >= height) send(e, 2)
            else -> send(e, 0)
        }
        return true
    }

    /**
     * Starts Android's drag for a Linux app's drag (while the pointer is
     * down here), so it can cross windows and apps. False if Android refused.
     */
    fun startDrag(text: String?, shadow: Bitmap?): Boolean {
        val mime = if (text != null) ClipDescription.MIMETYPE_TEXT_PLAIN else Window.DRAG_MIME
        val clip = ClipData(ClipDescription(Window.DRAG_LABEL, arrayOf(mime)), ClipData.Item(text.orEmpty()))
        val builder = object : DragShadowBuilder() {
            override fun onProvideShadowMetrics(size: Point, touch: Point) {
                size.set(shadow?.width ?: 1, shadow?.height ?: 1)
                touch.set(0, 0)
            }

            override fun onDrawShadow(canvas: Canvas) {
                shadow?.let { canvas.drawBitmap(it, 0f, 0f, null) }
            }
        }
        return startDragAndDrop(clip, builder, null, DRAG_FLAG_GLOBAL)
    }

    /** Android drags over this window, ours or another app's, go to the compositor. */
    override fun onDragEvent(e: DragEvent): Boolean {
        val desc = e.clipDescription
        val local = desc?.label == Window.DRAG_LABEL
        when (e.action) {
            DragEvent.ACTION_DRAG_STARTED -> return local || desc?.hasMimeType("text/*") == true
            DragEvent.ACTION_DRAG_ENTERED -> return true
            DragEvent.ACTION_DROP -> {
                val text = if (local) null else e.clipData?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()
                drag(e, local, text)
            }
            else -> drag(e, local, null)
        }
        return true
    }

    private fun drag(e: DragEvent, local: Boolean, text: String?) {
        val desc = e.clipDescription
        val mimes = if (desc == null) emptyArray() else Array(desc.mimeTypeCount) { desc.getMimeType(it) }
        call { session.drag(win, e.action, e.x, e.y, local, e.result, mimes, text) }
    }

    /** The focused Linux text field, if the app enabled text input. */
    var textField: TextField? = null
        private set
    private var connection: TextInputConnection? = null
    private val imm get() = context.getSystemService(InputMethodManager::class.java)

    override fun onCheckIsTextEditor() = textField != null

    override fun onCreateInputConnection(out: EditorInfo): InputConnection? {
        val f = textField ?: return null
        f.fill(out)
        return TextInputConnection(this, session, win).also { connection = it }
    }

    /** The compositor's report on the app's text field; null: none is enabled. */
    fun setTextField(field: TextField?) {
        val old = textField
        if (field == old) return
        textField = field
        when {
            field == null -> {
                connection = null
                imm.restartInput(this)
                imm.hideSoftInputFromWindow(windowToken, 0)
            }
            // A new field (or a different kind): a fresh IME session.
            old == null || old.purpose != field.purpose || old.hint != field.hint -> {
                imm.restartInput(this)
                imm.showSoftInput(this, 0)
            }
            else -> connection?.fieldChanged(imm)
        }
    }

    override fun onResolvePointerIcon(event: MotionEvent, pointerIndex: Int): PointerIcon? =
        icon ?: super.onResolvePointerIcon(event, pointerIndex)
}
