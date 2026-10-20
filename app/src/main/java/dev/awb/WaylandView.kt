package dev.awb

import android.annotation.SuppressLint
import android.content.Context
import android.view.InputDevice
import android.view.MotionEvent
import android.view.PointerIcon
import android.view.SurfaceHolder
import android.view.SurfaceView

/** The client's pixels (drawn natively into this surface) and its pointer input. */
@SuppressLint("ViewConstructor")
class WaylandView(context: Context, private val win: Int) : SurfaceView(context), SurfaceHolder.Callback {
    var icon: PointerIcon? = null

    init {
        holder.addCallback(this)
        isFocusable = true
        isFocusableInTouchMode = true
    }

    override fun surfaceCreated(holder: SurfaceHolder) = Native.attachSurface(win, holder.surface)

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) =
        Native.resize(win, width, height)

    override fun surfaceDestroyed(holder: SurfaceHolder) = Native.attachSurface(win, null)

    private fun isMouse(e: MotionEvent) =
        e.isFromSource(InputDevice.SOURCE_MOUSE) || e.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE)

    /** Button state for hover/generic events: only a mouse can have buttons down there. */
    private fun buttons(e: MotionEvent): Int = if (isMouse(e)) e.buttonState else 0

    private fun send(e: MotionEvent, action: Int, buttons: Int = buttons(e)) =
        Native.pointer(win, action, e.x, e.y, buttons, 0f, 0f, e.eventTime)

    @SuppressLint("ClickableViewAccessibility")
    override fun onTouchEvent(e: MotionEvent): Boolean {
        // Touch, stylus and injected mouse events carry no button bits; a
        // pointer that is down counts as the primary button.
        val down = e.actionMasked != MotionEvent.ACTION_UP && e.actionMasked != MotionEvent.ACTION_CANCEL
        val state = if (isMouse(e)) e.buttonState else 0
        send(e, 0, if (down && state == 0) 1 else state)
        return true
    }

    override fun onGenericMotionEvent(e: MotionEvent): Boolean {
        when (e.actionMasked) {
            MotionEvent.ACTION_SCROLL -> Native.pointer(
                win, 1, e.x, e.y, buttons(e),
                e.getAxisValue(MotionEvent.AXIS_VSCROLL), e.getAxisValue(MotionEvent.AXIS_HSCROLL), e.eventTime,
            )
            // A hover exit also precedes every press; only a real exit leaves.
            MotionEvent.ACTION_HOVER_EXIT ->
                if (e.x < 0 || e.y < 0 || e.x >= width || e.y >= height) send(e, 2)
            else -> send(e, 0)
        }
        return true
    }

    override fun onResolvePointerIcon(event: MotionEvent, pointerIndex: Int): PointerIcon? =
        icon ?: super.onResolvePointerIcon(event, pointerIndex)
}
