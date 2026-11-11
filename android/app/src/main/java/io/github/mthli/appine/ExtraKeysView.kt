package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Rect
import android.graphics.RectF
import android.graphics.drawable.InsetDrawable
import android.util.TypedValue
import android.view.Gravity
import android.view.HapticFeedbackConstants
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.ViewConfiguration
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.core.graphics.drawable.toDrawable
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * The keys the soft keyboard lacks: Termux's default two rows with the arrows as an inverted T,
 * widened to eight columns, with Shift and SYM in the last. Until it is tapped again, SYM puts
 * symbols in place of the keys that are not, so that page has every ASCII symbol Gboard types
 * neither directly nor from a long press on a letter. SYM is under Shift as the symbol key of a
 * phone keyboard is and fn on a laptop, and on the edge next to the IME, where a stray tap turns
 * the whole page rather than silently shifting the IME's next letter.
 *
 * ```
 * ESC  /   -  HOME  ↑  END  |  │ SHIFT
 * TAB CTRL ALT  ←   ↓   →   ~  │ SYM
 *
 *  `   /   -   =   ^   \   |   │  %
 *  [   ]   {   }   <   >   ~   │ SYM
 * ```
 *
 * The columns share the width but are never narrower than [MIN_COLUMN_DP], so all eight fit from
 * 360 dp on. In a narrower view the last column stays at the right edge and the first seven scroll
 * sideways under it, fading out into the blank left of SHIFT while more of them lie beyond, as the
 * tab row fades out into ×.
 *
 * A key acts when the finger lifts, and not at all if the finger left it first. Holding an arrow
 * repeats it. Ctrl, Alt and Shift are one-shot modifiers: tapped, they apply to the next key typed,
 * which reads them ([ctrl], [alt], [shift]) and reports taking them ([modifiersUsed]); held, they
 * stay on until tapped again. SYM is no modifier: turning the page leaves them as they are, and
 * Ctrl and Alt apply to a symbol too, while Shift, not shown with the symbols, waits for the first
 * page. The android/terminal map.
 */
@SuppressLint("ViewConstructor") // Created only in code, never in layout files
internal class ExtraKeysView(context: Context, private val target: Target) : FrameLayout(context) {

    /** Where the keys go: the terminal view of the tab shown. */
    interface Target {
        /** A special key (Esc, Tab, arrows); the one-shot modifiers are still to be read. */
        fun key(keyCode: Int)

        /** Plain characters; the one-shot modifiers are still to be read. */
        fun text(text: String)

        /** A short message about the keys, such as which modifier a long press locked. */
        fun notice(text: String)
    }

    /** Ctrl, Alt or Shift: on for the next key after a tap, [locked] on after a long press. */
    private class Modifier {
        var on = false
        var locked = false
        lateinit var view: TextView
    }

    /**
     * One key: its label, what it sends, whether holding it repeats, or the modifier it toggles.
     */
    private class Key(
        val label: String,
        val repeats: Boolean = false,
        val modifier: Modifier? = null,
        val send: () -> Unit = {},
    )

    /**
     * A page: the [grid] of its first seven columns, and the [last] key, above SYM. Only the page
     * shown is in the view.
     */
    private class Page(val grid: LinearLayout, val last: View)

    private val ctrlKey = Modifier()
    private val altKey = Modifier()
    private val shiftKey = Modifier()
    private val modifiers = arrayOf(ctrlKey, altKey, shiftKey)

    /** The keys, then the symbols SYM shows. */
    private val pages = mutableListOf<Page>()

    /** The keys in the pages' grids (sized in [onMeasure]). */
    private val gridKeys = mutableListOf<View>()

    /** Whether SYM shows the symbols. */
    private var symbols = false
    private lateinit var symView: TextView

    private val density = resources.displayMetrics.density

    /**
     * How far the fade reaches into the last column, in whole pixels, rounded down as
     * TabBarView's dp.
     */
    private val fadeIntoLast = (FADE_INTO_LAST_DP * density).toInt()

    /**
     * The page shown, scrolled sideways when the view is too narrow for all eight columns. It runs
     * under the last column to the view's end; its end padding, as wide as that column, lets the
     * grid's last column scroll out from under it. While keys lie beyond the padding, the right
     * fading edge hints that they scroll: a padding offset carries it into the blank left of SHIFT,
     * as the tab row's into ×.
     */
    private val scroll = object : HorizontalScrollView(context) {
        // It would take any sideways slide past the touch slop, cancelling the key under the
        // finger, even with nothing to scroll (360 dp and wider)
        override fun onInterceptTouchEvent(e: MotionEvent) = scrolls() && super.onInterceptTouchEvent(e)

        override fun getLeftFadingEdgeStrength() = 0f
        override fun isPaddingOffsetRequired() = true
        override fun getRightPaddingOffset() = fadeIntoLast

        // With gesture navigation, a slide from the screen's left edge over keys that scroll
        // scrolls them instead of going back; with nothing to scroll, the edge keeps its gesture.
        // Reused: the framework copies the list and reads the rectangle when it updates, after
        // each set
        private val excluded = Rect()
        private val exclusion = listOf(excluded)

        override fun onLayout(changed: Boolean, l: Int, t: Int, r: Int, b: Int) {
            super.onLayout(changed, l, t, r, b)
            // The part beside the last column: under it, the right edge keeps its gesture
            excluded.set(0, 0, width - paddingRight, height)
            systemGestureExclusionRects = if (scrolls()) exclusion else emptyList()
        }

        // Not canScrollHorizontally, which counts the end padding as shown, so from 315 dp on it
        // would find nothing to scroll
        private fun scrolls() = getChildAt(0).let { it != null && it.width > width - paddingLeft - paddingRight }
    }.apply {
        isHorizontalScrollBarEnabled = false
        isHorizontalFadingEdgeEnabled = true
        setFadingEdgeLength((FADE_DP * density).toInt())
        clipToPadding = false
    }

    /**
     * The last column, fixed at the right edge over the grid's end. Black but for its left part the
     * fade reaches into, so the keys under it are hidden; its keys fill it and take every touch on
     * it, a key showing through the fade too.
     */
    private val fixed = object : LinearLayout(context) {
        override fun dispatchDraw(canvas: Canvas) {
            super.dispatchDraw(canvas)
            indicateKeys(canvas, this)
        }
    }.apply {
        orientation = LinearLayout.VERTICAL
        background = InsetDrawable(Color.BLACK.toDrawable(), fadeIntoLast, 0, 0, 0)
        // The inset comes as padding with the background, which would narrow the keys and move
        // them right by as much
        setPadding(0, 0, 0, 0)
    }

    /** The width of a column in the grid, in pixels, as last measured; 0 before that. */
    private var column = 0

    private val slop = ViewConfiguration.get(context).scaledTouchSlop

    // On black like the terminal and the tab bar; the theme's colours, as the tab bar's, for the
    // labels, a key that is on and its indicator, and a key while pressed
    private val labelColor = themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant)
    private val onColor = themeColor(androidx.appcompat.R.attr.colorPrimary)
    private val indicatorPaint = indicatorPaint(onColor)
    private val pressedColor = themeColor(com.google.android.material.R.attr.colorSurfaceContainerHighest)

    /** The height of a row, in pixels. */
    private val rowHeight = (ROW_DP * density).roundToInt()

    init {
        setBackgroundColor(Color.BLACK)
        page(
            arrayOf(
                special("ESC", KeyEvent.KEYCODE_ESCAPE),
                char("/"),
                char("-"),
                special("HOME", KeyEvent.KEYCODE_MOVE_HOME),
                special("↑", KeyEvent.KEYCODE_DPAD_UP, repeats = true),
                special("END", KeyEvent.KEYCODE_MOVE_END),
                char("|"),
            ),
            arrayOf(
                special("TAB", KeyEvent.KEYCODE_TAB),
                Key("CTRL", modifier = ctrlKey),
                Key("ALT", modifier = altKey),
                special("←", KeyEvent.KEYCODE_DPAD_LEFT, repeats = true),
                special("↓", KeyEvent.KEYCODE_DPAD_DOWN, repeats = true),
                special("→", KeyEvent.KEYCODE_DPAD_RIGHT, repeats = true),
                char("~"),
            ),
            last = Key("SHIFT", modifier = shiftKey),
        )
        // The ASCII symbols that Gboard's letters do not give from a long press (as they give
        // $ & ; : and the like), and -: / - | ~ where they are on the first page, ` in the corner
        // it has on a keyboard, - = and \ | side by side as there, and ^ in place of ↑
        page(chars("`/-=^\\|"), chars("[]{}<>~"), last = char("%"))
        symView = fixedView(Key("SYM") { turnPage() })
        fixed.addView(symView)
        show(pages[0])
        // Their widths are set in onMeasure: the scroll view's end padding and the last column's
        addView(scroll, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT))
        addView(fixed, LayoutParams(0, LayoutParams.WRAP_CONTENT, Gravity.END))
        refresh()
    }

    private fun special(label: String, keyCode: Int, repeats: Boolean = false) =
        Key(label, repeats = repeats) { target.key(keyCode) }

    private fun char(text: String) = Key(text) { target.text(text) }

    /** A key for each character of [text]. */
    private fun chars(text: String) = Array(text.length) { char(text[it].toString()) }

    /** A page: a row of the grid for each of [rows], of [COLUMNS] − 1 keys each, and [last]. */
    private fun page(vararg rows: Array<Key>, last: Key) {
        val grid = object : LinearLayout(context) {
            // Drawn here, inside the scroll view, the indicators scroll with the keys
            override fun dispatchDraw(canvas: Canvas) {
                super.dispatchDraw(canvas)
                indicateKeys(canvas, this)
            }
        }.apply { orientation = LinearLayout.VERTICAL }
        for (keys in rows) {
            check(keys.size == COLUMNS - 1)
            val row = LinearLayout(context).apply { orientation = LinearLayout.HORIZONTAL }
            for (key in keys) {
                val view = keyView(key)
                gridKeys.add(view)
                // Its width is set in onMeasure
                row.addView(view, LinearLayout.LayoutParams(0, LayoutParams.MATCH_PARENT))
            }
            grid.addView(row, LinearLayout.LayoutParams(LayoutParams.WRAP_CONTENT, rowHeight))
        }
        pages.add(Page(grid, fixedView(last)))
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
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, LABEL_DP)
        isSingleLine = true
    }

    /** A key for the fixed column, laid out to fill a cell of it. */
    private fun fixedView(key: Key) =
        keyView(key).apply { layoutParams = LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, rowHeight) }

    /**
     * Puts [page] in the view in place of the other one, its last key above SYM. Removing a key's
     * view cancels its touch, so a key still held on the page going away stops repeating.
     */
    private fun show(page: Page) {
        scroll.removeAllViews()
        scroll.addView(page.grid)
        if (fixed.childCount > 1) fixed.removeViewAt(0)
        fixed.addView(page.last, 0)
    }

    private fun refresh() {
        symView.setTextColor(if (symbols) onColor else labelColor)
        for (m in modifiers) m.view.setTextColor(if (m.on) onColor else labelColor)
        // Their indicators
        for (page in pages) page.grid.invalidate()
        fixed.invalidate()
    }

    /**
     * Draws the indicator, as under the current tab, under the keys in [container] that stay on
     * until tapped (a locked modifier, and SYM with the symbols shown; a modifier on for the next
     * key only shows the colour), over the keys so that a pressed key's shading does not hide it.
     */
    private fun indicateKeys(canvas: Canvas, container: View) {
        for (m in modifiers) if (m.locked) drawIndicator(canvas, container, m.view, indicatorPaint)
        if (symbols) drawIndicator(canvas, container, symView, indicatorPaint)
    }

    /**
     * Shares the width out among the [COLUMNS], each at least [MIN_COLUMN_DP] wide; the last
     * column also takes the pixels left over. When the minimum wins, the grid is wider than the
     * room beside the last column and scrolls under it.
     */
    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        val width = MeasureSpec.getSize(widthMeasureSpec)
        val min = (MIN_COLUMN_DP * density).roundToInt()
        val c = max(min, width / COLUMNS)
        if (c != column) {
            column = c
            for (v in gridKeys) v.layoutParams.width = c
            // The rows and the grids are measured without a width limit, which need not change
            // with the columns, so their last measurement could be reused; the page not shown
            // keeps the flag until it is shown
            for (page in pages) {
                page.grid.forceLayout()
                for (i in 0 until page.grid.childCount) page.grid.getChildAt(i).forceLayout()
            }
        }
        val last = max(min, width - (COLUMNS - 1) * c)
        fixed.layoutParams.width = last
        scroll.setPadding(0, 0, last, 0)
        super.onMeasure(widthMeasureSpec, heightMeasureSpec)
    }

    /**
     * The touch on one key: highlights it while pressed, taps it on release (through
     * [View.performClick]), and repeats it or locks its modifier while held. A sideways slide on a
     * key that is not yet held scrolls the grid, if it can scroll, and cancels the key.
     */
    private inner class Press(private val key: Key, private val view: View) : OnTouchListener, Runnable {
        private var held = 0
        private var cancelled = false

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
                MotionEvent.ACTION_MOVE -> if (!cancelled && outside(v, e)) cancel()
                MotionEvent.ACTION_UP -> {
                    val tapped = !cancelled && held == 0
                    cancel()
                    if (tapped) {
                        v.performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP)
                        v.performClick()
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
        }
    }

    /** A tap, also what accessibility services perform as a click. */
    private fun tap(key: Key) {
        key.modifier?.let {
            it.on = !it.on
            it.locked = false
            refresh()
        } ?: key.send()
    }

    /**
     * A long press on Ctrl, Alt or Shift: locks it on, also when a tap had turned it on for the
     * next key, saying which one it locked, or turns it off when locked.
     */
    private fun hold(m: Modifier) {
        m.locked = !m.locked
        m.on = m.locked
        refresh()
        if (m.locked) target.notice(context.getString(R.string.modifier_locked, m.view.text))
    }

    /** SYM: shows the symbols in place of the first page, or the first page again. */
    private fun turnPage() {
        symbols = !symbols
        show(pages[if (symbols) 1 else 0])
        refresh()
    }

    val ctrl get() = ctrlKey.on
    val alt get() = altKey.on
    val shift get() = shiftKey.on && !symbols

    /**
     * The next key or character took the one-shot modifiers; locked ones stay on, and so does
     * Shift while it waits with the symbols shown.
     */
    fun modifiersUsed() {
        var changed = false
        for (m in modifiers) {
            if (m.on && !m.locked && !(symbols && m === shiftKey)) {
                m.on = false
                changed = true
            }
        }
        if (changed) post { refresh() }
    }

    companion object {
        /** The number of columns: seven in the grid and the fixed one. */
        private const val COLUMNS = 8

        /** The narrowest a column gets, so eight fit in 360 dp. */
        private const val MIN_COLUMN_DP = 45f

        /** The height of a row. */
        private const val ROW_DP = 32f

        /**
         * The size of the labels; HOME, the widest, takes about 34 dp of a 45 dp column. In dp, not
         * sp, so the system font scale cannot outgrow the fixed cells.
         */
        private const val LABEL_DP = 12f

        /**
         * The width over which a row that scrolls fades out under what is fixed at its end: the
         * keys under the last column, and the tab bar's tabs under its buttons.
         */
        const val FADE_DP = 32

        /**
         * How far the fade reaches into the last column, ending in the blank before SHIFT, its
         * widest label (about 33 dp of a 45 dp column in Roboto); the rest of [FADE_DP] lies at its
         * left.
         */
        private const val FADE_INTO_LAST_DP = 6

        /** How often a held key repeats after the long-press timeout, as in Termux. */
        private const val REPEAT_MS = 80L

        /** The size of the indicator (dp). */
        private const val INDICATOR_WIDTH_DP = 16
        private const val INDICATOR_HEIGHT_DP = 1

        /** How far the indicator lies below its label's baseline (dp). */
        private const val INDICATOR_GAP_DP = 4

        /** The paint for [drawIndicator], in [color]. */
        fun indicatorPaint(color: Int) = Paint(Paint.ANTI_ALIAS_FLAG).apply { this.color = color }

        /**
         * Draws the indicator under [label], which lies in [container] (directly or deeper), for a
         * state that stays until changed: the current tab, a locked modifier, SYM with the symbols
         * shown. As in Material's tabs, a short rounded bar, the same width under any label, and a
         * shape, so it shows without telling colours apart. It hangs [INDICATOR_GAP_DP] below the
         * label's baseline, where its letters and digits end, so it stays with the label whatever
         * the height of the bar or the size of the text; it fades with the label. Nothing is drawn
         * when [label] is not in [container]. [dx] moves it right of the label's place (the tab
         * row slides it there after tabs close).
         */
        fun drawIndicator(canvas: Canvas, container: View, label: TextView, paint: Paint, dx: Float = 0f) {
            val r = indicator
            if (!indicatorBounds(container, label, dx, r)) return
            val alpha = paint.alpha
            paint.alpha = (alpha * label.alpha).roundToInt()
            canvas.drawRoundRect(r, r.height() / 2, r.height() / 2, paint)
            paint.alpha = alpha
        }

        /** [drawIndicator]'s bounds, reused: drawing must not allocate. Main thread only. */
        private val indicator = RectF()

        /**
         * Puts where the indicator under [label] lies in [container] ([drawIndicator]) into
         * [out]; false when [label] is not in [container]. The tab row draws a background tab's
         * progress there too.
         */
        fun indicatorBounds(container: View, label: TextView, dx: Float, out: RectF): Boolean {
            // The label's place in the container: its left and top, not
            // offsetDescendantRectToMyCoords, which subtracts each view's scroll, and a single-line
            // label scrolls its text far sideways to centre it
            var x = 0
            var y = 0
            var v: View = label
            while (v !== container) {
                x += v.left
                y += v.top
                v = v.parent as? View ?: return false
            }
            // In whole pixels, rounded down as TabBarView's dp
            val density = label.resources.displayMetrics.density
            val width = (INDICATOR_WIDTH_DP * density).toInt()
            val height = max(1, (INDICATOR_HEIGHT_DP * density).toInt())
            val left = x + (label.width - width) / 2f + dx
            val top = (y + label.baseline + (INDICATOR_GAP_DP * density).toInt()).toFloat()
            out.set(left, top, left + width, top + height)
            return true
        }
    }
}
