package dev.awb

import android.graphics.Rect
import java.io.File

/**
 * Root side of interactive moves and resizes (xdg_toplevel.move/resize).
 * Apps can't move their own tasks, so awb-launcher.sh runs this as root
 * with app_process: it reads "taskId left top right bottom" lines from the
 * app's `task` fifo and resizes those tasks through ActivityTaskManager.
 */
object TaskHelper {
    @JvmStatic
    fun main(args: Array<String>) {
        val fifo = File(args.getOrNull(0) ?: error("usage: TaskHelper <fifo>"))
        val atm = Class.forName("android.app.ActivityTaskManager").getMethod("getService").invoke(null)!!
        val resize = atm.javaClass.getMethod("resizeTask", Int::class.javaPrimitiveType, Rect::class.java, Int::class.javaPrimitiveType)
        // As a user's drag: keeps the window (no relaunch).
        val mode = runCatching { Class.forName("android.app.ActivityTaskManager").getField("RESIZE_MODE_USER").getInt(null) }.getOrDefault(1)
        while (true) {
            if (!fifo.exists()) {
                Thread.sleep(2000)
                continue
            }
            // Open blocks until the app opens its end; EOF when it closes.
            fifo.bufferedReader().use { reader ->
                while (true) {
                    var line = reader.readLine() ?: break
                    // Only the latest position matters.
                    while (reader.ready()) line = reader.readLine() ?: break
                    val v = line.split(' ').mapNotNull { it.toIntOrNull() }
                    if (v.size != 5) continue
                    try {
                        resize.invoke(atm, v[0], Rect(v[1], v[2], v[3], v[4]), mode)
                    } catch (e: Exception) {
                        System.err.println("resizeTask ${v[0]}: ${e.cause ?: e}")
                    }
                }
            }
        }
    }
}
