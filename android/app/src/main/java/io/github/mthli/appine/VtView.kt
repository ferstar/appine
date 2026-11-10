package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.DashPathEffect
import android.graphics.Matrix
import android.graphics.Paint
import android.graphics.Path
import android.graphics.Rect
import android.graphics.Typeface
import android.graphics.drawable.Drawable
import android.os.SystemClock
import android.text.InputType
import android.util.Log
import android.util.TypedValue
import android.view.ActionMode
import android.view.GestureDetector
import android.view.HapticFeedbackConstants
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.Menu
import android.view.MenuItem
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.View
import android.view.ViewConfiguration
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.CursorAnchorInfo
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.OverScroller
import androidx.core.graphics.createBitmap
import androidx.core.graphics.withClip
import androidx.core.graphics.withScale
import androidx.core.net.toUri
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.Locale
import kotlin.math.abs
import kotlin.math.ceil
import kotlin.math.floor
import kotlin.math.hypot
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * The terminal view on libghostty-vt (the android/terminal map): draws the [VtSession] attached to
 * it with JetBrains Mono on a fixed cell grid. Sessions belong to [TerminalService] and keep parsing output while no view shows them;
 * [attach] switches the view to another tab.
 *
 * Each frame takes the rows that changed ([VtNative.frame]) into a mirror of the screen and paints
 * them into a bitmap the size of the grid; the cursor and the input method's composing text are
 * drawn over it. Glyphs are placed cell by cell: runs of ASCII share one draw call (the font size
 * is chosen so their advance is exactly one cell), every other glyph is centred in its one or two
 * cells, enlarged evenly to fill two cells when it is wide (the setting, for CJK fallback glyphs
 * narrower than two cells) or shrunk evenly when it is wider, never stretched. Box drawing, block
 * elements and the Powerline triangles are drawn as shapes ([VtGlyphs]).
 *
 * Touch: a tap opens the keyboard (and clicks, when the program tracks the mouse); a drag scrolls
 * the scrollback, or sends wheel events to a program tracking the mouse, or arrow keys on the
 * alternate screen (alternate scroll, mode 1007), and a fling goes on scrolling the same way; while
 * the scrollback scrolls, a scroll bar like a ScrollView's shows where the viewport is in it. A long
 * press selects a word, and dragging on extends the selection, as do its two handles; a floating
 * menu copies, pastes and selects all.
 * The selection is the terminal's, so it stays on its text through output and scrolling.
 */
@SuppressLint("ViewConstructor") // Created only in code
internal class VtView(context: Context, private val host: Host) : View(context) {

    /** The Activity's side (all on the main thread). */
    interface Host {
        /** The grid's size, first known or changed. */
        fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int)

        /** Bytes for the program: keys, text, pastes, mouse and focus events. */
        fun onInput(data: ByteArray)

        /** Pinch zoom changed the font size (pixels), to keep for next time. */
        fun onZoom(fontPx: Int)

        /** The extra keys' one-shot Ctrl, Alt and Shift, which apply to the next key or character. */
        val ctrl: Boolean
        val alt: Boolean
        val shift: Boolean

        /** The next key or character took the one-shot modifiers. */
        fun modifiersUsed()

        /** A link was tapped or chosen from the menu, to open in another app. */
        fun onLink(uri: String)
    }

    /** The tab shown, null for none. */
    var session: VtSession? = null
        private set

    /** The shown tab's terminal, 0 when there is none or its shell has not started. */
    private val tab: Long get() = session?.tab ?: 0L

    private val upright = font("fonts/JetBrainsMono.ttf", 400)
    private val paints = arrayOf(
        textPaint(upright),
        textPaint(font("fonts/JetBrainsMono.ttf", 700)),
        textPaint(font("fonts/JetBrainsMono-Italic.ttf", 400)),
        textPaint(font("fonts/JetBrainsMono-Italic.ttf", 700)),
    )
    private val fill = Paint()
    private val stroke = Paint(Paint.ANTI_ALIAS_FLAG).apply { style = Paint.Style.STROKE }
    private val curl = Path()
    private val glyphs = VtGlyphs()

    /** The font size in pixels; [MainActivity] keeps it across starts. */
    private var fontPx = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, 12f, resources.displayMetrics)
    private var cellWidth = 1
    private var cellHeight = 1
    private var baseline = 0
    private var light = 1
    private var underlineY = 0
    private var strikeY = 0
    private var dotted = DashPathEffect(floatArrayOf(1f, 1f), 0f)
    private var dashed = DashPathEffect(floatArrayOf(3f, 2f), 0f)

    /** Whether glyphs in wide cells are enlarged to fill them ([setFillWide]). */
    private var fillWide = true

    /**
     * How much glyphs in wide cells are enlarged: so that an ideograph's advance fills two cells
     * when [fillWide], else 1. The system's CJK fallback draws ideographs one em wide, short of two
     * cells of JetBrains Mono (1.2 em), which leaves wide gaps between them; measured, so a font
     * with its own ideographs two cells wide gets 1.
     */
    private var wideScale = 1f

    /** Advances of single codepoints, per paint (index << 32 | codepoint). */
    private val widths = HashMap<Long, Float>()

    // The screen, as of the last frame
    private var cols = 0
    private var rows = 0
    private var cp = IntArray(0)
    private var fg = IntArray(0)
    private var bg = IntArray(0)
    private var attrs = IntArray(0)

    /** Whole graphemes of cells with more than one codepoint, else null. */
    private var clusters = arrayOfNulls<String>(0)
    private var defaultFg = Color.WHITE
    private var defaultBg = Color.BLACK
    private var cursorColor = 0
    private var cursorShown = false
    private var cursorStyle = 1
    private var cursorBlinking = false
    private var cursorX = 0
    private var cursorY = 0
    private var atBottom = true

    /** A blinking cursor ([updateBlink]): whether the timer runs, and the phase it is in. */
    private var blinkRunning = false
    private var blinkOn = true
    private val blinkTick = object : Runnable {
        override fun run() {
            blinkOn = !blinkOn
            invalidate()
            postDelayed(this, BLINK_MS)
        }
    }

    /** When the bell last vibrated ([bell]). */
    private var lastBell = 0L

    // The scrollable area (scrollback and screen) in rows, the viewport's first row in it and the
    // rows it shows, for the scroll bar
    private var scrollTotal = 0
    private var scrollOffset = 0
    private var scrollLen = 0

    // Modes and the selection, as of the last frame (vt.c's header flags)
    private var mouseTracking = false
    private var altScreen = false
    private var altScroll = false

    /** Whether the terminal has a selection; also set at once by the view's own changes. */
    private var selected = false

    /** The selection's first and last cell in the viewport, -1 when outside it. */
    private var selStartX = -1
    private var selStartY = -1
    private var selEndX = -1
    private var selEndY = -1

    /** Each row's selected columns (inclusive), -1 when none. */
    private var selFrom = IntArray(0)
    private var selTo = IntArray(0)

    private var bitmap: Bitmap? = null
    private var canvas: Canvas? = null

    /** Ask the next frame for every row (after a resize, a new bitmap, new default colours). */
    private var full = true
    private val runChars = CharArray(4096)

    /** The input method's composing text, drawn at the cursor. */
    private var preedit = ""

    // Frame statistics for logcat (tag appine-vt): taking and painting a frame
    private var frames = 0
    private var frameNanos = 0L
    private var frameMax = 0L
    private var frameRows = 0

    private var scrollRemainder = 0f
    private var scaleRemainder = 1f

    /**
     * A fling: pixels scrolled over time, taken in rows as a drag's are, the way scrolling went when
     * it started ([scrollWay]) and with wheel events at [flingX], [flingY]; [flingLast] is the
     * position taken so far.
     */
    private val scroller = OverScroller(context)
    private var flingWay = WAY_VIEWPORT
    private var flingX = 0f
    private var flingY = 0f
    private var flingLast = 0

    /** The touch under way stopped a fling, so its tap does nothing else. */
    private var stoppedFling = false

    // Selection drags: by the long-pressed word, or by a handle; in mouse mode a long press held
    // on may instead become a drag for the program
    private var drag = DRAG_NONE
    private var dragOffsetX = 0f
    private var dragOffsetY = 0f
    private var dragCellX = -1
    private var dragCellY = -1

    /** Where a long press in mouse mode was: the program's drag starts there if the finger moves on. */
    private var pressX = 0f
    private var pressY = 0f

    /** The button of a mouse (not a finger) held down for a program tracking the mouse, or 0. */
    private var mouseButton = 0

    /** The last tap's lift, to tell the second tap of a double tap. */
    private var lastTapTime = 0L
    private var lastTapX = 0f
    private var lastTapY = 0f
    private val touchSlop = ViewConfiguration.get(context).scaledTouchSlop
    private val doubleTapSlop = ViewConfiguration.get(context).scaledDoubleTapSlop
    private val minFling = ViewConfiguration.get(context).scaledMinimumFlingVelocity

    /** Rows the viewport scrolls by while a drag is held above (-1) or below (1) the grid. */
    private var autoscroll = 0
    private var autoscrollX = 0
    private val autoscrollTick = object : Runnable {
        override fun run() {
            if (autoscroll == 0 || tab == 0L) return
            VtNative.scroll(tab, autoscroll)
            VtNative.selectTo(tab, autoscrollX, if (autoscroll < 0) 0 else rows - 1)
            awakenScrollBars()
            invalidate()
            postDelayed(this, AUTOSCROLL_MS)
        }
    }

    private val handleStart = themeDrawable(android.R.attr.textSelectHandleLeft)
    private val handleEnd = themeDrawable(android.R.attr.textSelectHandleRight)
    /** How far around a handle a touch still takes it. */
    private val handleSlop = (8 * resources.displayMetrics.density).toInt()
    /** The handles' areas last kept out of the system's gestures. */
    private var handleExclusion = emptyList<Rect>()

    private var menu: ActionMode? = null

    /** Where a menu without a selection (paste only) points, or -1 to point at the selection. */
    private var menuX = -1f
    private var menuY = -1f

    /** Show the menu after the next frame, which has the selection's new bounds. */
    private var menuPending = false

    /** The link under the long press that began the selection, which the menu offers to open. */
    private var menuLink: String? = null

    /** The IME asked for cursor positions as they change (its candidate window follows them). */
    private var monitorCursor = false

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        // Otherwise, focused outside touch mode (after hardware keys), the whole view is covered by
        // the system's translucent focus highlight
        defaultFocusHighlightEnabled = false
        // The theme's scroll bar, shown and faded as a ScrollView's (computeVerticalScroll*)
        isVerticalScrollBarEnabled = true
        isScrollbarFadingEnabled = true
        verticalScrollbarThumbDrawable = themeDrawable(android.R.attr.scrollbarThumbVertical)
        measureCells()
    }

    private fun themeDrawable(attr: Int): Drawable? {
        val a = context.obtainStyledAttributes(intArrayOf(attr))
        try {
            return a.getDrawable(0)
        } finally {
            a.recycle()
        }
    }

    private fun font(path: String, weight: Int): Typeface =
        Typeface.Builder(context.assets, path).setFontVariationSettings("'wght' $weight").build()

    private fun textPaint(typeface: Typeface) =
        Paint(Paint.ANTI_ALIAS_FLAG or Paint.SUBPIXEL_TEXT_FLAG).apply {
            this.typeface = typeface
            // Ligatures would join cells
            fontFeatureSettings = "'calt' 0, 'liga' 0"
        }

    /**
     * Shows [s] (null for nothing): the grid is applied to its terminal, the whole screen is taken
     * again, and a selection drag, menu or fling of the previous tab ends.
     */
    fun attach(s: VtSession?) {
        if (s === session) return
        session?.let { if (it.view === this) it.view = null }
        menu?.finish()
        drag = DRAG_NONE
        autoscroll = 0
        removeCallbacks(autoscrollTick)
        scroller.forceFinished(true)
        selected = false
        scrollTotal = 0
        scrollOffset = 0
        scrollLen = 0
        session = s
        s?.view = this
        if (s != null && cols > 0) {
            s.resize(cols, rows, cellWidth, cellHeight)
            // A tab whose shell has not started yet starts on this grid
            host.onGrid(cols, rows, cellWidth, cellHeight)
        }
        full = true
        invalidate()
    }

    /** The session's shell started: take the whole screen. */
    fun onStarted() {
        full = true
        invalidate()
    }

    /**
     * A program in the tab shown rang the bell: a short vibration (the system's reject pattern,
     * subject to its touch feedback setting), at most one per [BELL_MS], so a shell's completion
     * beeps or a flood of BELs do not buzz on.
     */
    fun bell() {
        val now = SystemClock.uptimeMillis()
        if (now - lastBell < BELL_MS) return
        lastBell = now
        performHapticFeedback(HapticFeedbackConstants.REJECT)
    }

    // ---- Metrics and the grid ----

    /**
     * Cell size for [fontPx]: the width is the font's advance rounded to whole pixels, and the text
     * size is then adjusted so the advance is exactly that width (ASCII runs stay on the grid).
     */
    private fun measureCells() {
        for (p in paints) p.textSize = fontPx
        val advance = paints[0].measureText("M")
        cellWidth = max(1, advance.roundToInt())
        val size = fontPx * cellWidth / advance
        for (p in paints) p.textSize = size
        val fm = paints[0].fontMetrics
        val height = fm.descent - fm.ascent
        cellHeight = max(1, ceil(height).toInt())
        baseline = (-fm.ascent + (cellHeight - height) / 2).roundToInt()
        light = max(1, paints[0].underlineThickness.roundToInt())
        underlineY = baseline + max(1, paints[0].underlinePosition.roundToInt())
        strikeY = baseline + paints[0].strikeThruPosition.roundToInt()
        dotted = DashPathEffect(floatArrayOf(light.toFloat(), light.toFloat()), 0f)
        dashed = DashPathEffect(floatArrayOf(light * 3f, light * 2f), 0f)
        stroke.strokeWidth = light.toFloat()
        val ideograph = paints[0].measureText("水")
        wideScale = if (fillWide && ideograph > 0f) max(1f, 2 * cellWidth / ideograph) else 1f
        widths.clear()
    }

    override fun onSizeChanged(w: Int, h: Int, oldw: Int, oldh: Int) = regrid()

    private fun regrid() {
        if (width == 0 || height == 0) return
        val c = max(1, width / cellWidth)
        val r = max(1, height / cellHeight)
        val bw = c * cellWidth
        val bh = r * cellHeight
        if (c == cols && r == rows && bitmap?.width == bw && bitmap?.height == bh) return
        cols = c
        rows = r
        val n = c * r
        cp = IntArray(n)
        fg = IntArray(n)
        bg = IntArray(n)
        attrs = IntArray(n)
        clusters = arrayOfNulls(n)
        selFrom = IntArray(r) { -1 }
        selTo = IntArray(r) { -1 }
        bitmap?.recycle()
        bitmap = createBitmap(bw, bh).also { canvas = Canvas(it) }
        full = true
        session?.resize(c, r, cellWidth, cellHeight)
        host.onGrid(c, r, cellWidth, cellHeight)
        invalidate()
    }

    /** Sets the font size in pixels (kept within 6–32 dp). */
    fun setFontSize(px: Float) {
        val density = resources.displayMetrics.density
        val size = px.coerceIn(6 * density, 32 * density)
        if (size == fontPx) return
        fontPx = size
        measureCells()
        regrid()
    }

    /** Enlarges glyphs in wide cells to fill them, or draws them at their size (the setting). */
    fun setFillWide(on: Boolean) {
        if (on == fillWide) return
        fillWide = on
        measureCells()
        full = true
        invalidate()
    }

    private fun zoom(step: Int) {
        val before = fontPx
        setFontSize(fontPx + step * resources.displayMetrics.density)
        if (fontPx != before) host.onZoom(fontPx.roundToInt())
    }

    // ---- Frames ----

    override fun onDraw(c: Canvas) {
        c.drawColor(defaultBg)
        val bmp = bitmap ?: return
        if (tab != 0L) {
            val start = SystemClock.elapsedRealtimeNanos()
            val f = full
            full = false
            VtNative.frame(tab, f)?.let(::apply)
            stats(SystemClock.elapsedRealtimeNanos() - start)
        }
        c.drawBitmap(bmp, 0f, 0f, null)
        drawCursor(c)
        drawPreedit(c)
        drawHandles(c)
        if (menuPending) {
            menuPending = false
            post { showMenu() }
        } else {
            menu?.invalidateContentRect()
        }
    }

    /** Takes a frame (format in `hosts/android/src/vt.c`) into the mirror and the bitmap. */
    private fun apply(b: ByteBuffer) {
        b.order(ByteOrder.LITTLE_ENDIAN)
        val c = b.getShort(0).toInt() and 0xffff
        val r = b.getShort(2).toInt() and 0xffff
        if (c != cols || r != rows) {
            // Captured before the resize reached the terminal
            full = true
            postInvalidateOnAnimation()
            return
        }
        val oldCursorX = cursorX
        val oldCursorY = cursorY
        cursorShown = b.get(5).toInt() != 0
        cursorStyle = b.get(6).toInt()
        cursorBlinking = b.get(7).toInt() != 0
        cursorX = b.getShort(8).toInt() and 0xffff
        cursorY = b.getShort(10).toInt() and 0xffff
        val newFg = b.getInt(12)
        val newBg = b.getInt(16)
        cursorColor = b.getInt(20)
        val count = b.getShort(24).toInt() and 0xffff
        atBottom = b.get(26).toInt() != 0
        val flags = b.get(27).toInt()
        mouseTracking = flags and FLAG_MOUSE != 0
        altScreen = flags and FLAG_ALT_SCREEN != 0
        altScroll = flags and FLAG_ALT_SCROLL != 0
        val hadSelection = selected
        selected = flags and FLAG_SELECTION != 0
        if (hadSelection && !selected && menuX < 0) menu?.finish()
        selStartX = coordinate(b, 28)
        selStartY = coordinate(b, 30)
        selEndX = coordinate(b, 32)
        selEndY = coordinate(b, 34)
        scrollTotal = b.getInt(36)
        scrollOffset = b.getInt(40)
        scrollLen = b.getInt(44)
        if (monitorCursor && (cursorX != oldCursorX || cursorY != oldCursorY)) updateCursorAnchor()
        updateBlink()
        if (cursorX != oldCursorX || cursorY != oldCursorY) restartBlink()
        if (newFg != defaultFg || newBg != defaultBg) {
            // Rows that did not change still have the old default colours
            defaultFg = newFg
            defaultBg = newBg
            full = true
            postInvalidateOnAnimation()
        }
        frameRows += count
        val target = canvas ?: return
        var pos = HEADER_BYTES
        repeat(count) {
            val y = b.getShort(pos).toInt() and 0xffff
            val n = b.getShort(pos + 2).toInt() and 0xffff
            val extra = b.getInt(pos + 4)
            val from = coordinate(b, pos + 8)
            val to = coordinate(b, pos + 10)
            pos += ROW_HEADER_BYTES
            var e = pos + n * 16
            if (y < rows) {
                selFrom[y] = if (to >= 0) from else -1
                selTo[y] = to
                val base = y * cols
                for (x in 0 until cols) {
                    val i = base + x
                    if (x >= n) {
                        cp[i] = 0
                        fg[i] = 0
                        bg[i] = 0
                        attrs[i] = 0
                        clusters[i] = null
                        continue
                    }
                    val o = pos + x * 16
                    cp[i] = b.getInt(o)
                    fg[i] = b.getInt(o + 4)
                    bg[i] = b.getInt(o + 8)
                    attrs[i] = b.getShort(o + 12).toInt() and 0xffff
                    val more = b.getShort(o + 14).toInt() and 0xffff
                    clusters[i] = if (more == 0) {
                        null
                    } else {
                        StringBuilder().apply {
                            appendCodePoint(cp[i])
                            repeat(more) {
                                appendCodePoint(b.getInt(e))
                                e += 4
                            }
                        }.toString()
                    }
                }
                drawRow(target, y)
            }
            pos += n * 16 + extra * 4
        }
    }

    /** A u16 coordinate of the frame, -1 for 0xffff (none). */
    private fun coordinate(b: ByteBuffer, at: Int): Int =
        (b.getShort(at).toInt() and 0xffff).let { if (it == 0xffff) -1 else it }

    private fun stats(nanos: Long) {
        frames++
        frameNanos += nanos
        frameMax = max(frameMax, nanos)
        if (frames == 120) {
            Log.d(
                "appine-vt",
                "frames: avg %.2f ms, max %.2f ms, %.1f rows each".format(
                    frameNanos / 1e6 / frames, frameMax / 1e6, frameRows.toFloat() / frames,
                ),
            )
            frames = 0
            frameNanos = 0
            frameMax = 0
            frameRows = 0
        }
    }

    // The scroll bar, which View draws after onDraw from the frame just taken; it shows only with a
    // scrollback, so never on the alternate screen
    override fun computeVerticalScrollRange() = scrollTotal

    override fun computeVerticalScrollOffset() = scrollOffset

    override fun computeVerticalScrollExtent() = scrollLen

    // ---- Painting ----

    private fun drawRow(c: Canvas, y: Int) {
        val top = y * cellHeight
        val base = y * cols
        c.withClip(0, top, cols * cellWidth, top + cellHeight) {
            fill.color = defaultBg
            c.drawRect(0f, top.toFloat(), (cols * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
            var x = 0
            while (x < cols) {
                val color = bgOf(base + x)
                var end = x + 1
                while (end < cols && bgOf(base + end) == color) end++
                if (color != defaultBg) {
                    fill.color = color
                    c.drawRect((x * cellWidth).toFloat(), top.toFloat(), (end * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
                }
                x = end
            }
            val from = selFrom[y]
            if (from >= 0) {
                // The highlight covers a wide character whose first cell ends the selection
                val to = selTo[y] + if ((attrs[base + selTo[y]] shr 12) and 3 == WIDE) 2 else 1
                fill.color = SELECTION
                c.drawRect((from * cellWidth).toFloat(), top.toFloat(), (min(to, cols) * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
            }

            // Runs of plain ASCII in one style go out in one call; other cells one at a time
            var runStart = -1
            var runLength = 0
            var runPaint: Paint? = null
            var runColor = 0
            var runDecor = 0
            fun flush() {
                if (runLength > 0) {
                    runPaint!!.color = runColor
                    c.drawText(runChars, 0, runLength, (runStart * cellWidth).toFloat(), (top + baseline).toFloat(), runPaint!!)
                    decorate(c, runDecor, runStart, runStart + runLength, top, runColor)
                }
                runStart = -1
                runLength = 0
            }
            x = 0
            while (x < cols) {
                val i = base + x
                val a = attrs[i]
                val wide = (a shr 12) and 3
                if (wide == SPACER_TAIL || wide == SPACER_HEAD) {
                    x++
                    continue
                }
                val code = cp[i]
                val color = fgOf(i)
                val paint = paintOf(i)
                val decor = a and DECORATIONS
                val plain = clusters[i] == null && wide == NARROW && a and INVISIBLE == 0 &&
                    (code == 0 || code in 0x20..0x7e)
                if (plain) {
                    if (runLength > 0 && (paint !== runPaint || color != runColor || decor != runDecor)) flush()
                    if (runLength == 0) {
                        runStart = x
                        runPaint = paint
                        runColor = color
                        runDecor = decor
                    }
                    runChars[runLength++] = if (code == 0) ' ' else code.toChar()
                    x++
                    continue
                }
                flush()
                val span = if (wide == WIDE) 2 else 1
                if (a and INVISIBLE == 0) drawGlyph(c, i, x, span, top, color, paint)
                decorate(c, decor, x, x + span, top, color)
                x += span
            }
            flush()
        }
    }

    private fun bgOf(i: Int): Int =
        if (attrs[i] and INVERSE != 0) fg[i].takeIf { it != 0 } ?: defaultFg else bg[i].takeIf { it != 0 } ?: defaultBg

    private fun fgOf(i: Int): Int {
        val a = attrs[i]
        val color = if (a and INVERSE != 0) bg[i].takeIf { it != 0 } ?: defaultBg else fg[i].takeIf { it != 0 } ?: defaultFg
        return if (a and FAINT != 0) (color and 0x00ffffff) or 0x80000000.toInt() else color
    }

    private fun paintOf(i: Int): Paint = paints[(attrs[i] and BOLD) or ((attrs[i] and ITALIC))]

    /**
     * One cell's glyph, centred in its [span] cells: a wide one enlarged by [wideScale], and any
     * shrunk evenly to fit them.
     */
    private fun drawGlyph(c: Canvas, i: Int, x: Int, span: Int, top: Int, color: Int, paint: Paint) {
        val code = cp[i]
        val cluster = clusters[i]
        val left = x * cellWidth
        val w = span * cellWidth
        if (cluster == null && glyphs.covers(code)) {
            glyphs.draw(c, code, left, top, w, cellHeight, light, color)
            return
        }
        if (cluster == null && (code == 0 || code == ' '.code)) return
        val text = cluster ?: String(Character.toChars(code))
        val advance = if (cluster != null) {
            paint.measureText(cluster)
        } else {
            val key = (paints.indexOf(paint).toLong() shl 32) or code.toLong()
            widths.getOrPut(key) { paint.measureText(text) }
        }
        paint.color = color
        var s = if (span == 2) wideScale else 1f
        if (advance * s > w + 0.5f) s = w / advance
        val l = left + (w - advance * s) / 2
        if (s == 1f) {
            c.drawText(text, l, (top + baseline).toFloat(), paint)
        } else {
            c.withScale(s, s, l, top + cellHeight / 2f) { c.drawText(text, l, (top + baseline).toFloat(), paint) }
        }
    }

    private fun decorate(c: Canvas, decor: Int, from: Int, to: Int, top: Int, color: Int) {
        if (decor == 0) return
        val l = (from * cellWidth).toFloat()
        val r = (to * cellWidth).toFloat()
        fill.color = color
        stroke.color = color
        val underline = (decor shr 8) and 7
        val uy = (top + underlineY).toFloat()
        when (underline) {
            0 -> {}
            UNDERLINE_DOUBLE -> {
                c.drawRect(l, uy, r, uy + light, fill)
                c.drawRect(l, uy + 2 * light, r, uy + 3 * light, fill)
            }
            UNDERLINE_CURLY -> {
                curl.rewind()
                val amp = light * 1.5f
                val wave = max(4f, cellWidth / 2f)
                val mid = uy + amp
                curl.moveTo(l, mid)
                var px = l
                var up = true
                while (px < r) {
                    val nx = minOf(r, px + wave)
                    curl.quadTo((px + nx) / 2, if (up) mid - 2 * amp else mid + 2 * amp, nx, mid)
                    px = nx
                    up = !up
                }
                c.drawPath(curl, stroke)
            }
            UNDERLINE_DOTTED, UNDERLINE_DASHED -> {
                stroke.pathEffect = if (underline == UNDERLINE_DOTTED) dotted else dashed
                c.drawLine(l, uy + light / 2f, r, uy + light / 2f, stroke)
                stroke.pathEffect = null
            }
            else -> c.drawRect(l, uy, r, uy + light, fill)
        }
        if (decor and STRIKE != 0) {
            val sy = (top + strikeY).toFloat()
            c.drawRect(l, sy, r, sy + light, fill)
        }
        if (decor and OVERLINE != 0) c.drawRect(l, top.toFloat(), r, (top + light).toFloat(), fill)
    }

    private fun drawCursor(c: Canvas) {
        if (tab == 0L || !cursorShown || cursorX >= cols || cursorY >= rows) return
        if (blinkRunning && !blinkOn) return
        val i = cursorY * cols + cursorX
        val span = if ((attrs[i] shr 12) and 3 == WIDE) 2 else 1
        val l = cursorX * cellWidth
        val t = cursorY * cellHeight
        val r = l + span * cellWidth
        val b = t + cellHeight
        val color = if (cursorColor != 0) cursorColor else defaultFg
        fill.color = color
        stroke.color = color
        when {
            cursorStyle == CURSOR_BAR -> c.drawRect(l.toFloat(), t.toFloat(), (l + 2 * light).toFloat(), b.toFloat(), fill)
            cursorStyle == CURSOR_UNDERLINE -> c.drawRect(l.toFloat(), (b - 2 * light).toFloat(), r.toFloat(), b.toFloat(), fill)
            cursorStyle == CURSOR_HOLLOW || !hasFocus() ->
                c.drawRect(l + light / 2f, t + light / 2f, r - light / 2f, b - light / 2f, stroke)
            else -> {
                // A block: the cell's glyph in the background colour on top
                c.withClip(l, t, r, b) {
                    c.drawRect(l.toFloat(), t.toFloat(), r.toFloat(), b.toFloat(), fill)
                    if (attrs[i] and INVISIBLE == 0) drawGlyph(c, i, cursorX, span, t, bgOf(i), paintOf(i))
                }
            }
        }
    }

    /**
     * Blinks the cursor every [BLINK_MS] (as Android's text fields do) while the program asks for
     * a blinking one (DECSCUSR, mode 12) and the view has the keyboard focus in a focused window;
     * otherwise no timer runs and the cursor stays shown (hollow without the focus).
     */
    private fun updateBlink() {
        val blinking = cursorBlinking && cursorShown && hasFocus() && hasWindowFocus() && isAttachedToWindow
        if (blinking == blinkRunning) return
        blinkRunning = blinking
        removeCallbacks(blinkTick)
        if (blinking) {
            blinkOn = true
            postDelayed(blinkTick, BLINK_MS)
        } else if (!blinkOn) {
            blinkOn = true
            invalidate()
        }
    }

    /** Input or a moved cursor shows a blinking cursor again for a whole period. */
    private fun restartBlink() {
        if (!blinkRunning) return
        removeCallbacks(blinkTick)
        if (!blinkOn) {
            blinkOn = true
            invalidate()
        }
        postDelayed(blinkTick, BLINK_MS)
    }

    private fun drawPreedit(c: Canvas) {
        if (preedit.isEmpty() || cursorY >= rows) return
        val p = paints[0]
        val w = p.measureText(preedit)
        // Shifted left when it would run past the right edge
        val l = min((cursorX * cellWidth).toFloat(), cols * cellWidth - w).coerceAtLeast(0f)
        val t = (cursorY * cellHeight).toFloat()
        fill.color = defaultBg
        c.drawRect(l, t, l + w, t + cellHeight, fill)
        p.color = defaultFg
        c.drawText(preedit, l, t + baseline, p)
        fill.color = defaultFg
        c.drawRect(l, t + underlineY, l + w, t + underlineY + light, fill)
    }

    // ---- Input ----

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean =
        key(event) || super.onKeyDown(keyCode, event)

    /** Releases matter only to programs using the Kitty keyboard protocol's event types. */
    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        val key = VtKeys.of(keyCode)
        if (tab == 0L || systemKey(event) || key == 0) return super.onKeyUp(keyCode, event)
        val bytes = VtNative.key(tab, key, VtKeys.mods(event.metaState), 0, VtKeys.ACTION_RELEASE, null, event.getUnicodeChar(0))
        if (bytes == null || bytes.isEmpty()) return super.onKeyUp(keyCode, event)
        send(bytes)
        return true
    }

    /**
     * A key the system keeps (Back, Home, volume, media): Android counts the menu key among them,
     * but a hardware keyboard's context menu key goes to the program, as `CSI 29 ~` like Ghostty's
     * (this activity has no options menu for it).
     */
    private fun systemKey(event: KeyEvent) = event.isSystem && event.keyCode != KeyEvent.KEYCODE_MENU

    /** Encodes a key press for the program; false for keys it does not take (Back, volume). */
    private fun key(event: KeyEvent): Boolean {
        if (tab == 0L || systemKey(event)) return false
        val key = VtKeys.of(event.keyCode)
        val action = if (event.repeatCount > 0) VtKeys.ACTION_REPEAT else VtKeys.ACTION_PRESS
        if (VtKeys.isModifier(event.keyCode)) {
            // Sent only when the program asked for every key (Kitty keyboard protocol)
            val bytes = VtNative.key(tab, key, VtKeys.mods(event.metaState), 0, action, null, 0)
            if (bytes == null || bytes.isEmpty()) return false
            send(bytes)
            return true
        }
        // The extra keys' one-shot modifiers, as if held on a keyboard: with Shift a key types its
        // shifted character, and with Ctrl and Shift C and V copy and paste
        val extra = extraMeta()
        val e = if (extra == 0) {
            event
        } else {
            KeyEvent(
                event.downTime, event.eventTime, event.action, event.keyCode, event.repeatCount,
                event.metaState or extra, event.deviceId, event.scanCode, event.flags, event.source,
            )
        }
        if (shortcut(e)) {
            if (extra != 0) host.modifiersUsed()
            return true
        }
        val mods = VtKeys.mods(e.metaState)
        val plain = e.getUnicodeChar(
            e.metaState and (KeyEvent.META_CTRL_MASK or KeyEvent.META_ALT_MASK or KeyEvent.META_META_MASK).inv(),
        )
        val text = if (plain >= 0x20 && plain != 0x7f && plain and KeyCharacterMap.COMBINING_ACCENT == 0) {
            String(Character.toChars(plain))
        } else {
            null
        }
        if (key == 0) {
            if (text == null) return false
            if (mods and (VtKeys.MOD_CTRL or VtKeys.MOD_ALT) == 0) {
                if (extra != 0) host.modifiersUsed()
                input(text.toByteArray())
                return true
            }
        }
        val unshifted = e.getUnicodeChar(0)
        val consumed = if (mods and VtKeys.MOD_SHIFT != 0 && text != null && plain != unshifted) VtKeys.MOD_SHIFT else 0
        val bytes = VtNative.key(tab, key, mods, consumed, action, text, unshifted) ?: return true
        if (extra != 0) host.modifiersUsed()
        input(bytes)
        return true
    }

    /** The meta state of the extra keys' one-shot modifiers that are on, 0 for none. */
    private fun extraMeta(): Int {
        var m = 0
        if (host.shift) m = m or KeyEvent.META_SHIFT_ON or KeyEvent.META_SHIFT_LEFT_ON
        if (host.ctrl) m = m or KeyEvent.META_CTRL_ON or KeyEvent.META_CTRL_LEFT_ON
        if (host.alt) m = m or KeyEvent.META_ALT_ON or KeyEvent.META_ALT_LEFT_ON
        return m
    }

    /** Copy and paste keys: Ctrl+Shift+C and V (held, or from the extra keys) and the dedicated keys. */
    private fun shortcut(event: KeyEvent): Boolean {
        val both = event.isCtrlPressed && event.isShiftPressed
        return when {
            event.keyCode == KeyEvent.KEYCODE_PASTE || both && event.keyCode == KeyEvent.KEYCODE_V -> {
                pasteClipboard()
                true
            }
            event.keyCode == KeyEvent.KEYCODE_COPY || both && event.keyCode == KeyEvent.KEYCODE_C -> {
                if (selected) copySelection()
                true
            }
            else -> false
        }
    }

    /** A key from the extra keys row. */
    fun press(keyCode: Int) {
        key(KeyEvent(KeyEvent.ACTION_DOWN, keyCode))
    }

    /** Text from the input method or the extra keys; one character with a one-shot modifier is a key. */
    fun type(text: String) {
        if ((host.ctrl || host.alt || host.shift) && text.codePointCount(0, text.length) == 1) {
            val events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD).getEvents(text.toCharArray())
            val down = events?.firstOrNull { it.action == KeyEvent.ACTION_DOWN && it.keyCode != KeyEvent.KEYCODE_SHIFT_LEFT }
            if (down != null && key(down)) return
        }
        input(text.replace('\n', '\r').toByteArray())
    }

    /** Typed or pasted input, which also ends a selection. */
    private fun input(data: ByteArray?) {
        if (data == null || data.isEmpty()) return
        if (selected) clearSelection()
        send(data)
    }

    /**
     * Bytes for the program; the viewport returns to the bottom first. Other input stops a fling,
     * scrolling's own wheel events and arrow keys do not.
     */
    private fun send(data: ByteArray?, scrolling: Boolean = false) {
        if (data == null || data.isEmpty()) return
        if (!scrolling) scroller.forceFinished(true)
        restartBlink()
        if (!atBottom && tab != 0L) {
            VtNative.scroll(tab, 0)
            awakenScrollBars()
            invalidate()
        }
        host.onInput(data)
    }

    override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
        super.onWindowFocusChanged(hasWindowFocus)
        // A fling not drawn to its end would jump there when the window draws again
        if (!hasWindowFocus) scroller.forceFinished(true)
        // Focus events (mode 1004): editors reload changed files when they regain focus
        if (tab != 0L) send(VtNative.focus(tab, hasWindowFocus))
        updateBlink()
        invalidate()
    }

    override fun onFocusChanged(gainFocus: Boolean, direction: Int, previouslyFocusedRect: Rect?) {
        super.onFocusChanged(gainFocus, direction, previouslyFocusedRect)
        updateBlink()
    }

    override fun onDetachedFromWindow() {
        super.onDetachedFromWindow()
        // Still counted as attached during this call
        blinkRunning = false
        blinkOn = true
        removeCallbacks(blinkTick)
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_ACTION_NONE
        return Connection()
    }

    /**
     * Committed text goes to the program at once; composing text is only shown at the cursor
     * until committed. The editable is kept empty otherwise, so Backspace with nothing composed
     * arrives as deleteSurroundingText and becomes DEL. A commit of several lines (the IME's
     * clipboard) is a paste.
     */
    private inner class Connection : BaseInputConnection(this, true) {
        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
            editable?.clear()
            setPreedit("")
            val s = text.toString()
            when {
                s == "\n" -> press(KeyEvent.KEYCODE_ENTER)
                s.length > 1 && (s.contains('\n') || s.contains('\r')) -> paste(s)
                else -> type(s)
            }
            return true
        }

        override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
            super.setComposingText(text, newCursorPosition)
            setPreedit(text.toString())
            return true
        }

        override fun finishComposingText(): Boolean {
            val text = editable?.toString().orEmpty()
            super.finishComposingText()
            editable?.clear()
            setPreedit("")
            if (text.isNotEmpty()) type(text)
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            if (editable.isNullOrEmpty()) {
                repeat(beforeLength) { press(KeyEvent.KEYCODE_DEL) }
                return true
            }
            return super.deleteSurroundingText(beforeLength, afterLength)
        }

        override fun deleteSurroundingTextInCodePoints(beforeLength: Int, afterLength: Int): Boolean =
            deleteSurroundingText(beforeLength, afterLength)

        override fun performEditorAction(editorAction: Int): Boolean {
            press(KeyEvent.KEYCODE_ENTER)
            return true
        }

        override fun requestCursorUpdates(cursorUpdateMode: Int): Boolean {
            monitorCursor = cursorUpdateMode and InputConnection.CURSOR_UPDATE_MONITOR != 0
            if (cursorUpdateMode and InputConnection.CURSOR_UPDATE_IMMEDIATE != 0) updateCursorAnchor()
            return true
        }
    }

    private fun setPreedit(text: String) {
        if (text == preedit) return
        preedit = text
        invalidate()
        if (monitorCursor) updateCursorAnchor()
    }

    /** Tells the IME where the cursor and the composing text are, for a candidate window next to them. */
    private fun updateCursorAnchor() {
        val location = IntArray(2)
        getLocationOnScreen(location)
        val matrix = Matrix().apply { setTranslate(location[0].toFloat(), location[1].toFloat()) }
        val x = (cursorX * cellWidth).toFloat() + paints[0].measureText(preedit)
        val top = (cursorY * cellHeight).toFloat()
        val info = CursorAnchorInfo.Builder()
            .setMatrix(matrix)
            .setSelectionRange(preedit.length, preedit.length)
            .setComposingText(0, preedit)
            .setInsertionMarkerLocation(x, top, top + baseline, top + cellHeight, CursorAnchorInfo.FLAG_HAS_VISIBLE_REGION)
            .build()
        context.getSystemService(InputMethodManager::class.java).updateCursorAnchorInfo(this, info)
    }

    // ---- Clipboard ----

    private val clipboard get() = context.getSystemService(ClipboardManager::class.java)

    private fun copySelection() {
        val text = VtNative.copy(tab) ?: return
        clipboard.setPrimaryClip(ClipData.newPlainText(context.getString(R.string.app_name), text))
        clearSelection()
    }

    private fun pasteClipboard() {
        val clip = clipboard.primaryClip?.takeIf { it.itemCount > 0 } ?: return
        val text = clip.getItemAt(0).coerceToText(context)?.toString()
        if (!text.isNullOrEmpty()) paste(text)
    }

    /**
     * Pastes [text] as the program's modes ask; text that could run commands at once (line breaks
     * without bracketed paste) is pasted only after the user agrees.
     */
    private fun paste(text: String) {
        if (tab == 0L) return
        val bytes = VtNative.paste(tab, text, false)
        if (bytes != null) {
            input(bytes)
            return
        }
        val preview = if (text.length > PASTE_PREVIEW) text.take(PASTE_PREVIEW) + "…" else text
        MaterialAlertDialogBuilder(context)
            .setTitle(R.string.paste_unsafe_title)
            .setMessage(context.getString(R.string.paste_unsafe, preview))
            .setPositiveButton(android.R.string.paste) { _, _ ->
                if (tab != 0L) input(VtNative.paste(tab, text, true))
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    // ---- Selection ----

    private fun startDrag(kind: Int, x: Float, y: Float) {
        drag = kind
        menu?.finish()
        // The finger stays at the same offset from the dragged end's cell centre
        val (cx, cy) = when (kind) {
            DRAG_START -> selStartX to selStartY
            DRAG_END -> selEndX to selEndY
            else -> -1 to -1
        }
        dragOffsetX = if (cx < 0) 0f else (cx + 0.5f) * cellWidth - x
        dragOffsetY = if (cy < 0) 0f else (cy + 0.5f) * cellHeight - y
        dragCellX = -1
        dragCellY = -1
        parent?.requestDisallowInterceptTouchEvent(true)
    }

    private fun dragTo(x: Float, y: Float) {
        val px = x + dragOffsetX
        val py = y + dragOffsetY
        val cx = floor(px / cellWidth).toInt().coerceIn(0, cols - 1)
        val edge = when {
            py < 0 -> -1
            py >= rows * cellHeight -> 1
            else -> 0
        }
        if (edge != autoscroll) {
            autoscroll = edge
            removeCallbacks(autoscrollTick)
            if (edge != 0) postDelayed(autoscrollTick, AUTOSCROLL_MS)
        }
        autoscrollX = cx
        val cy = floor(py / cellHeight).toInt().coerceIn(0, rows - 1)
        if (cx == dragCellX && cy == dragCellY) return
        dragCellX = cx
        dragCellY = cy
        if (VtNative.selectTo(tab, cx, cy)) {
            selected = true
            invalidate()
        }
    }

    private fun endDrag() {
        drag = DRAG_NONE
        autoscroll = 0
        removeCallbacks(autoscrollTick)
        menuX = -1f
        menuPending = true
        invalidate()
    }

    /**
     * The floating menu: Open link after a long press on one, Copy with a selection, Paste with
     * text on the clipboard, Select all.
     */
    private fun showMenu() {
        menu?.finish()
        if (tab == 0L || (!selected && menuX < 0)) return
        menu = startActionMode(
            object : ActionMode.Callback2() {
                override fun onCreateActionMode(mode: ActionMode, m: Menu): Boolean {
                    if (menuLink != null) m.add(Menu.NONE, MENU_OPEN_LINK, 0, R.string.open_link)
                    if (selected) m.add(Menu.NONE, MENU_COPY, 1, android.R.string.copy)
                    if (clipboard.hasPrimaryClip()) m.add(Menu.NONE, MENU_PASTE, 2, android.R.string.paste)
                    m.add(Menu.NONE, MENU_SELECT_ALL, 3, android.R.string.selectAll)
                    return true
                }

                override fun onPrepareActionMode(mode: ActionMode, m: Menu) = false

                override fun onActionItemClicked(mode: ActionMode, item: MenuItem): Boolean {
                    when (item.itemId) {
                        MENU_OPEN_LINK -> menuLink?.let {
                            clearSelection()
                            host.onLink(it)
                        }
                        MENU_COPY -> copySelection()
                        MENU_PASTE -> {
                            mode.finish()
                            pasteClipboard()
                        }
                        MENU_SELECT_ALL -> if (VtNative.selectAll(tab)) {
                            selected = true
                            menuLink = null
                            menuX = -1f
                            menuPending = true
                            mode.finish()
                            invalidate()
                        }
                    }
                    return true
                }

                override fun onDestroyActionMode(mode: ActionMode) {
                    if (menu === mode) menu = null
                }

                override fun onGetContentRect(mode: ActionMode, view: View, outRect: Rect) {
                    if (menuX >= 0) {
                        outRect.set(menuX.toInt(), menuY.toInt(), menuX.toInt() + 1, menuY.toInt() + 1)
                        return
                    }
                    // The selected rows in the viewport; the top when none is
                    val first = selFrom.indexOfFirst { it >= 0 }
                    val last = selFrom.indexOfLast { it >= 0 }
                    if (first < 0) {
                        outRect.set(0, 0, width, cellHeight)
                        return
                    }
                    val l = if (first == last) selFrom[first] * cellWidth else 0
                    val r = if (first == last) (selTo[last] + 1) * cellWidth else cols * cellWidth
                    outRect.set(l, first * cellHeight, r, (last + 1) * cellHeight)
                }
            },
            ActionMode.TYPE_FLOATING,
        )
    }

    /**
     * Handles at the selection's ends (when they are in the viewport), as text fields have. With
     * gesture navigation, a handle at the screen's edge is kept out of the back gesture, which a
     * drag from there would otherwise start.
     */
    private fun drawHandles(c: Canvas) {
        val excluded = mutableListOf<Rect>()
        if (selected) {
            for (start in booleanArrayOf(true, false)) {
                val (d, b) = handle(start) ?: continue
                d.bounds = b
                d.draw(c)
                excluded += Rect(b).apply { inset(-handleSlop, -handleSlop) }
            }
        }
        if (excluded != handleExclusion) {
            handleExclusion = excluded
            systemGestureExclusionRects = excluded
        }
    }

    /**
     * The drawable of a handle and where it is drawn: below its end's cell, pointing at the cell's
     * left edge (start) or right edge (end), as Android's own text handles do. A handle that would
     * stick out of the view at the left (start) or right (end) edge is drawn mirrored, the other
     * end's drawable on the other side of the same point, so a selection from the first column or
     * to the last still has its handle on the screen.
     */
    private fun handle(start: Boolean): Pair<Drawable, Rect>? {
        val x = if (start) selStartX else selEndX
        val y = if (start) selStartY else selEndY
        if (x < 0 || y < 0 || x >= cols || y >= rows) return null
        val edge = if (start) {
            x * cellWidth
        } else {
            (x + if ((attrs[y * cols + x] shr 12) and 3 == WIDE) 2 else 1) * cellWidth
        }
        val left = { d: Drawable, leftHandle: Boolean ->
            // A left handle's point is at three quarters of its width, a right one's at a quarter
            if (leftHandle) edge - d.intrinsicWidth * 3 / 4 else edge - d.intrinsicWidth / 4
        }
        val own = (if (start) handleStart else handleEnd) ?: return null
        val other = if (start) handleEnd else handleStart
        var d = own
        var l = left(own, start)
        if (other != null && (if (start) l < 0 else l + own.intrinsicWidth > width)) {
            d = other
            l = left(other, !start)
        }
        val top = (y + 1) * cellHeight
        return d to Rect(l, top, l + d.intrinsicWidth, top + d.intrinsicHeight)
    }

    /** The handle under a touch, with some slop: DRAG_START, DRAG_END or DRAG_NONE. */
    private fun handleAt(x: Float, y: Float): Int {
        if (!selected) return DRAG_NONE
        for (start in booleanArrayOf(false, true)) {
            val b = handle(start)?.second ?: continue
            b.inset(-handleSlop, -handleSlop)
            if (b.contains(x.toInt(), y.toInt())) return if (start) DRAG_START else DRAG_END
        }
        return DRAG_NONE
    }

    private fun clearSelection() {
        if (tab != 0L) VtNative.selectClear(tab)
        selected = false
        menuLink = null
        menu?.finish()
        invalidate()
    }

    // ---- Gestures ----

    @SuppressLint("ClickableViewAccessibility") // Taps open the keyboard; selection has its own menu
    override fun onTouchEvent(event: MotionEvent): Boolean {
        // A touch stops a fling
        if (event.actionMasked == MotionEvent.ACTION_DOWN) {
            stoppedFling = !scroller.isFinished
            scroller.forceFinished(true)
        }
        // A mouse's buttons and drags go to a program tracking it as they are, from the press on
        val press = event.actionMasked == MotionEvent.ACTION_DOWN && mouseTracking && drag == DRAG_NONE
        if (tab != 0L && event.isFromSource(InputDevice.SOURCE_MOUSE) && (press || mouseButton != 0)) {
            mouseButtons(event)
            return true
        }
        // A handle drag owns the whole gesture
        if (event.actionMasked == MotionEvent.ACTION_DOWN && tab != 0L) {
            val handle = handleAt(event.x, event.y)
            if (handle != DRAG_NONE && VtNative.selectGrab(tab, handle == DRAG_END)) {
                startDrag(handle, event.x, event.y)
                return true
            }
        }
        if (drag == DRAG_START || drag == DRAG_END) {
            when (event.actionMasked) {
                MotionEvent.ACTION_MOVE -> dragTo(event.x, event.y)
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> endDrag()
            }
            return true
        }
        scaler.onTouchEvent(event)
        if (!scaler.isInProgress) gestures.onTouchEvent(event)
        when (drag) {
            // After a long press selected a word, moving on extends the selection
            DRAG_WORD -> when (event.actionMasked) {
                MotionEvent.ACTION_MOVE -> dragTo(event.x, event.y)
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> endDrag()
            }
            // A long press in mouse mode: moving on drags for the program, lifting keeps the word
            DRAG_PRESS -> when (event.actionMasked) {
                MotionEvent.ACTION_MOVE -> if (hypot(event.x - pressX, event.y - pressY) > touchSlop) startMouseDrag(event)
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> endPress()
            }
            DRAG_MOUSE -> when (event.actionMasked) {
                MotionEvent.ACTION_MOVE -> send(VtNative.mouse(tab, MOUSE_MOTION, MOUSE_LEFT, 0, event.x, event.y, true))
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                    drag = DRAG_NONE
                    send(VtNative.mouse(tab, MOUSE_RELEASE, MOUSE_LEFT, 0, event.x, event.y, false))
                }
            }
        }
        return true
    }

    /**
     * A mouse (or touchpad) button pressed, dragged and released, for a program tracking the
     * mouse; the first button pressed is the one reported until it is released.
     */
    private fun mouseButtons(e: MotionEvent) {
        val mods = VtKeys.mods(e.metaState)
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                if (selected || menu != null) clearSelection()
                requestFocus()
                mouseButton = when {
                    e.buttonState and MotionEvent.BUTTON_SECONDARY != 0 -> MOUSE_RIGHT
                    e.buttonState and MotionEvent.BUTTON_TERTIARY != 0 -> MOUSE_MIDDLE
                    else -> MOUSE_LEFT // Also a touchpad's tap, which holds no button
                }
                send(VtNative.mouse(tab, MOUSE_PRESS, mouseButton, mods, e.x, e.y, true))
            }
            MotionEvent.ACTION_MOVE -> if (mouseButton != 0) {
                send(VtNative.mouse(tab, MOUSE_MOTION, mouseButton, mods, e.x, e.y, true))
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> if (mouseButton != 0) {
                send(VtNative.mouse(tab, MOUSE_RELEASE, mouseButton, mods, e.x, e.y, false))
                mouseButton = 0
            }
        }
    }

    /** The finger moved on after a long press in mouse mode: the program gets a left-button drag. */
    private fun startMouseDrag(e: MotionEvent) {
        if (selected) clearSelection()
        menuLink = null
        drag = DRAG_MOUSE
        send(VtNative.mouse(tab, MOUSE_PRESS, MOUSE_LEFT, 0, pressX, pressY, true))
        send(VtNative.mouse(tab, MOUSE_MOTION, MOUSE_LEFT, 0, e.x, e.y, true))
    }

    /** Lifted after a long press in mouse mode without moving: the word's menu, as without mouse mode. */
    private fun endPress() {
        if (selected) {
            endDrag()
            return
        }
        drag = DRAG_NONE
        menuX = pressX
        menuY = pressY
        showMenu()
    }

    /** Taps without a delay: a double tap's first tap acts at once, its second selects the word. */
    private val gestures = GestureDetector(
        context,
        object : GestureDetector.OnGestureListener {
            override fun onDown(e: MotionEvent) = true

            override fun onShowPress(e: MotionEvent) = Unit

            override fun onSingleTapUp(e: MotionEvent): Boolean {
                // Only stops the fling, as a tap on a flinging ScrollView does
                if (stoppedFling) return true
                val second = e.downTime - lastTapTime <= ViewConfiguration.getDoubleTapTimeout() &&
                    hypot(e.x - lastTapX, e.y - lastTapY) <= doubleTapSlop
                // A third tap starts over
                lastTapTime = if (second) 0L else e.eventTime
                lastTapX = e.x
                lastTapY = e.y
                // In mouse mode both taps are clicks, for the program's own double click
                if (second && !mouseTracking && tab != 0L && VtNative.selectWord(tab, cellX(e.x), cellY(e.y))) {
                    selected = true
                    menuLink = null
                    menuX = -1f
                    menuPending = true
                    invalidate()
                    return true
                }
                if (selected || menu != null) {
                    clearSelection()
                    return true
                }
                // A link opens instead of the keyboard (in mouse mode the tap is the program's,
                // and the long press's menu opens links); a tap after it is a first tap again
                val link = if (tab != 0L && !mouseTracking) linkAt(e.x, e.y) else null
                if (link != null) {
                    lastTapTime = 0L
                    host.onLink(link)
                    return true
                }
                if (tab != 0L && mouseTracking) click(e)
                requestFocus()
                context.getSystemService(InputMethodManager::class.java).showSoftInput(this@VtView, 0)
                return true
            }

            override fun onScroll(e1: MotionEvent?, e2: MotionEvent, distanceX: Float, distanceY: Float): Boolean {
                if (tab == 0L) return false
                scrollRemainder += distanceY
                val n = (scrollRemainder / cellHeight).toInt()
                if (n != 0) {
                    scrollRemainder -= n * cellHeight
                    scrollBy(n, e2.x, e2.y)
                }
                return true
            }

            override fun onLongPress(e: MotionEvent) {
                if (tab == 0L || scaler.isInProgress) return
                performHapticFeedback(HapticFeedbackConstants.LONG_PRESS)
                val word = VtNative.selectWord(tab, cellX(e.x), cellY(e.y))
                menuLink = linkAt(e.x, e.y)
                if (mouseTracking) {
                    // Whether this selects the word or drags for the program waits for the finger
                    selected = word
                    startDrag(DRAG_PRESS, e.x, e.y)
                    pressX = e.x
                    pressY = e.y
                    invalidate()
                } else if (word) {
                    selected = true
                    startDrag(DRAG_WORD, e.x, e.y)
                    invalidate()
                } else {
                    // Nothing to select there: only Paste and Select all
                    selected = false
                    invalidate()
                    menuX = e.x
                    menuY = e.y
                    showMenu()
                }
            }

            override fun onFling(e1: MotionEvent?, e2: MotionEvent, velocityX: Float, velocityY: Float): Boolean {
                if (tab == 0L || drag != DRAG_NONE || abs(velocityY) < minFling) return false
                fling(e2.x, e2.y, -velocityY.roundToInt())
                return true
            }
        },
    )

    private val scaler = ScaleGestureDetector(
        context,
        object : ScaleGestureDetector.SimpleOnScaleGestureListener() {
            override fun onScale(detector: ScaleGestureDetector): Boolean {
                scaleRemainder *= detector.scaleFactor
                if (scaleRemainder > 1.1f || scaleRemainder < 0.9f) {
                    zoom(if (scaleRemainder > 1f) 1 else -1)
                    scaleRemainder = 1f
                }
                return true
            }
        },
    )

    /**
     * A mouse wheel (or touchpad scroll) scrolls like a drag; a mouse moving with no button held
     * goes to a program tracking all motion (mode 1003).
     */
    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (tab != 0L && event.isFromSource(InputDevice.SOURCE_CLASS_POINTER)) {
            when (event.actionMasked) {
                MotionEvent.ACTION_SCROLL -> {
                    val v = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
                    if (v != 0f) {
                        scrollBy(if (v > 0) -WHEEL_ROWS else WHEEL_ROWS, event.x, event.y)
                        return true
                    }
                }
                MotionEvent.ACTION_HOVER_MOVE -> if (mouseTracking) {
                    send(VtNative.mouse(tab, MOUSE_MOTION, MOUSE_NONE, VtKeys.mods(event.metaState), event.x, event.y, false))
                    return true
                }
            }
        }
        return super.onGenericMotionEvent(event)
    }

    /** A left click at a tap, for a program tracking the mouse. */
    private fun click(e: MotionEvent) {
        send(VtNative.mouse(tab, MOUSE_PRESS, MOUSE_LEFT, 0, e.x, e.y, true))
        send(VtNative.mouse(tab, MOUSE_RELEASE, MOUSE_LEFT, 0, e.x, e.y, false))
    }

    /**
     * The link at a touch that may be opened: one with a scheme in [LINK_SCHEMES] (a program's
     * OSC 8 link can name any, other apps' deep links among them); null for none.
     */
    private fun linkAt(x: Float, y: Float): String? {
        val link = VtNative.link(tab, cellX(x), cellY(y)) ?: return null
        return link.takeIf { link.toUri().scheme?.lowercase(Locale.ROOT) in LINK_SCHEMES }
    }

    /** Scrolls by [n] rows (positive: towards the newest output), the way [scrollWay] says. */
    private fun scrollBy(n: Int, x: Float, y: Float) {
        when (scrollWay()) {
            WAY_WHEEL -> repeat(abs(n)) {
                val button = if (n > 0) MOUSE_WHEEL_DOWN else MOUSE_WHEEL_UP
                send(VtNative.mouse(tab, MOUSE_PRESS, button, 0, x, y, false), scrolling = true)
            }
            WAY_ARROWS -> {
                val arrow = VtKeys.of(if (n > 0) KeyEvent.KEYCODE_DPAD_DOWN else KeyEvent.KEYCODE_DPAD_UP)
                repeat(abs(n)) { send(VtNative.key(tab, arrow, 0, 0, VtKeys.ACTION_PRESS, null, 0), scrolling = true) }
            }
            else -> {
                VtNative.scroll(tab, n)
                awakenScrollBars()
                invalidate()
            }
        }
    }

    /**
     * How scrolling goes now: wheel events for a program tracking the mouse, arrow keys on the
     * alternate screen with alternate scroll, else the viewport.
     */
    private fun scrollWay() = when {
        mouseTracking -> WAY_WHEEL
        altScreen && altScroll -> WAY_ARROWS
        else -> WAY_VIEWPORT
    }

    /**
     * Goes on scrolling after a drag at [velocity] pixels a second (positive: towards the newest
     * output), slowing down as a ScrollView does ([computeScroll] takes the rows): the viewport up
     * to either end, wheel events and arrow keys for at most a screen of rows.
     */
    private fun fling(x: Float, y: Float, velocity: Int) {
        flingWay = scrollWay()
        flingX = x
        flingY = y
        flingLast = 0
        val reach = (if (flingWay == WAY_VIEWPORT) scrollTotal else rows) * cellHeight
        scroller.fling(0, 0, 0, velocity, 0, 0, -reach, reach)
        postInvalidateOnAnimation()
    }

    /**
     * A fling's next rows, before each frame while it lasts. It stops when the way to scroll
     * changes (a program turning mouse tracking on, the alternate screen) or the viewport reaches
     * the end it moves to, as of the last frame.
     */
    override fun computeScroll() {
        if (!scroller.computeScrollOffset()) return
        val delta = scroller.currY - flingLast
        flingLast = scroller.currY
        val end = flingWay == WAY_VIEWPORT && (if (delta < 0) scrollOffset == 0 else delta > 0 && atBottom)
        if (tab == 0L || scrollWay() != flingWay || end) {
            scroller.forceFinished(true)
            return
        }
        scrollRemainder += delta
        val n = (scrollRemainder / cellHeight).toInt()
        if (n != 0) {
            scrollRemainder -= n * cellHeight
            scrollBy(n, flingX, flingY)
        }
        postInvalidateOnAnimation()
    }

    private fun cellX(x: Float) = floor(x / cellWidth).toInt().coerceIn(0, cols - 1)

    private fun cellY(y: Float) = floor(y / cellHeight).toInt().coerceIn(0, rows - 1)

    private companion object {
        // Cell attributes (vt.c)
        const val BOLD = 1
        const val ITALIC = 2
        const val FAINT = 1 shl 2
        const val INVERSE = 1 shl 4
        const val INVISIBLE = 1 shl 5
        const val STRIKE = 1 shl 6
        const val OVERLINE = 1 shl 7
        const val DECORATIONS = STRIKE or OVERLINE or (7 shl 8)
        const val NARROW = 0
        const val WIDE = 1
        const val SPACER_TAIL = 2
        const val SPACER_HEAD = 3

        // GhosttySgrUnderline
        const val UNDERLINE_DOUBLE = 2
        const val UNDERLINE_CURLY = 3
        const val UNDERLINE_DOTTED = 4
        const val UNDERLINE_DASHED = 5

        // GhosttyRenderStateCursorVisualStyle
        const val CURSOR_BAR = 0
        const val CURSOR_UNDERLINE = 2
        const val CURSOR_HOLLOW = 3

        // The frame format (vt.c)
        const val HEADER_BYTES = 48
        const val ROW_HEADER_BYTES = 12
        const val FLAG_MOUSE = 1
        const val FLAG_ALT_SCREEN = 1 shl 1
        const val FLAG_ALT_SCROLL = 1 shl 2
        const val FLAG_SELECTION = 1 shl 4

        // VtNative.mouse
        const val MOUSE_PRESS = 0
        const val MOUSE_RELEASE = 1
        const val MOUSE_MOTION = 2
        const val MOUSE_NONE = 0
        const val MOUSE_LEFT = 1
        const val MOUSE_RIGHT = 2
        const val MOUSE_MIDDLE = 3
        const val MOUSE_WHEEL_UP = 4
        const val MOUSE_WHEEL_DOWN = 5

        /** Rows per mouse wheel notch. */
        const val WHEEL_ROWS = 3

        /** A blinking cursor's half period, as Android's text fields. */
        const val BLINK_MS = 500L

        /** The shortest time between two vibrations of the bell. */
        const val BELL_MS = 100L

        // How scrolling goes (scrollWay)
        const val WAY_VIEWPORT = 0
        const val WAY_WHEEL = 1
        const val WAY_ARROWS = 2

        const val DRAG_NONE = 0
        const val DRAG_WORD = 1
        const val DRAG_START = 2
        const val DRAG_END = 3
        const val DRAG_PRESS = 4
        const val DRAG_MOUSE = 5
        const val AUTOSCROLL_MS = 50L

        const val MENU_COPY = 1
        const val MENU_PASTE = 2
        const val MENU_SELECT_ALL = 3
        const val MENU_OPEN_LINK = 4

        /**
         * The schemes a tapped link may have: web pages, and from an OSC 8 link mail and guest
         * files (`ls --hyperlink`).
         */
        val LINK_SCHEMES = setOf("http", "https", "mailto", "file")

        /** Characters of a paste shown when asking whether to paste it. */
        const val PASTE_PREVIEW = 300

        /** The selection's highlight, over cell backgrounds and under text. */
        const val SELECTION = 0x604A90E2
    }
}
