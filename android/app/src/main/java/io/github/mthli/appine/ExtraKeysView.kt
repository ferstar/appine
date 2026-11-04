package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Color
import android.util.TypedValue
import android.view.Gravity
import android.view.KeyEvent
import android.widget.LinearLayout
import android.widget.TextView

/**
 * The keys the soft keyboard lacks. Ctrl and Alt are one-shot modifiers: pressed once, they apply
 * to the next key typed, which reads them ([consumeCtrl], [consumeAlt], or [ctrl], [alt] and
 * [modifiersUsed]).
 * Termux's extra keys bar belongs to its GPLv3 main module, so this is a rewrite (the
 * android/terminal map).
 */
@SuppressLint("ViewConstructor") // Created only in code, never in layout files
internal class ExtraKeysView(context: Context, private val target: Target) : LinearLayout(context) {

    /** Where the keys go: the terminal view of the tab shown. */
    interface Target {
        /** A special key (Esc, Tab, arrows); the one-shot modifiers are still to be read. */
        fun key(keyCode: Int)

        /** Plain characters; the one-shot modifiers are still to be read. */
        fun text(text: String)
    }

    var ctrl = false
        private set
    var alt = false
        private set
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

    /** The next key or character took both one-shot modifiers. */
    fun modifiersUsed() {
        if (!ctrl && !alt) return
        ctrl = false
        alt = false
        post { refresh() }
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

    private fun send(keyCode: Int) = target.key(keyCode)

    private fun type(text: String) = target.text(text)
}
