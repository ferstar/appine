package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Color
import android.util.TypedValue
import android.view.Gravity
import android.view.KeyEvent
import android.widget.LinearLayout
import android.widget.TextView
import com.termux.terminal.KeyHandler
import com.termux.view.TerminalView

/**
 * The keys the soft keyboard lacks. Ctrl and Alt are one-shot modifiers: pressed once, they apply
 * to the next key typed.
 * Termux's extra keys bar belongs to its GPLv3 main module, so this is a rewrite (the
 * android/terminal map).
 */
@SuppressLint("ViewConstructor") // Created only in code, never in layout files
internal class ExtraKeysView(
    context: Context,
    private val terminal: () -> TerminalView,
) : LinearLayout(context) {

    private var ctrl = false
    private var alt = false
    private val ctrlKey: TextView
    private val altKey: TextView

    init {
        orientation = HORIZONTAL
        setBackgroundColor(MainActivity.BAR_COLOR)
        key("ESC") { send(KeyEvent.KEYCODE_ESCAPE) }
        key("TAB") { send(KeyEvent.KEYCODE_TAB) }
        ctrlKey = key("CTRL") {
            ctrl = !ctrl
            refresh()
        }
        altKey = key("ALT") {
            alt = !alt
            refresh()
        }
        key("-") { type("-") }
        key("/") { type("/") }
        key("|") { type("|") }
        key("←") { send(KeyEvent.KEYCODE_DPAD_LEFT) }
        key("↓") { send(KeyEvent.KEYCODE_DPAD_DOWN) }
        key("↑") { send(KeyEvent.KEYCODE_DPAD_UP) }
        key("→") { send(KeyEvent.KEYCODE_DPAD_RIGHT) }
    }

    /** Called when TerminalView reads the modifier: one read resets it. */
    fun consumeCtrl(): Boolean = ctrl.also {
        if (it) {
            ctrl = false
            post { refresh() }
        }
    }

    fun consumeAlt(): Boolean = alt.also {
        if (it) {
            alt = false
            post { refresh() }
        }
    }

    private fun refresh() {
        ctrlKey.setTextColor(if (ctrl) Color.CYAN else Color.LTGRAY)
        altKey.setTextColor(if (alt) Color.CYAN else Color.LTGRAY)
    }

    private fun key(label: String, onClick: () -> Unit): TextView {
        val view = TextView(context).apply {
            text = label
            gravity = Gravity.CENTER
            setTextColor(Color.LTGRAY)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            val pad = (10 * resources.displayMetrics.density).toInt()
            setPadding(0, pad, 0, pad)
            setOnClickListener { onClick() }
        }
        addView(view, LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f))
        return view
    }

    /**
     * Special keys: they go through TerminalView, so modifiers and the terminal's key modes
     * (application cursor keys, say) take effect.
     */
    private fun send(keyCode: Int) {
        var mod = 0
        if (consumeCtrl()) mod = mod or KeyHandler.KEYMOD_CTRL
        if (consumeAlt()) mod = mod or KeyHandler.KEYMOD_ALT
        terminal().handleKeyCode(keyCode, mod)
    }

    /** Plain characters: Ctrl and Alt are read by TerminalView ([consumeCtrl], [consumeAlt]). */
    private fun type(text: String) {
        val view = terminal()
        for (c in text) view.inputCodePoint(KeyEvent.KEYCODE_UNKNOWN, c.code, false, false)
    }
}
