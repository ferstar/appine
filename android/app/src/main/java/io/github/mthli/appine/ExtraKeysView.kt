package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import android.view.ViewGroup
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.PopupWindow
import android.widget.TextView
import com.google.android.material.color.MaterialColors
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * The keys the soft keyboard lacks: Termux's default two rows with the arrows as an inverted T,
 * widened to eight columns.
 *
 * ```
 * ESC  /   -  HOME  ↑  END  |  │ PGUP
 * TAB CTRL ALT  ←   ↓   →   ~  │ PGDN
 * ```
 *
 * The columns share the width but are never narrower than [MIN_COLUMN_DP], so all eight fit from
 * 360 dp on. In a narrower view the last column stays at the right edge and the first seven scroll
 * sideways beside it. Faint lines one pixel wide put each key in a cell of its own.
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

    /** The first seven columns of both rows, and the keys in them (sized in [onMeasure]). */
    private val grid = object : LinearLayout(context) {
        // Over the keys, so a pressed key's shading does not hide them; drawn here, inside the
        // scroll view, the lines between columns scroll with the keys
        override fun dispatchDraw(canvas: Canvas) {
            super.dispatchDraw(canvas)
            for (i in 0 until childCount) {
                val row = getChildAt(i) as ViewGroup
                hLine(canvas, row.top, width)
                for (j in 1 until row.childCount) vLine(canvas, row.left + row.getChildAt(j).left, row.top, row.bottom)
            }
            hLine(canvas, height - 1, width)
        }
    }.apply { orientation = VERTICAL }
    private val gridKeys = mutableListOf<View>()

    /** The grid, scrolled sideways when the view is too narrow for all eight columns. */
    private val scroll = object : HorizontalScrollView(context) {
        // It would take any sideways slide past the touch slop, cancelling the key under the
        // finger, even with nothing to scroll (360 dp and wider)
        override fun onInterceptTouchEvent(e: MotionEvent) =
            (canScrollHorizontally(1) || canScrollHorizontally(-1)) && super.onInterceptTouchEvent(e)
    }.apply {
        isHorizontalScrollBarEnabled = false
        addView(grid)
    }

    /** The last column, fixed at the right edge. */
    private val fixed = object : LinearLayout(context) {
        // Its left line also ends the grid where it scrolls out of sight
        override fun dispatchDraw(canvas: Canvas) {
            super.dispatchDraw(canvas)
            vLine(canvas, 0, 0, height)
            for (i in 0 until childCount) hLine(canvas, getChildAt(i).top, width)
            hLine(canvas, height - 1, width)
        }
    }.apply { orientation = VERTICAL }

    /** The width of a column in the grid, in pixels, as last measured; 0 before that. */
    private var column = 0

    val ctrl get() = ctrlKey.on
    val alt get() = altKey.on

    private val density = resources.displayMetrics.density
    private val slop = ViewConfiguration.get(context).scaledTouchSlop

    // On black like the terminal and the tab bar; the theme's colours, as the tab bar's, for the
    // labels, a modifier that is on, a key while pressed and the popup above one
    private val labelColor = themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant)
    private val onColor = themeColor(androidx.appcompat.R.attr.colorPrimary)
    private val pressedColor = themeColor(com.google.android.material.R.attr.colorSurfaceContainerHighest)

    /**
     * The lines around the keys: above each row, below the last one (the system bars and what lies
     * under them are black too) and between the columns, in the divider colour at half opacity.
     */
    private val linePaint = Paint().apply {
        color = themeColor(com.google.android.material.R.attr.colorOutlineVariant)
        alpha = 128 // after color, which sets the alpha from the opaque theme colour
    }

    init {
        orientation = HORIZONTAL
        setBackgroundColor(Color.BLACK)
        row(
            special("ESC", KeyEvent.KEYCODE_ESCAPE),
            char("/"),
            char("-", popup = char("|")),
            special("HOME", KeyEvent.KEYCODE_MOVE_HOME),
            special("↑", KeyEvent.KEYCODE_DPAD_UP, repeats = true),
            special("END", KeyEvent.KEYCODE_MOVE_END),
            char("|"),
            special("PGUP", KeyEvent.KEYCODE_PAGE_UP, repeats = true),
        )
        row(
            special("TAB", KeyEvent.KEYCODE_TAB),
            Key("CTRL", modifier = ctrlKey),
            Key("ALT", modifier = altKey),
            special("←", KeyEvent.KEYCODE_DPAD_LEFT, repeats = true),
            special("↓", KeyEvent.KEYCODE_DPAD_DOWN, repeats = true),
            special("→", KeyEvent.KEYCODE_DPAD_RIGHT, repeats = true),
            char("~"),
            special("PGDN", KeyEvent.KEYCODE_PAGE_DOWN, repeats = true),
        )
        addView(scroll, LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f))
        // Its width is set in onMeasure
        addView(fixed, LayoutParams(0, LayoutParams.WRAP_CONTENT))
        refresh()
    }

    /**
     * Shares the width out among the [COLUMNS], each at least [MIN_COLUMN_DP] wide; the last
     * column also takes the pixels left over. When the minimum wins, the grid is wider than the
     * room beside the last column and scrolls.
     */
    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val width = MeasureSpec.getSize(widthMeasureSpec)
        val min = (MIN_COLUMN_DP * density).roundToInt()
        val c = max(min, width / COLUMNS)
        if (c != column) {
            column = c
            for (v in gridKeys) v.layoutParams.width = c
            // The rows and the grid are measured without a width limit, which need not change
            // with the columns, so their last measurement could be reused
            grid.forceLayout()
            for (i in 0 until grid.childCount) grid.getChildAt(i).forceLayout()
        }
        fixed.layoutParams.width = max(min, width - (COLUMNS - 1) * c)
        super.onMeasure(widthMeasureSpec, heightMeasureSpec)
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

    /** A line one pixel high from the left edge across [width], its top at [y]. */
    private fun hLine(canvas: Canvas, y: Int, width: Int) =
        canvas.drawRect(0f, y.toFloat(), width.toFloat(), y + 1f, linePaint)

    /** A line one pixel wide from [top] to [bottom], its left at [x]. */
    private fun vLine(canvas: Canvas, x: Int, top: Int, bottom: Int) =
        canvas.drawRect(x.toFloat(), top.toFloat(), x + 1f, bottom.toFloat(), linePaint)

    private fun special(label: String, keyCode: Int, repeats: Boolean = false) =
        Key(label, repeats = repeats) { target.key(keyCode) }

    private fun char(text: String, popup: Key? = null) = Key(text, popup = popup) { target.text(text) }

    /** One row of [COLUMNS] keys: all but the last in the grid, the last in the fixed column. */
    private fun row(vararg keys: Key) {
        check(keys.size == COLUMNS)
        val height = (ROW_DP * density).roundToInt()
        val row = LinearLayout(context).apply { orientation = HORIZONTAL }
        for (key in keys.dropLast(1)) {
            val view = keyView(key)
            gridKeys.add(view)
            // Its width is set in onMeasure
            row.addView(view, LayoutParams(0, LayoutParams.MATCH_PARENT))
        }
        grid.addView(row, LayoutParams(LayoutParams.WRAP_CONTENT, height))
        fixed.addView(keyView(keys.last()), LayoutParams(LayoutParams.MATCH_PARENT, height))
    }

    @SuppressLint("ClickableViewAccessibility") // Press calls performClick on release
    private fun keyView(key: Key) = label(key.label).also { view ->
        key.modifier?.view = view
        view.setOnClickListener { tap(key) }
        view.setOnTouchListener(Press(key, view))
    }

    private fun label(text: String) = TextView(context).apply {
        this.text = text
        gravity = Gravity.CENTER
        setTextColor(labelColor)
        // HOME, the widest label, takes about 34 dp of a 45 dp column
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
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
     * the finger is above the key. A sideways slide on a key that is not yet held or showing its
     * popup scrolls the grid, if it can scroll, and cancels the key.
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
            if (held++ == 0) keepTouch()
            val m = key.modifier
            if (m != null) {
                view.performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
                hold(m)
            } else {
                key.send()
                view.postDelayed(this, REPEAT_MS)
            }
        }

        /** From now on the grid does not take the finger to scroll. */
        private fun keepTouch() = view.parent.requestDisallowInterceptTouchEvent(true)

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
            keepTouch()
            val label = label(popupKey.label).apply { setBackgroundColor(pressedColor) }
            window = PopupWindow(label, view.width, view.height).apply {
                isTouchable = false
                animationStyle = 0
                showAsDropDown(view, 0, -2 * view.height)
            }
        }
    }

    companion object {
        /** The number of columns: seven in the grid and the fixed one. */
        private const val COLUMNS = 8

        /** The narrowest a column gets, so eight fit in 360 dp. */
        private const val MIN_COLUMN_DP = 45f

        /** The height of a row. */
        private const val ROW_DP = 32f

        /** How often a held key repeats after the long-press timeout, as in Termux. */
        private const val REPEAT_MS = 80L
    }
}
