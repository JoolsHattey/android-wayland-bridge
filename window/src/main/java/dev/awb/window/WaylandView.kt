package dev.awb.window

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipDescription
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BlendMode
import android.graphics.Canvas
import android.graphics.HardwareBufferRenderer
import android.graphics.Paint
import android.graphics.Point
import android.graphics.Rect
import android.graphics.RenderNode
import android.hardware.HardwareBuffer
import android.hardware.SyncFence
import android.os.Bundle
import android.os.RemoteException
import android.view.DragEvent
import android.view.InputDevice
import android.view.MotionEvent
import android.view.PointerIcon
import android.view.SurfaceControl
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.math.roundToInt

/** Waits for the present fences of direct-mode frames (see [WaylandView.present]). */
private val presenter = java.util.concurrent.Executors.newSingleThreadExecutor()

/** Copies a synced frame as it is, alpha included (see [WaylandView.copy]). */
private val copyPaint = Paint().apply { blendMode = BlendMode.SRC }

/**
 * The client's pixels and its pointer input. The surface is handed to the
 * compositor, which draws into it from its own process.
 */
@SuppressLint("ViewConstructor")
class WaylandView(context: Context, private val session: IToplevelSession, private val win: Int) :
    SurfaceView(context), SurfaceHolder.Callback2 {
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

    /**
     * The direct layer, above the compositor's swapchain: the client's own
     * buffers, shown as they are (see docs/DESIGN.md, direct mode). Ours, so
     * that a synced frame carries only buffers this process set.
     */
    @Volatile
    private var layer: SurfaceControl? = null

    /** Client buffers by id, so each is imported once. */
    private val buffers = object : LinkedHashMap<Long, HardwareBuffer>(8, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<Long, HardwareBuffer>) =
            (size > 8).also { if (it) eldest.value.close() }
    }

    /** A buffer of ours that synced frames are copied into, and its renderer. */
    private class Copy(val buffer: HardwareBuffer, val renderer: HardwareBufferRenderer) {
        fun close() {
            renderer.close()
            buffer.close()
        }
    }

    /** Two, used in turn, so a copy never goes into the one on screen. UI thread. */
    private val copies = arrayOfNulls<Copy>(2)
    private var nextCopy = 0

    override fun surfaceCreated(holder: SurfaceHolder) {
        layer = SurfaceControl.Builder().setName("awb-direct").setParent(surfaceControl).build().also {
            SurfaceControl.Transaction().setLayer(it, 1).apply()
        }
        call { session.attachSurface(win, holder.surface) }
    }

    // The redraw that follows carries the size: a separate resize would reach
    // the compositor first and be drawn unsynced.
    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        // Returns once the compositor stopped drawing, so no frame follows.
        call { session.attachSurface(win, null) }
        layer?.let {
            SurfaceControl.Transaction().reparent(it, null).apply()
            it.release()
        }
        layer = null
        synchronized(buffers) {
            buffers.values.forEach { it.close() }
            buffers.clear()
        }
        copies.forEachIndexed { i, c ->
            c?.close()
            copies[i] = null
        }
    }

    override fun surfaceRedrawNeeded(holder: SurfaceHolder) {}

    /**
     * Android holds the window's new bounds until [drawingFinished]. Blocks
     * the UI thread (briefly, the compositor times out) for the compositor's
     * frame at the new size and applies it with the window's own frame, so
     * content and bounds change together (see docs/DESIGN.md, synced resizes).
     */
    override fun surfaceRedrawNeededAsync(holder: SurfaceHolder, drawingFinished: Runnable) {
        val f = holder.surfaceFrame
        val frame = try {
            session.redraw(win, f.width(), f.height())
        } catch (_: RemoteException) {
            null
        }
        if (frame != null) {
            // The client's buffer itself must not ride in the synced frame
            // (see [copy]); without a copy the frame goes out on its own.
            val copied = copy(frame)
            val t = transaction(frame)
            if (copied) t?.let { rootSurfaceControl?.applyTransactionOnDraw(it) ?: it.apply() } else t?.apply()
        }
        drawingFinished.run()
    }

    /**
     * Copies [frame]'s client buffer into a buffer of ours and puts that in
     * [frame] instead, with its fence. The client's buffers are wrapped by
     * the compositor around the client's dmabuf (see docs/DESIGN.md, direct
     * mode); one in the window's synced frame sometimes kept the shell's
     * resize synchronizer from ever committing it, which hung SystemUI.
     */
    private fun copy(frame: Bundle): Boolean {
        val client = frame.getParcelable("b", HardwareBuffer::class.java) ?: return false
        val src = frame.getIntArray("src")!!
        val (w, h) = src[2] - src[0] to src[3] - src[1]
        val bitmap = Bitmap.wrapHardwareBuffer(client, null)
        client.close()
        if (bitmap == null || w <= 0 || h <= 0) return false
        val i = nextCopy
        nextCopy = 1 - i
        var c = copies[i]
        if (c == null || c.buffer.width < w || c.buffer.height < h) {
            c?.close()
            // Room to grow, so a drag does not allocate on every step.
            val (bw, bh) = (w + 255) / 256 * 256 to (h + 255) / 256 * 256
            val usage = HardwareBuffer.USAGE_GPU_COLOR_OUTPUT or HardwareBuffer.USAGE_GPU_SAMPLED_IMAGE or
                HardwareBuffer.USAGE_COMPOSER_OVERLAY
            val b = HardwareBuffer.create(bw, bh, HardwareBuffer.RGBA_8888, 1, usage)
            c = Copy(b, HardwareBufferRenderer(b))
            copies[i] = c
        }
        val node = RenderNode("awb-copy").apply { setPosition(0, 0, w, h) }
        node.beginRecording(w, h).drawBitmap(bitmap, Rect(src[0], src[1], src[2], src[3]), Rect(0, 0, w, h), copyPaint)
        node.endRecording()
        c.renderer.setContentRoot(node)
        var fence: SyncFence? = null
        val done = CountDownLatch(1)
        c.renderer.obtainRenderRequest().draw({ it.run() }) { r ->
            if (r.status == HardwareBufferRenderer.RenderResult.SUCCESS) fence = r.fence
            done.countDown()
        }
        if (!done.await(50, TimeUnit.MILLISECONDS) || fence == null) return false
        frame.putParcelable("b", c.buffer)
        frame.putIntArray("src", intArrayOf(0, 0, w, h))
        frame.putParcelable("fence", fence)
        frame.putBoolean("copy", true)
        return true
    }

    /** Shows a direct-mode frame ([IToplevelHost.present]); any thread. */
    fun present(frame: Bundle) {
        transaction(frame)?.apply()
    }

    /**
     * The transaction that shows [frame] on [layer], reporting it to the
     * compositor once on screen; null (and reported) without a layer.
     */
    private fun transaction(frame: Bundle): SurfaceControl.Transaction? {
        val token = frame.getLong("k")
        val sc = layer
        val buffer = frame.getParcelable("b", HardwareBuffer::class.java)
        if (sc == null) {
            buffer?.close()
            call { session.presented(win, token) }
            return null
        }
        val t = SurfaceControl.Transaction()
        if (buffer == null) {
            t.setVisibility(sc, false).setBuffer(sc, null)
        } else {
            val (src, dst) = frame.getIntArray("src")!! to frame.getIntArray("dst")!!
            if (frame.getBoolean("copy")) {
                t.setBuffer(sc, buffer, frame.getParcelable("fence", SyncFence::class.java))
            } else {
                t.setBuffer(sc, cached(buffer))
            }
            t.setGeometry(sc, Rect(src[0], src[1], src[2], src[3]), Rect(dst[0], dst[1], dst[2], dst[3]), 0)
                .setOpaque(sc, frame.getBoolean("opaque"))
                .setVisibility(sc, true)
        }
        t.addTransactionCompletedListener(presenter) { stats ->
            runCatching { stats.presentFence.awaitForever() }
            call { session.presented(win, token) }
        }
        return t
    }

    /** [b], or the copy of the same buffer already imported. */
    private fun cached(b: HardwareBuffer): HardwareBuffer = synchronized(buffers) {
        val old = buffers[b.id]
        if (old != null && !old.isClosed) {
            b.close()
            old
        } else {
            buffers[b.id] = b
            b
        }
    }

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
