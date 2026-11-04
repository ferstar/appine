package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Color
import android.graphics.Paint
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import android.widget.LinearLayout
import android.widget.PopupWindow
import android.widget.TextView
import com.google.android.material.color.MaterialColors
import kotlin.math.roundToInt

/**
 * The keys the soft keyboard lacks, in Termux's default layout: two rows of seven with the arrows
 * as an inverted T.
 *
 * ```
 * ESC  /  -    HOME  ↑  END  PGUP
 * TAB CTRL ALT  ←    ↓   →   PGDN
 * ```
 *
 * A key acts when the finger lifts, and not at all if the finger left it first. Holding an arrow,
 * PGUP or PGDN repeats it; swiping up from `-` sends `|` instead. Ctrl and Alt are one-shot
 * modifiers: tapped, they apply to the next key typed, which reads them ([ctrl], [alt]) and reports
 * taking them ([modifiersUsed]); held, they stay on until tapped again. The android/terminal map.
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

    /** Ctrl or Alt: on for the next key after a tap, [locked] on after a long press. */
    private class Modifier {
        var on = false
        var locked = false
        lateinit var view: TextView
    }

    /**
     * One key: its label, what it sends, whether holding it repeats, the key a swipe up sends
     * instead, or the modifier it toggles.
     */
    private class Key(
        val label: String,
        val repeats: Boolean = false,
        val popup: Key? = null,
        val modifier: Modifier? = null,
        val send: () -> Unit = {},
    )

    private val ctrlKey = Modifier()
    private val altKey = Modifier()

    val ctrl get() = ctrlKey.on
    val alt get() = altKey.on

    private val density = resources.displayMetrics.density
    private val slop = ViewConfiguration.get(context).scaledTouchSlop

    // The theme's colours, as the tab bar's: the bar, the labels, a modifier that is on, a key
    // while pressed and the popup above one
    private val barColor = themeColor(com.google.android.material.R.attr.colorSurfaceContainer)
    private val labelColor = themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant)
    private val onColor = themeColor(androidx.appcompat.R.attr.colorPrimary)
    private val pressedColor = themeColor(com.google.android.material.R.attr.colorSurfaceContainerHighest)

    init {
        orientation = VERTICAL
        setBackgroundColor(barColor)
        row(
            special("ESC", KeyEvent.KEYCODE_ESCAPE),
            char("/"),
            char("-", popup = char("|")),
            special("HOME", KeyEvent.KEYCODE_MOVE_HOME),
            special("↑", KeyEvent.KEYCODE_DPAD_UP, repeats = true),
            special("END", KeyEvent.KEYCODE_MOVE_END),
            special("PGUP", KeyEvent.KEYCODE_PAGE_UP, repeats = true),
        )
        row(
            special("TAB", KeyEvent.KEYCODE_TAB),
            Key("CTRL", modifier = ctrlKey),
            Key("ALT", modifier = altKey),
            special("←", KeyEvent.KEYCODE_DPAD_LEFT, repeats = true),
            special("↓", KeyEvent.KEYCODE_DPAD_DOWN, repeats = true),
            special("→", KeyEvent.KEYCODE_DPAD_RIGHT, repeats = true),
            special("PGDN", KeyEvent.KEYCODE_PAGE_DOWN, repeats = true),
        )
        refresh()
    }

    /** The next key or character took the one-shot modifiers; locked ones stay on. */
    fun modifiersUsed() {
        var changed = false
        for (m in arrayOf(ctrlKey, altKey)) {
            if (m.on && !m.locked) {
                m.on = false
                changed = true
            }
        }
        if (changed) post { refresh() }
    }

    private fun themeColor(attr: Int) = MaterialColors.getColor(context, attr, Color.WHITE)

    private fun special(label: String, keyCode: Int, repeats: Boolean = false) =
        Key(label, repeats = repeats) { target.key(keyCode) }

    private fun char(text: String, popup: Key? = null) = Key(text, popup = popup) { target.text(text) }

    @SuppressLint("ClickableViewAccessibility") // Press calls performClick on release
    private fun row(vararg keys: Key) {
        val row = LinearLayout(context).apply { orientation = HORIZONTAL }
        for (key in keys) {
            val view = label(key.label)
            key.modifier?.view = view
            view.setOnClickListener { tap(key) }
            view.setOnTouchListener(Press(key, view))
            row.addView(view, LayoutParams(0, LayoutParams.MATCH_PARENT, 1f))
        }
        addView(row, LayoutParams(LayoutParams.MATCH_PARENT, (ROW_DP * density).roundToInt()))
    }

    private fun label(text: String) = TextView(context).apply {
        this.text = text
        gravity = Gravity.CENTER
        setTextColor(labelColor)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        isSingleLine = true
    }

    /** A tap, also what accessibility services perform as a click. */
    private fun tap(key: Key) {
        key.modifier?.let {
            it.on = !it.on
            it.locked = false
            refresh()
        } ?: key.send()
    }

    /** A long press on Ctrl or Alt: locks it on, or turns it off. */
    private fun hold(m: Modifier) {
        m.on = !m.on
        m.locked = m.on
        refresh()
    }

    private fun refresh() {
        for (m in arrayOf(ctrlKey, altKey)) {
            m.view.setTextColor(if (m.on) onColor else labelColor)
            m.view.paintFlags = if (m.locked) {
                m.view.paintFlags or Paint.UNDERLINE_TEXT_FLAG
            } else {
                m.view.paintFlags and Paint.UNDERLINE_TEXT_FLAG.inv()
            }
        }
    }

    /**
     * The touch on one key: highlights it while pressed, taps it on release (through
     * [View.performClick]), repeats it or locks its modifier while held, and shows its popup while
     * the finger is above the key.
     */
    private inner class Press(private val key: Key, private val view: View) : OnTouchListener, Runnable {
        private var held = 0
        private var cancelled = false
        private var window: PopupWindow? = null

        override fun onTouch(v: View, e: MotionEvent): Boolean {
            when (e.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    held = 0
                    cancelled = false
                    v.setBackgroundColor(pressedColor)
                    if (key.repeats || key.modifier != null) {
                        v.postDelayed(this, ViewConfiguration.getLongPressTimeout().toLong())
                    }
                }
                MotionEvent.ACTION_MOVE -> if (!cancelled) {
                    if (key.popup != null && e.y < 0) {
                        if (window == null) showPopup(key.popup)
                    } else {
                        window?.dismiss()
                        window = null
                        if (outside(v, e)) cancel()
                    }
                }
                MotionEvent.ACTION_UP -> {
                    val shown = window
                    val tapped = !cancelled && held == 0
                    cancel()
                    when {
                        shown != null -> {
                            v.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                            key.popup!!.send()
                        }
                        tapped -> {
                            v.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                            v.performClick()
                        }
                    }
                }
                MotionEvent.ACTION_CANCEL -> cancel()
            }
            return true
        }

        /** The long-press timeout passed, then every [REPEAT_MS] while a repeating key is held. */
        override fun run() {
            held++
            val m = key.modifier
            if (m != null) {
                view.performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
                hold(m)
            } else {
                key.send()
                view.postDelayed(this, REPEAT_MS)
            }
        }

        private fun outside(v: View, e: MotionEvent) =
            e.x < -slop || e.y < -slop || e.x > v.width + slop || e.y > v.height + slop

        private fun cancel() {
            cancelled = true
            view.removeCallbacks(this)
            view.background = null
            window?.dismiss()
            window = null
        }

        /** The popup key, as large as this one, right above it. */
        private fun showPopup(popupKey: Key) {
            view.removeCallbacks(this)
            val label = label(popupKey.label).apply { setBackgroundColor(pressedColor) }
            window = PopupWindow(label, view.width, view.height).apply {
                isTouchable = false
                animationStyle = 0
                showAsDropDown(view, 0, -2 * view.height)
            }
        }
    }

    companion object {
        /** The height of a row, as in Termux. */
        private const val ROW_DP = 37.5f

        /** How often a held key repeats after the long-press timeout, as in Termux. */
        private const val REPEAT_MS = 80L
    }
}
