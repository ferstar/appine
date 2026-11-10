package io.github.mthli.appine

import android.animation.ValueAnimator
import android.annotation.SuppressLint
import android.content.Context
import android.graphics.BlurMaskFilter
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.MaskFilter
import android.graphics.Rect
import android.graphics.drawable.Drawable
import android.graphics.drawable.InsetDrawable
import android.util.TypedValue
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.ViewTreeObserver
import android.view.animation.PathInterpolator
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.core.animation.doOnEnd
import androidx.core.graphics.ColorUtils
import androidx.core.graphics.drawable.toDrawable
import com.google.android.material.color.MaterialColors
import kotlin.math.roundToInt

/**
 * The tab bar, one row in any window: a scrolling row of tabs, each a square with its number (a
 * long press shows its title), with `×` (closes the current tab), `+` and `⋮` over its end; the
 * tabs fade out into the left part of `×` while more of them lie beyond. Scrolled away from the
 * current tab, the row brings it back into view a few seconds after it comes to rest. When tabs
 * close, the ones after them slide into their places and then roll to their new numbers.
 *
 * It shows the sessions it is given ([render]) and passes taps on to a [Target]: the sessions,
 * and which of them is current, belong to the activity. The android/terminal map.
 */
@SuppressLint("ViewConstructor") // Created only in code, never in layout files
internal class TabBarView(context: Context, private val target: Target) : FrameLayout(context) {

    /** Where taps on the bar go: the activity, which owns the sessions. */
    interface Target {
        /** A tab was tapped. */
        fun show(session: VtSession)

        /** `×`: the current tab is to close. */
        fun closeCurrent()

        /** `+`. */
        fun newTab()

        /** `⋮`: its menu, at [anchor]. */
        fun menu(anchor: View)

        /** A tab's title, from a long press on it. */
        fun notice(text: String)
    }

    /** The current tab's session, as last rendered; null with none. */
    private var current: VtSession? = null

    /** [current]'s number in the row, which the row draws the indicator under; null with none. */
    private var currentTab: TextView? = null

    /** The row's tabs in its order; a closed one leaves the list as it starts fading out. */
    private val rowTabs = mutableListOf<Tab>()

    /**
     * Where each tab's place in the row was on screen before the change that [slideTabs] animates,
     * or null with none waiting for the next layout. Taken at the first change before that layout:
     * closing the current tab shows the next one, which renders the row once more.
     */
    private var slideFrom: Map<Tab, Int>? = null

    /** Where the current tab's place was on screen, taken with [slideFrom]; null with no tab. */
    private var indicatorFrom: Int? = null

    /** How far right of [currentTab]'s place the indicator is drawn while it slides there. */
    private var indicatorShift = 0f
    private var indicatorSlide: ValueAnimator? = null

    /**
     * Once the row is laid out after a change, slides each tab from where its place was on screen
     * to its new one, then gives it the number of that place ([Tab.renumber]). A closed tab keeps
     * fading out where it was, though the row's scroll moved: a shorter row may scroll less.
     */
    private val slideTabs = object : ViewTreeObserver.OnPreDrawListener {
        override fun onPreDraw(): Boolean {
            row.viewTreeObserver.removeOnPreDrawListener(this)
            val from = slideFrom ?: return true
            slideFrom = null
            for ((tab, x) in from) {
                val shift = x - (tab.view.left - scroll.scrollX)
                tab.view.translationX += shift
                // A tab whose place stays on screen but whose number changes waits as long as
                // those that slide (a 0 to 0 slide); one still sliding renumbers at its end
                if (tab in rowTabs && (shift != 0 || tab.stale && tab.view.translationX == 0f)) tab.slide()
            }
            moveIndicator()
            return true
        }
    }

    /** Whether a finger is on the row, which holds off [returnToCurrentTab]. */
    private var touched = false

    /** Brings the current tab back into view a while after the row was scrolled away from it. */
    private val returnToCurrentTab = Runnable { revealCurrentTab() }

    private val density = resources.displayMetrics.density

    // On black like the terminal; the theme's colours, from the wallpaper on Android 12 and later,
    // as the extra keys'

    /** The buttons and the other tabs' numbers. */
    private val mutedColor = themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant)

    /** The current tab's number and indicator. */
    private val currentColor = themeColor(androidx.appcompat.R.attr.colorPrimary)
    private val indicatorPaint = ExtraKeysView.indicatorPaint(currentColor)

    /** The tabs, with the current one's indicator drawn over them, as the extra keys draw theirs. */
    private val row = object : LinearLayout(context) {
        override fun dispatchDraw(canvas: Canvas) {
            super.dispatchDraw(canvas)
            currentTab?.let { ExtraKeysView.drawIndicator(canvas, this, it, indicatorPaint, indicatorShift) }
        }
    }.apply { orientation = LinearLayout.HORIZONTAL }

    /**
     * The row, scrolling. It runs under the buttons to the bar's end; its end padding, as wide as
     * the buttons, lets the last tab scroll out from under them. While tabs lie beyond the padding,
     * the right fading edge hints that the row scrolls: a padding offset carries it into the blank
     * left part of ×, so the tabs fade out under the button rather than at its edge. Scrolled away
     * from the current tab, the row brings it back once it has rested for a while.
     */
    private val scroll = object : HorizontalScrollView(context) {
        override fun getLeftFadingEdgeStrength() = 0f
        override fun isPaddingOffsetRequired() = true
        override fun getRightPaddingOffset() = dp(FADE_INTO_CLOSE_DP)

        // A finger on the row, on a tab too, holds the return off; lifting it starts the wait
        override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
            when (ev.actionMasked) {
                MotionEvent.ACTION_DOWN -> {
                    touched = true
                    removeCallbacks(returnToCurrentTab)
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    touched = false
                    returnToCurrentTabLater()
                }
            }
            return super.dispatchTouchEvent(ev)
        }

        // Every move without a finger (a fling, TalkBack's scroll actions) starts it again
        override fun onScrollChanged(l: Int, t: Int, oldl: Int, oldt: Int) {
            super.onScrollChanged(l, t, oldl, oldt)
            if (!touched) returnToCurrentTabLater()
        }
    }.apply {
        isHorizontalScrollBarEnabled = false
        isHorizontalFadingEdgeEnabled = true
        setFadingEdgeLength(dp(ExtraKeysView.FADE_DP))
        clipToPadding = false
        // A closed last tab fades out past the end of the shortened row (Tab.leave)
        clipChildren = false
        addView(row, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.MATCH_PARENT))
    }

    /**
     * `×`, hidden while there is no current tab. The multiplication sign, which Roboto draws like
     * its +; U+2715 (✕) is not in Roboto, and the fallback font's is larger and thinner than + and ⋮.
     */
    private val closeButton = button("×") { target.closeCurrent() }.apply {
        contentDescription = context.getString(R.string.close_tab)
    }

    /**
     * `×`, `+` and `⋮` over the row's end. Black but for the left part of × the fade reaches into,
     * so the tabs under the buttons are hidden, and taking every touch there, so a tab under the
     * hidden × cannot be tapped; TalkBack skips the group and reads each button.
     */
    private val buttons = LinearLayout(context).apply {
        orientation = LinearLayout.HORIZONTAL
        background = InsetDrawable(Color.BLACK.toDrawable(), dp(FADE_INTO_CLOSE_DP), 0, 0, 0)
        // The inset comes as padding with the background, which would widen the group to the
        // left by as much and keep the black at the left of ×
        setPadding(0, 0, 0, 0)
        isClickable = true
        importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
        addView(closeButton, item())
        addView(button("+") { target.newTab() }.apply { contentDescription = context.getString(R.string.new_tab) }, item())
        addView(button("⋮") { target.menu(it) }.apply { contentDescription = context.getString(R.string.more_options) }, item())
    }

    // With gesture navigation, a slide from the screen's left edge over the tabs scrolls them
    // instead of going back; the buttons, which do not scroll, leave the right edge's gesture as
    // it is. Reused: the framework copies the list and reads the rectangle when it updates, after
    // each set
    private val excluded = Rect()
    private val exclusion = listOf(excluded)

    init {
        setBackgroundColor(Color.BLACK)
        scroll.setPadding(0, 0, buttons.childCount * dp(BAR_DP), 0)
        addView(scroll, LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT))
        addView(buttons, LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.MATCH_PARENT, Gravity.END))
    }

    /** [BAR_DP] tall, whatever the parent offers. */
    override fun onMeasure(widthMeasureSpec: Int, heightMeasureSpec: Int) {
        super.onMeasure(widthMeasureSpec, MeasureSpec.makeMeasureSpec(dp(BAR_DP), MeasureSpec.EXACTLY))
    }

    override fun onLayout(changed: Boolean, l: Int, t: Int, r: Int, b: Int) {
        super.onLayout(changed, l, t, r, b)
        excluded.set(0, 0, width - scroll.paddingRight, height)
        systemGestureExclusionRects = exclusion
    }

    /**
     * Shows [sessions], a number each, [current] marked (the indicator sliding to it from the last
     * current one, [moveIndicator]) and scrolled into view. Called whenever tabs, titles or the
     * current tab change.
     *
     * Closed tabs shrink and fade out in place, and the tabs after them slide into the places left,
     * keeping their numbers until they are there ([slideTabs]): closing the current tab shows the
     * next one in its place, under the same number, and it is seen arriving.
     */
    fun render(sessions: List<VtSession>, current: VtSession?) {
        this.current = current
        if (slideFrom == null && row.isLaidOut && rowTabs.isNotEmpty()) {
            slideFrom = rowTabs.associateWith { it.view.left - scroll.scrollX }
            indicatorFrom = currentTab?.let { it.left - scroll.scrollX }
            row.viewTreeObserver.addOnPreDrawListener(slideTabs)
        }
        // Out of the list first, so that they renumber no more. Sessions are added only at the end,
        // so the tabs left keep their order
        val closed = rowTabs.filter { it.session !in sessions }
        rowTabs.removeAll(closed)
        closed.forEach { it.leave() }
        sessions.forEachIndexed { i, session ->
            if (i == rowTabs.size) rowTabs.add(Tab(session, i).also { row.addView(it.view, item()) })
            rowTabs[i].update(i)
        }
        closeButton.visibility = if (current == null) View.INVISIBLE else View.VISIBLE

        // Indicate the current tab (the row draws the indicator), and bring it into view once the
        // new row is laid out (this runs after the layout pass that changing the tabs requested).
        // Not through returnToCurrentTab: a scroll in that layout (closing tabs can shorten the row)
        // would put it off
        currentTab = rowTabs.getOrNull(sessions.indexOf(current))?.view
        row.invalidate()
        if (currentTab != null) scroll.post { revealCurrentTab() }
    }

    /**
     * Moves the indicator to the current tab's new place on screen ([slideTabs]), sliding there
     * from where it was drawn, whether another tab was shown or tabs closed; a change while it
     * slides (a held Ctrl+Tab) slides on from where it is. Closing the current tab of a row that
     * does not scroll leaves its place where it was, so the indicator stays while the next tab
     * slides in above it; a row scrolled to its end scrolls back as it shortens, and the indicator
     * slides on to the next tab. With no place to come from (the row's first layout) it is put
     * there at once.
     */
    private fun moveIndicator() {
        val tab = currentTab
        val from = indicatorFrom
        indicatorFrom = null
        val shift = if (tab == null || from == null) null else from - (tab.left - scroll.scrollX)
        when (shift) {
            0 -> {} // the same place: a slide still running goes on
            null -> {
                indicatorSlide?.cancel()
                indicatorShift = 0f
            }
            else -> {
                indicatorSlide?.cancel()
                indicatorSlide = ValueAnimator.ofFloat(indicatorShift + shift, 0f).apply {
                    duration = SLIDE_MS
                    interpolator = STANDARD
                    addUpdateListener {
                        indicatorShift = it.animatedValue as Float
                        row.invalidate()
                    }
                    start()
                }
            }
        }
    }

    /**
     * Scrolls the row to bring the current tab into view, clear of the fade towards the buttons;
     * nothing when it already is, or when there is none.
     */
    private fun revealCurrentTab() {
        val tab = currentTab ?: return
        val fade = scroll.horizontalFadingEdgeLength
        val shownWidth = scroll.width - scroll.paddingRight
        val end = row.width - shownWidth
        // Where the fade begins, from the row's visible left edge
        val clearWidth = shownWidth + dp(FADE_INTO_CLOSE_DP) - fade
        when {
            tab.left < scroll.scrollX -> scroll.smoothScrollTo(tab.left, 0)
            tab.right > scroll.scrollX + clearWidth -> {
                val x = tab.right - clearWidth
                // Less than a fade's width of tabs left beyond the padding would fade only inside
                // ×, leaving the last tab barely faded against it: go to the row's end, where
                // nothing fades
                scroll.smoothScrollTo(if (end - x < fade) end else x, 0)
            }
        }
    }

    /** Starts, or starts again, the wait before [returnToCurrentTab]. */
    private fun returnToCurrentTabLater() {
        scroll.removeCallbacks(returnToCurrentTab)
        scroll.postDelayed(returnToCurrentTab, RETURN_TO_CURRENT_TAB_MS)
    }

    /**
     * A tab in the row: its number (from [i]) in a square as tall as the bar, the current one in
     * the primary colour above the row's indicator ([currentTab]), an ended one faded. Tap shows
     * it; a long press shows its title.
     *
     * The indicator goes by the current tab's place in the row ([moveIndicator]), not by where the
     * tab is while it slides, and a changing number rolls inside the view ([TabNumber]), whose
     * opacity the indicator takes, so it leaves the indicator as it is.
     */
    private inner class Tab(val session: VtSession, i: Int) {
        val view = TabNumber(context).apply {
            text = number(i)
            setTextSize(TypedValue.COMPLEX_UNIT_DIP, TAB_NUMBER_DP)
            gravity = Gravity.CENTER
            foreground = touchFeedback()
            setOnClickListener { target.show(session) }
            setOnLongClickListener {
                target.notice(title(session))
                true
            }
        }

        /**
         * Whether it shows another number than that of its place in the row: it is still on its
         * way there.
         */
        val stale get() = rowTabs.indexOf(this).let { it >= 0 && view.text.toString() != number(it) }

        /**
         * Brings it up to date as the [i]th tab; its number waits while a slide is to be worked
         * out ([slideTabs]), the description (TalkBack) does not.
         */
        fun update(i: Int) {
            view.contentDescription = context.getString(R.string.tab_description, i + 1, title(session))
            view.alpha = if (session.isRunning) 1f else ENDED_ALPHA
            if (slideFrom?.containsKey(this) != true) view.text = number(i)
            view.setTextColor(if (session == current) currentColor else mutedColor)
        }

        /** Slides to its place in the row, then takes the number of that place. */
        fun slide() {
            view.animate().translationX(0f).setDuration(SLIDE_MS).setInterpolator(STANDARD).withEndAction { renumber() }
        }

        /**
         * Rolls its number to the number of its place, when they differ. A change while it rolls
         * is taken at its end.
         */
        fun renumber() {
            if (!view.rolling && stale) view.roll(number(rowTabs.indexOf(this))) { renumber() }
        }

        /**
         * Out of the row, into its overlay, where it shrinks and fades out in its place; a roll
         * under way goes on.
         */
        fun leave() {
            // A slide stops where it is
            view.animate().cancel()
            row.overlay.add(view)
            view.animate().alpha(0f).scaleX(LEAVE_SCALE).scaleY(LEAVE_SCALE)
                .setDuration(LEAVE_MS).setInterpolator(ACCELERATE)
                .withEndAction { row.overlay.remove(view) }
        }
    }

    private fun number(i: Int) = "%d".format(i + 1)

    /** The program's title (`sh` when it set none), marked once the shell has ended. */
    private fun title(session: VtSession): String {
        val text = session.title?.takeIf { it.isNotBlank() } ?: "sh"
        return if (session.isRunning) text else context.getString(R.string.tab_ended, text)
    }

    /**
     * A tab's number, which rolls to a new one ([roll]) like SwiftUI's numeric text: each digit
     * that changes moves out, fading and blurring, as the new one moves in from the other side and
     * sharpens; a lower number comes in from above, a higher one from below. Digits that stay the
     * same in place stay still, and a number of another length rolls whole.
     *
     * Drawn by hand digit by digit, rolling or not, centred as by its gravity, so that a digit
     * that stays does not shift by a fraction of a pixel as a roll starts or ends.
     */
    @SuppressLint("AppCompatCustomView") // The activity is a framework one, like the views it builds
    private class TabNumber(context: Context) : TextView(context) {
        /** The number rolling out; null while none rolls. */
        private var old: String? = null

        /** How far the roll has come, eased: from 0, the old number in place, to 1. */
        private var progress = 0f

        /** 1 when the new number comes in from above, -1 from below. */
        private var direction = 1f

        /** The blur of the digits moving out and of those moving in, null when too slight. */
        private var outBlur: MaskFilter? = null
        private var inBlur: MaskFilter? = null

        val rolling get() = old != null

        /** Rolls to [number] (when not [rolling]); [onEnd] runs once it shows alone. */
        fun roll(number: String, onEnd: () -> Unit) {
            val from = text.toString()
            old = from
            text = number
            direction = if ((number.toIntOrNull() ?: 0) < (from.toIntOrNull() ?: 0)) 1f else -1f
            ValueAnimator.ofFloat(0f, 1f).apply {
                duration = RENUMBER_MS
                interpolator = STANDARD
                addUpdateListener {
                    progress = it.animatedValue as Float
                    outBlur = blur(progress)
                    inBlur = blur(1f - progress)
                    invalidate()
                }
                doOnEnd {
                    old = null
                    outBlur = null
                    inBlur = null
                    invalidate()
                    onEnd()
                }
                start()
            }
        }

        /** A blur by [f] of the most, made here as onDraw must not allocate. */
        private fun blur(f: Float): MaskFilter? {
            val radius = f * textSize * ROLL_BLUR
            return if (radius < 0.5f) null else BlurMaskFilter(radius, BlurMaskFilter.Blur.NORMAL)
        }

        override fun onDraw(canvas: Canvas) {
            val new = text.toString()
            val old = old
            val color = currentTextColor
            val y = baseline.toFloat()
            val shift = textSize * ROLL_DISTANCE * direction
            val newStart = start(new)
            for (i in new.indices) {
                val x = newStart + paint.measureText(new, 0, i)
                if (old == null || stays(old, new, i)) {
                    drawDigit(canvas, new, i, x, y, color, 1f, null)
                } else {
                    drawDigit(canvas, new, i, x, y - shift * (1f - progress), color, progress, inBlur)
                }
            }
            if (old != null) {
                val oldStart = start(old)
                for (i in old.indices) {
                    if (stays(old, new, i)) continue
                    val x = oldStart + paint.measureText(old, 0, i)
                    drawDigit(canvas, old, i, x, y + shift * progress, color, 1f - progress, outBlur)
                }
            }
            paint.maskFilter = null
        }

        /** Whether [old]'s [i]th digit stays as [new]'s, in its place. */
        private fun stays(old: String, new: String, i: Int) = old.length == new.length && old[i] == new[i]

        /** Where [number] starts when centred between the paddings. */
        private fun start(number: String) =
            paddingLeft + (width - paddingLeft - paddingRight - paint.measureText(number)) / 2

        /** Draws [number]'s [i]th digit at [x], [y] in [color], [shown] opaque. */
        private fun drawDigit(canvas: Canvas, number: String, i: Int, x: Float, y: Float, color: Int, shown: Float, blur: MaskFilter?) {
            paint.color = ColorUtils.setAlphaComponent(color, (Color.alpha(color) * shown).roundToInt())
            paint.maskFilter = blur
            canvas.drawText(number, i, i + 1, x, y, paint)
        }
    }

    /** A button: a square as tall as the bar ([item]) with its label centred. */
    private fun button(label: String, onClick: (View) -> Unit) = TextView(context).apply {
        text = label
        setTextColor(mutedColor)
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, BAR_LABEL_DP)
        gravity = Gravity.CENTER
        background = touchFeedback()
        setOnClickListener(onClick)
    }

    /** A tab or a button: a square as tall as the bar. */
    private fun item() = LinearLayout.LayoutParams(dp(BAR_DP), ViewGroup.LayoutParams.MATCH_PARENT)

    /** The theme's ripple for something that can be tapped. */
    private fun touchFeedback(): Drawable? {
        val a = context.obtainStyledAttributes(intArrayOf(android.R.attr.selectableItemBackground))
        return a.getDrawable(0).also { a.recycle() }
    }

    private fun dp(v: Int) = (v * density).toInt()

    private fun themeColor(attr: Int) = MaterialColors.getColor(context, attr, Color.WHITE)

    companion object {
        /** The bar's height, and the side of its tabs and buttons. */
        private const val BAR_DP = 32

        /**
         * The size of the buttons' labels. In dp, not sp: the bar's height is fixed, so its labels
         * ignore the system font scale.
         */
        private const val BAR_LABEL_DP = 14f

        /** The size of the tabs' numbers (dp, like [BAR_LABEL_DP]). */
        private const val TAB_NUMBER_DP = 12f

        /**
         * How far the fade reaches into `×`, ending in the blank before its glyph; the rest of
         * [ExtraKeysView.FADE_DP] lies at its left.
         */
        private const val FADE_INTO_CLOSE_DP = 12

        /** The opacity of an ended tab's number (Material's disabled content). */
        private const val ENDED_ALPHA = 0.38f

        /**
         * How long the row rests, with no finger on it, before it brings a current tab it was
         * scrolled away from back into view; longer than a tab's title stays up after a long press.
         */
        private const val RETURN_TO_CURRENT_TAB_MS = 3000L

        /** How long tabs take to slide into the places of closed ones (Material's short 4). */
        private const val SLIDE_MS = 200L

        /** How long a closed tab takes to shrink to [LEAVE_SCALE] and fade out. */
        private const val LEAVE_MS = 150L
        private const val LEAVE_SCALE = 0.6f

        /** How long a tab's number takes to roll to the new one (Material's medium 2). */
        private const val RENUMBER_MS = 300L

        /** How far a rolling digit moves, and how much it blurs at most, in its text size. */
        private const val ROLL_DISTANCE = 0.7f
        private const val ROLL_BLUR = 0.12f

        /** Material's standard easing, for the slides and the roll. */
        private val STANDARD = PathInterpolator(0.2f, 0f, 0f, 1f)

        /** Material's standard accelerate easing, for a closed tab on its way out. */
        private val ACCELERATE = PathInterpolator(0.3f, 0f, 1f, 1f)
    }
}
