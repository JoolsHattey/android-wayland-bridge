package dev.awb

import android.view.KeyEvent

/**
 * Hardware keys arrive with their Linux scan code, which is what Wayland
 * wants. Synthetic events (e.g. `input keyevent`) have none, so map those.
 */
object Keys {
    private val map: Map<Int, Int> by lazy {
        val m = HashMap<Int, Int>()
        val letters = intArrayOf(30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44)
        letters.forEachIndexed { i, code -> m[KeyEvent.KEYCODE_A + i] = code }
        m[KeyEvent.KEYCODE_0] = 11
        for (i in 1..9) m[KeyEvent.KEYCODE_0 + i] = 1 + i
        for (i in 0..9) m[KeyEvent.KEYCODE_F1 + i] = 59 + i
        m[KeyEvent.KEYCODE_F11] = 87
        m[KeyEvent.KEYCODE_F12] = 88
        listOf(
            KeyEvent.KEYCODE_ESCAPE to 1, KeyEvent.KEYCODE_MINUS to 12, KeyEvent.KEYCODE_EQUALS to 13,
            KeyEvent.KEYCODE_DEL to 14, KeyEvent.KEYCODE_TAB to 15, KeyEvent.KEYCODE_LEFT_BRACKET to 26,
            KeyEvent.KEYCODE_RIGHT_BRACKET to 27, KeyEvent.KEYCODE_ENTER to 28, KeyEvent.KEYCODE_CTRL_LEFT to 29,
            KeyEvent.KEYCODE_SEMICOLON to 39, KeyEvent.KEYCODE_APOSTROPHE to 40, KeyEvent.KEYCODE_GRAVE to 41,
            KeyEvent.KEYCODE_SHIFT_LEFT to 42, KeyEvent.KEYCODE_BACKSLASH to 43, KeyEvent.KEYCODE_COMMA to 51,
            KeyEvent.KEYCODE_PERIOD to 52, KeyEvent.KEYCODE_SLASH to 53, KeyEvent.KEYCODE_SHIFT_RIGHT to 54,
            KeyEvent.KEYCODE_ALT_LEFT to 56, KeyEvent.KEYCODE_SPACE to 57, KeyEvent.KEYCODE_CAPS_LOCK to 58,
            KeyEvent.KEYCODE_CTRL_RIGHT to 97, KeyEvent.KEYCODE_ALT_RIGHT to 100, KeyEvent.KEYCODE_MOVE_HOME to 102,
            KeyEvent.KEYCODE_DPAD_UP to 103, KeyEvent.KEYCODE_PAGE_UP to 104, KeyEvent.KEYCODE_DPAD_LEFT to 105,
            KeyEvent.KEYCODE_DPAD_RIGHT to 106, KeyEvent.KEYCODE_MOVE_END to 107, KeyEvent.KEYCODE_DPAD_DOWN to 108,
            KeyEvent.KEYCODE_PAGE_DOWN to 109, KeyEvent.KEYCODE_INSERT to 110, KeyEvent.KEYCODE_FORWARD_DEL to 111,
            KeyEvent.KEYCODE_META_LEFT to 125, KeyEvent.KEYCODE_META_RIGHT to 126,
        ).forEach { (k, v) -> m[k] = v }
        m
    }

    fun evdevFromKeycode(keyCode: Int): Int = map[keyCode] ?: 0
}
