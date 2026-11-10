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
import android.os.Build
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
import android.view.PointerIcon
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
import androidx.core.graphics.withTranslation
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
 * The terminal view on libghostty-vt (the android/vtview map): draws the [VtSession] attached to
 * it with JetBrains Mono on a fixed cell grid. Sessions belong to [TerminalService] and keep parsing output while no view shows them;
 * [attach] switches the view to another tab.
 *
 * Each frame takes the rows that changed ([VtNative.frame]) into a mirror of the screen and paints
 * them into a bitmap the size of the grid; the cursor and the input method's composing text are
 * drawn over it. Glyphs are placed cell by cell: runs of ASCII share one draw call (the font size
 * is chosen so their advance is exactly one cell), every other glyph is centred in its one or two
 * cells, enlarged evenly to fill two cells when it is wide (the setting, for CJK fallback glyphs
 * narrower than two cells) or shrunk evenly when it is wider, never stretched. Box drawing, block
 * elements and the Powerline triangles are drawn as shapes ([VtGlyphs]). Images programs show
 * (the Kitty graphics protocol) are painted into the rows they reach, below the backgrounds, below
 * the text or above it as the frame places them; their pixels are copied once per change.
 *
 * Touch: a tap opens the keyboard (and clicks, when the program tracks the mouse); a drag scrolls
 * the scrollback, or sends wheel events to a program tracking the mouse, or arrow keys on the
 * alternate screen (alternate scroll, mode 1007), and a fling goes on scrolling the same way; while
 * the scrollback scrolls, a scroll bar like a ScrollView's shows where the viewport is in it. A long
 * press or a double tap selects a word and a triple tap a line, and dragging on after a long press
 * extends the selection, as do its two handles; a floating menu copies, pastes, selects all and
 * turns the selection into a rectangle and back.
 * A mouse or touchpad selects as in a desktop terminal (Ghostty's): dragging selects by cells (a
 * rectangle with Ctrl+Alt), a double click a word and a triple click a line, Shift+click extends
 * the selection, Ctrl+click opens a link and a right-click brings up the menu; with Shift it does
 * so while the program tracks the mouse too.
 * The selection is the terminal's, so it stays on its text through output and scrolling.
 *
 * A search ([search], from the activity's search bar) marks its matches in yellow, the selected
 * one in orange, as Ghostty does; libghostty-vt finds them and frames carry the marks.
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

        /**
         * The search's count changed ([search]): [total] matches so far, the selected one [selected]
         * counted from the newest (1 for the newest), 0 when none is selected.
         */
        fun onSearchCount(total: Int, selected: Int)
    }

    /** Where a frame draws an image, or part of it (vt.c's image placements). */
    private class Placement {
        var id = 0
        var layer = 0
        var generation = 0L
        var imageWidth = 0
        var imageHeight = 0

        /** In the grid's pixels; it can reach past the grid. */
        val dst = Rect()

        /** In the image's pixels. */
        val src = Rect()
    }

    /** An image's bitmap and the [generation] of its pixels it holds, -1 when copying them failed. */
    private class Picture(val bitmap: Bitmap, var generation: Long)

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

    /** The lock drawn for the cursor while a password is read ([drawLock]). */
    private val lockPaint = Paint(Paint.ANTI_ALIAS_FLAG)
    private val shackle = Path()

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

    /**
     * While the cursor is a lock (the console reads a password) and the window shows, the console
     * is asked again every [PASSWORD_CHECK_MS] whether it still does: a program may turn echo back
     * on without printing anything after (sudo prints its line break first), which no frame tells.
     */
    private var checkingPassword = false
    private val passwordCheck = object : Runnable {
        override fun run() {
            if (tab != 0L && cursorStyle == CURSOR_LOCK && VtNative.passwordInput(tab)) {
                postDelayed(this, PASSWORD_CHECK_MS)
            } else {
                checkingPassword = false
                invalidate()
            }
        }
    }

    /**
     * The input method was last told that what is typed is a password ([onCreateInputConnection]):
     * while the cursor is a lock, as Ghostty turns macOS's secure input on at password prompts.
     */
    private var imePassword = false

    /** When the bell last vibrated ([bell]). */
    private var lastBell = 0L

    // The scrollable area (scrollback and screen) in rows, the viewport's first row in it and the
    // rows it shows, for the scroll bar
    private var scrollTotal = 0
    private var scrollOffset = 0
    private var scrollLen = 0

    /**
     * The mouse pointer: the shape the program asked for (OSC 22, a `GhosttyMouseShape`; an
     * I-beam by default), the cell a hovering mouse is over (-1 for none), whether that is a link
     * and whether Ctrl (or Meta) is held, with which a click opens it and the pointer shows a
     * hand, as in Ghostty on Linux; and the `PointerIcon` type set on the view (-1 for none yet).
     */
    private var pointerShape = POINTER_TEXT
    private var hoverCell = -1
    private var hoverLink = false
    private var hoverCtrl = false
    private var pointerSet = -1

    // Modes and the selection, as of the last frame (vt.c's header flags)
    private var mouseTracking = false
    private var altScroll = false

    /** The alternate screen is shown (a full-screen program), as of the last frame. */
    var altScreen = false
        private set

    /** Whether the terminal has a selection; also set at once by the view's own changes. */
    private var selected = false

    /** Whether the selection is a rectangle, as of the last frame. */
    private var rectangle = false

    /** Whether a search runs in the tab shown ([search]), and its count as last reported. */
    private var searching = false
    private var searchTotal = 0
    private var searchSelected = 0

    /**
     * The search's matches along the scroll bar ([drawMatchMarks]): the scroll bar rows they start
     * on, newest first, and the selected match's (-1 for none), as taken ([takeMatchRows]) at the
     * count, selected match and time noted with them; [matchRowsTotal] -1 asks for them again.
     */
    private var matchRows = IntArray(0)
    private var matchSelectedRow = -1
    private var matchRowsTotal = -1
    private var matchRowsSelected = 0
    private var matchRowsAt = 0L

    /** The scrollable rows of the frame before, to tell when rows left the scrollback. */
    private var lastScrollTotal = 0

    /** The height of a match's mark along the scroll bar. */
    private val matchMarkHeight = (2 * resources.displayMetrics.density).roundToInt().coerceAtLeast(1)

    /**
     * Where the viewport was before a step to another match ([searchStep]) or prompt
     * ([jumpToPrompt]), -1 when none is under way: the next frame shows the scroll bar if the step
     * scrolled.
     */
    private var stepFrom = -1

    /** The selection's first and last cell in the viewport, -1 when outside it. */
    private var selStartX = -1
    private var selStartY = -1
    private var selEndX = -1
    private var selEndY = -1

    /** Each row's selected columns (inclusive), -1 when none. */
    private var selFrom = IntArray(0)
    private var selTo = IntArray(0)

    /**
     * Kitty images: the last frame's placements in drawing order (the first [imageCount], from a
     * pool kept across frames), painted into the rows they reach, and the bitmaps of the images
     * they show by id, dropped once no placement shows them.
     */
    private val placements = ArrayList<Placement>()
    private var imageCount = 0
    private val pictures = HashMap<Int, Picture>()
    private val imagePaint = Paint(Paint.FILTER_BITMAP_FLAG)

    /** Draws again for the next frame of an animated image. */
    private val animationTick = Runnable { invalidate() }

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
     * Pixels the grid is drawn above its place, from 0 to less than a row: the viewport scrolls
     * by pixels, hiding part of its first row and showing as much of the row after it (the frame's
     * overscan row). Only while scrolling moves the viewport ([WAY_VIEWPORT]) and it is not at the
     * bottom; scrolling comes to rest on a whole row ([settle]). Touches are taken to cells with
     * it ([cellY]).
     */
    private var scrollPx = 0

    /** What a drag scrolled the viewport by beyond whole pixels. */
    private var pixelRemainder = 0f

    /**
     * A fling: pixels scrolled over time, the viewport's as a drag's are and in rows otherwise,
     * the way scrolling went when it started ([scrollWay]) and with wheel events at [flingX],
     * [flingY]; [flingLast] is the position taken so far. It also brings the viewport to rest on a
     * whole row ([settling]).
     */
    private val scroller = OverScroller(context)
    private var flingWay = WAY_VIEWPORT
    private var flingX = 0f
    private var flingY = 0f
    private var flingLast = 0

    /** The scroller only brings the viewport to a whole row ([settle]), which a touch does not count as a fling. */
    private var settling = false

    /** The touch under way stopped a fling, so its tap does nothing else. */
    private var stoppedFling = false

    // Selection drags: by words from a long-pressed word or a double tap's, by lines from a triple
    // tap's line (libghostty-vt's selection gesture), or by a handle; in mouse mode a long press
    // held on may instead become a drag for the program
    private var drag = DRAG_NONE
    private var dragOffsetX = 0f
    private var dragOffsetY = 0f
    private var dragCellX = -1
    private var dragCellY = -1

    /**
     * Where a long press in mouse mode was (the program's drag starts there if the finger moves
     * on), or the press of a double or triple tap (a drag from there extends its selection).
     */
    private var pressX = 0f
    private var pressY = 0f

    /** The finger pressing a double or triple tap has moved past the touch slop: it drags. */
    private var pressMoved = false

    /** The button of a mouse (not a finger) held down for a program tracking the mouse, or 0. */
    private var mouseButton = 0

    /**
     * The button of a mouse (or touchpad) held down to select, as in a desktop terminal (without
     * mouse tracking, or with Shift), or 0: the left one drags the selection ([DRAG_POINTER]), the
     * right one brought up the menu, the middle one does nothing.
     */
    private var pointerButton = 0

    /** When the last left press to select came (uptime), after which Shift+press extends. */
    private var lastPointerPress = 0L

    /** The left button's drag makes a rectangle: Ctrl+Alt held, as in Ghostty on Linux. */
    private var pointerRectangle = false

    /** The selection was made with a mouse, so no handles show, as in Android's text fields. */
    private var pointerSelection = false

    /**
     * The last tap's lift and how many taps came in a row before it (each within the double-tap
     * timeout and slop of the one before), to tell a double and a triple tap.
     */
    private var lastTapTime = 0L
    private var lastTapX = 0f
    private var lastTapY = 0f
    private var taps = 0
    private val touchSlop = ViewConfiguration.get(context).scaledTouchSlop
    private val doubleTapSlop = ViewConfiguration.get(context).scaledDoubleTapSlop
    private val minFling = ViewConfiguration.get(context).scaledMinimumFlingVelocity

    /** Rows the viewport scrolls by while a drag is held above (-1) or below (1) the grid. */
    private var autoscroll = 0
    private var autoscrollX = 0
    private val autoscrollTick = object : Runnable {
        override fun run() {
            if (autoscroll == 0 || tab == 0L) return
            // By whole rows, the grid in its place
            scrollPx = 0
            VtNative.scroll(tab, autoscroll)
            val y = if (autoscroll < 0) 0 else rows - 1
            extendTo(autoscrollX, y, (autoscrollX + 0.5f) * cellWidth, (y + 0.5f) * cellHeight)
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

    /**
     * The long press that began the selection was in a command's output ([VtNative.outputAt]),
     * which the menu offers to select.
     */
    private var menuOutput = false

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
     * again, and a selection drag, menu, fling or search of the previous tab ends.
     */
    fun attach(s: VtSession?) {
        if (s === session) return
        endSearch()
        session?.let { if (it.view === this) it.view = null }
        menu?.finish()
        if (drag == DRAG_WORD || drag == DRAG_TAPS || drag == DRAG_PRESS || drag == DRAG_POINTER) {
            VtNative.gestureEnd(tab)
        }
        drag = DRAG_NONE
        autoscroll = 0
        removeCallbacks(autoscrollTick)
        stopPasswordCheck()
        if (imePassword) {
            // The other tab's frames say whether it reads one too
            imePassword = false
            restartInput()
        }
        hoverCell = -1
        hoverLink = false
        scroller.forceFinished(true)
        scrollPx = 0
        selected = false
        scrollTotal = 0
        scrollOffset = 0
        scrollLen = 0
        dropImages()
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

    /** Forgets the images of the tab shown, which another tab's image ids do not name. */
    private fun dropImages() {
        removeCallbacks(animationTick)
        imageCount = 0
        for (p in pictures.values) p.bitmap.recycle()
        pictures.clear()
    }

    /** Stops asking whether a password is still read ([passwordCheck]). */
    private fun stopPasswordCheck() {
        removeCallbacks(passwordCheck)
        checkingPassword = false
    }

    /**
     * Has the input method take a new connection ([onCreateInputConnection]), for whether a
     * password is typed; nothing happens while the view is not the one it serves.
     */
    private fun restartInput() {
        context.getSystemService(InputMethodManager::class.java).restartInput(this)
    }

    /** The session's shell started: take the whole screen. */
    fun onStarted() {
        full = true
        invalidate()
    }

    /**
     * Searches the tab shown for [needle] as the search bar's text changes (empty: nothing); the
     * frames that follow mark the matches and report their count ([Host.onSearchCount]).
     */
    fun search(needle: String) {
        if (tab == 0L) return
        searching = true
        VtNative.search(tab, needle)
        // Another needle can find as many matches elsewhere
        matchRowsTotal = -1
        invalidate()
    }

    /**
     * Selects the next match above ([older]) or below, going round, and scrolls to it unless it
     * is shown already.
     */
    fun searchStep(older: Boolean) {
        if (tab == 0L || !searching || !VtNative.searchSelect(tab, older)) return
        scroller.forceFinished(true)
        scrollPx = 0
        stepFrom = scrollOffset
        invalidate()
    }

    /** Ends the search, taking its marks away. */
    fun endSearch() {
        if (!searching) return
        searching = false
        searchTotal = 0
        searchSelected = 0
        matchRows = IntArray(0)
        matchSelectedRow = -1
        matchRowsTotal = -1
        if (tab != 0L) VtNative.searchEnd(tab)
        invalidate()
    }

    /**
     * Scrolls to the prompt [delta] prompts above (negative) or below the viewport's top, as the
     * shell's prompt marks (OSC 133) find them; false on the alternate screen, which has none, so
     * the key goes to the program there.
     */
    fun jumpToPrompt(delta: Int): Boolean {
        if (tab == 0L || altScreen) return false
        scroller.forceFinished(true)
        scrollPx = 0
        stepFrom = scrollOffset
        VtNative.jumpPrompt(tab, delta)
        invalidate()
        return true
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

    /**
     * Fits the grid to the view. The mirror and the bitmap hold one row more than the grid, the
     * frame's overscan row after the viewport, which shows in part while the grid is drawn
     * shifted up by [scrollPx].
     */
    private fun regrid() {
        if (width == 0 || height == 0) return
        val c = max(1, width / cellWidth)
        val r = max(1, height / cellHeight)
        val bw = c * cellWidth
        val bh = (r + 1) * cellHeight
        if (c == cols && r == rows && bitmap?.width == bw && bitmap?.height == bh) return
        cols = c
        rows = r
        scrollPx = 0
        val n = c * (r + 1)
        cp = IntArray(n)
        fg = IntArray(n)
        bg = IntArray(n)
        attrs = IntArray(n)
        clusters = arrayOfNulls(n)
        selFrom = IntArray(r + 1) { -1 }
        selTo = IntArray(r + 1) { -1 }
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
        // The grid, shifted up by part of a row while the viewport scrolls; the margin below the
        // last whole row keeps the background
        val s = scrollPx.toFloat()
        c.withClip(0, 0, cols * cellWidth, rows * cellHeight) {
            c.drawBitmap(bmp, 0f, -s, null)
        }
        c.withTranslation(0f, -s) {
            drawCursor(c)
            drawPreedit(c)
        }
        drawMatchMarks(c)
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
        if (cursorStyle == CURSOR_LOCK && !checkingPassword && windowVisibility == VISIBLE) {
            checkingPassword = true
            postDelayed(passwordCheck, PASSWORD_CHECK_MS)
        }
        // The input method takes what is typed at a password prompt as a password: no suggestions,
        // nothing learnt. Posted, as restarting the input calls into the input method's service
        if ((cursorStyle == CURSOR_LOCK) != imePassword) {
            imePassword = !imePassword
            post(::restartInput)
        }
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
        rectangle = flags and FLAG_RECTANGLE != 0
        if (hadSelection && !selected && menuX < 0) menu?.finish()
        selStartX = coordinate(b, 28)
        selStartY = coordinate(b, 30)
        selEndX = coordinate(b, 32)
        selEndY = coordinate(b, 34)
        scrollTotal = b.getInt(36)
        scrollOffset = b.getInt(40)
        scrollLen = b.getInt(44)
        // Without a row after the viewport (at the bottom), or once scrolling no longer moves it
        // (mouse tracking, the alternate screen), the grid is in its place
        if (b.get(58).toInt() == 0 || scrollWay() != WAY_VIEWPORT) scrollPx = 0
        pointerShape = b.get(59).toInt()
        updatePointer()
        if (stepFrom >= 0) {
            if (scrollOffset != stepFrom) awakenScrollBars()
            stepFrom = -1
        }
        if (searching) {
            // Searching a long scrollback takes more than one frame
            if (flags and FLAG_SEARCH_PENDING != 0) postInvalidateOnAnimation()
            val total = b.getInt(48)
            val selectedMatch = b.getInt(52)
            if (total != searchTotal || selectedMatch != searchSelected) {
                searchTotal = total
                searchSelected = selectedMatch
                host.onSearchCount(total, selectedMatch)
            }
            // The matches' rows move only when rows leave the scrollback (at its limit, cleared)
            // or the search changes; output goes on adding rows below them, taken again now and
            // then in case as many rows left as came
            val now = SystemClock.uptimeMillis()
            if (total != matchRowsTotal || selectedMatch != matchRowsSelected || scrollTotal < lastScrollTotal ||
                (b.get(4).toInt() != DIRTY_NONE && now - matchRowsAt >= MATCH_ROWS_MS)
            ) {
                takeMatchRows(total, selectedMatch, now)
            }
        }
        lastScrollTotal = scrollTotal
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
        val images = b.getShort(56).toInt() and 0xffff
        takeImages(b, HEADER_BYTES, images, b.get(4).toInt() == DIRTY_FULL)
        removeCallbacks(animationTick)
        val animationMs = b.getInt(60)
        if (animationMs > 0) postDelayed(animationTick, animationMs.toLong())
        frameRows += count
        val target = canvas ?: return
        var pos = HEADER_BYTES + images * IMAGE_BYTES
        repeat(count) {
            val y = b.getShort(pos).toInt() and 0xffff
            val n = b.getShort(pos + 2).toInt() and 0xffff
            val extra = b.getInt(pos + 4)
            val from = coordinate(b, pos + 8)
            val to = coordinate(b, pos + 10)
            pos += ROW_HEADER_BYTES
            var e = pos + n * 16
            // Up to the overscan row after the viewport (y == rows)
            if (y <= rows) {
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

    /** Takes the rows of the search's matches along the scroll bar, at a count and selected match. */
    private fun takeMatchRows(total: Int, selectedMatch: Int, now: Long) {
        val taken = VtNative.searchRows(tab) ?: return
        matchSelectedRow = taken[0]
        matchRows = taken.copyOfRange(1, taken.size)
        matchRowsTotal = total
        matchRowsSelected = selectedMatch
        matchRowsAt = now
    }

    /** A u16 coordinate of the frame, -1 for 0xffff (none). */
    private fun coordinate(b: ByteBuffer, at: Int): Int =
        (b.getShort(at).toInt() and 0xffff).let { if (it == 0xffff) -1 else it }

    /**
     * Shows a mouse over the view the pointer for its place: a hand over a link a Ctrl+click opens
     * (without mouse tracking), else the shape the program asked for (OSC 22) as the closest of
     * Android's system pointers. Set on the view, so it changes at once, the mouse still or not.
     */
    private fun updatePointer() {
        val type = if (hoverLink && hoverCtrl && !mouseTracking) PointerIcon.TYPE_HAND else pointerType(pointerShape)
        if (type == pointerSet) return
        pointerSet = type
        pointerIcon = PointerIcon.getSystemIcon(context, type)
    }

    /**
     * Takes the frame's [count] image placements at [at] and copies the pixels of the images they
     * show that are not held at their generation yet; bitmaps no placement shows are dropped. New
     * pixels come with a frame that repaints every row ([repaintsAll]) since the placements
     * changed; when they come later (an earlier frame could not get them), the whole screen is
     * asked for again so that the rows they reach show them.
     */
    private fun takeImages(b: ByteBuffer, at: Int, count: Int, repaintsAll: Boolean) {
        while (placements.size < count) placements.add(Placement())
        imageCount = count
        var arrived = false
        for (k in 0 until count) {
            val o = at + k * IMAGE_BYTES
            val p = placements[k]
            p.id = b.getInt(o)
            p.layer = b.get(o + 4).toInt()
            p.generation = b.getLong(o + 8)
            rect(p.dst, b.getInt(o + 16), b.getInt(o + 20), b.getInt(o + 24), b.getInt(o + 28))
            rect(p.src, b.getInt(o + 32), b.getInt(o + 36), b.getInt(o + 40), b.getInt(o + 44))
            p.imageWidth = b.getInt(o + 48)
            p.imageHeight = b.getInt(o + 52)
            val held = pictures[p.id]
            if (held != null && held.generation == p.generation) continue
            if (copyImage(p, held)) arrived = true
        }
        val it = pictures.entries.iterator()
        while (it.hasNext()) {
            val e = it.next()
            if (!shown(e.key)) {
                e.value.bitmap.recycle()
                it.remove()
            }
        }
        if (arrived && !repaintsAll) {
            full = true
            postInvalidateOnAnimation()
        }
    }

    /**
     * Sets [r] to [w] × [h] pixels at ([x], [y]), the sizes read as unsigned (vt.c's u32) and the
     * far edges kept within an Int.
     */
    private fun rect(r: Rect, x: Int, y: Int, w: Int, h: Int) {
        val right = min(x + (w.toLong() and 0xffffffffL), Int.MAX_VALUE.toLong())
        val bottom = min(y + (h.toLong() and 0xffffffffL), Int.MAX_VALUE.toLong())
        r.set(x, y, right.toInt(), bottom.toInt())
    }

    /**
     * Copies the pixels of [p]'s image at its generation into its bitmap, reusing the one [held]
     * for an older generation when the size is the same; false when they cannot be had now (a
     * later frame names a newer generation), or the bitmap cannot be made.
     */
    private fun copyImage(p: Placement, held: Picture?): Boolean {
        if (p.imageWidth <= 0 || p.imageHeight <= 0) return false
        val reuse = held?.bitmap?.takeIf { it.width == p.imageWidth && it.height == p.imageHeight }
        val bitmap = reuse ?: try {
            createBitmap(p.imageWidth, p.imageHeight)
        } catch (_: OutOfMemoryError) {
            return false
        } catch (_: IllegalArgumentException) {
            return false
        }
        if (held != null && held.bitmap !== bitmap) held.bitmap.recycle()
        val copied = VtNative.image(tab, p.id, p.generation, bitmap)
        pictures[p.id] = Picture(bitmap, if (copied) p.generation else -1L)
        return copied
    }

    /** Whether one of the last frame's placements shows image [id]. */
    private fun shown(id: Int): Boolean {
        for (k in 0 until imageCount) if (placements[k].id == id) return true
        return false
    }

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

    // The scroll bar, which View draws after onDraw from the frame just taken, in pixels so it
    // follows [scrollPx]; it shows only with a scrollback, so never on the alternate screen
    override fun computeVerticalScrollRange() = scrollTotal * cellHeight

    override fun computeVerticalScrollOffset() = scrollOffset * cellHeight + scrollPx

    override fun computeVerticalScrollExtent() = scrollLen * cellHeight

    // ---- Painting ----

    private fun drawRow(c: Canvas, y: Int) {
        val top = y * cellHeight
        val base = y * cols
        c.withClip(0, top, cols * cellWidth, top + cellHeight) {
            fill.color = defaultBg
            c.drawRect(0f, top.toFloat(), (cols * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
            // Images below the backgrounds show through cells of the default one
            drawImages(c, LAYER_BELOW_BACKGROUND, top)
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
            drawImages(c, LAYER_BELOW_TEXT, top)
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
            drawImages(c, LAYER_ABOVE_TEXT, top)
        }
    }

    /**
     * The images of [layer] that reach into the row at [top], to which the canvas is clipped, in
     * the frame's order; images whose pixels are not held at the placement's generation are left
     * out.
     */
    private fun drawImages(c: Canvas, layer: Int, top: Int) {
        for (k in 0 until imageCount) {
            val p = placements[k]
            if (p.layer != layer || p.dst.bottom <= top || p.dst.top >= top + cellHeight) continue
            val picture = pictures[p.id] ?: continue
            if (picture.generation != p.generation) continue
            c.drawBitmap(picture.bitmap, p.src, p.dst, imagePaint)
        }
    }

    /** A cell's background: a search match's colour, else its own (inverse swaps it with the text's). */
    private fun bgOf(i: Int): Int = when (markOf(i)) {
        MARK_MATCH -> SEARCH_MATCH
        MARK_SELECTED -> SEARCH_SELECTED
        else -> if (attrs[i] and INVERSE != 0) fg[i].takeIf { it != 0 } ?: defaultFg else bg[i].takeIf { it != 0 } ?: defaultBg
    }

    /** A cell's text colour: black on a search match, as in Ghostty, else its own. */
    private fun fgOf(i: Int): Int {
        if (markOf(i) != MARK_NONE) return SEARCH_TEXT
        val a = attrs[i]
        val color = if (a and INVERSE != 0) bg[i].takeIf { it != 0 } ?: defaultBg else fg[i].takeIf { it != 0 } ?: defaultFg
        return if (a and FAINT != 0) (color and 0x00ffffff) or 0x80000000.toInt() else color
    }

    /**
     * A cell's search mark (vt.c's attrs bits 14 and 15): [MARK_MATCH], [MARK_SELECTED] or
     * [MARK_NONE]. The second cell of a wide character takes the first's, so a match ending on one
     * covers the whole character.
     */
    private fun markOf(i: Int): Int {
        val a = attrs[i]
        val own = (a shr 14) and 3
        if (own != MARK_NONE || (a shr 12) and 3 != SPACER_TAIL || i % cols == 0) return own
        return (attrs[i - 1] shr 14) and 3
    }

    private fun paintOf(i: Int): Paint = paints[(attrs[i] and BOLD) or ((attrs[i] and ITALIC))]

    /**
     * One cell's glyph, centred in its [span] cells: a wide one enlarged by [wideScale], and any
     * shrunk evenly to fit them.
     */
    private fun drawGlyph(c: Canvas, i: Int, x: Int, span: Int, top: Int, color: Int, paint: Paint) {
        val code = cp[i]
        // With or without the diacritics that say which part of the image the cell shows
        if (code == KITTY_PLACEHOLDER) return
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
            // Composing text shows a block, as in Ghostty
            cursorStyle == CURSOR_LOCK && preedit.isEmpty() -> drawLock(c, l, t, r, b, color)
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
     * The cursor while the console reads a password: a padlock in the cursor's colour, about as
     * tall as a capital letter, standing on the baseline in the middle of its cell (or two), where
     * Ghostty draws a lock glyph (U+F023 from its Nerd Font, which the bundled font lacks); not
     * blinking, and shown without the focus too.
     */
    private fun drawLock(c: Canvas, l: Int, t: Int, r: Int, b: Int, color: Int) {
        val w = min(cellWidth, b - t) * 0.85f
        val h = w * 1.15f
        val cx = (l + r) / 2f
        val top = t + baseline - h
        val bodyTop = top + h * 0.42f
        lockPaint.color = color
        lockPaint.style = Paint.Style.FILL
        c.drawRoundRect(cx - w / 2f, bodyTop, cx + w / 2f, top + h, w * 0.12f, w * 0.12f, lockPaint)
        // The shackle: an arch from the body's top, inset from its sides
        val sw = max(light.toFloat(), w * 0.16f)
        val radius = (w * 0.68f - sw) / 2f
        shackle.rewind()
        shackle.moveTo(cx - radius, bodyTop)
        shackle.lineTo(cx - radius, top + sw / 2f + radius)
        shackle.arcTo(cx - radius, top + sw / 2f, cx + radius, top + sw / 2f + 2 * radius, 180f, 180f, false)
        shackle.lineTo(cx + radius, bodyTop)
        lockPaint.style = Paint.Style.STROKE
        lockPaint.strokeWidth = sw
        c.drawPath(shackle, lockPaint)
    }

    /**
     * Blinks the cursor every [BLINK_MS] (as Android's text fields do) while the frame says it
     * blinks (the setting, unless the program chose otherwise with DECSCUSR or mode 12) and the
     * view has the keyboard focus in a focused window;
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

    /**
     * The search's matches along the scroll bar, as browsers mark found text: a mark as wide as
     * the scroll bar where each row a match starts on lies among the scrollable rows, as the thumb
     * lies over the rows shown (its top where the mark of the viewport's first row is, unless the
     * thumb is held at its least length), the selected match's in its own colour, while a search
     * runs and there is a scrollback to scroll. They do not fade, unlike the thumb drawn over them.
     */
    private fun drawMatchMarks(c: Canvas) {
        if (!searching || scrollTotal <= scrollLen || (matchRows.isEmpty() && matchSelectedRow < 0)) return
        val right = width.toFloat()
        val left = right - verticalScrollbarWidth
        val last = height - matchMarkHeight
        val at = { row: Int -> (row.toLong() * height / scrollTotal).toInt().coerceIn(0, max(0, last)) }
        fill.color = SEARCH_MATCH
        var drawn = -1
        for (row in matchRows) {
            val top = at(row)
            // Rows closer than a pixel share one mark
            if (top == drawn) continue
            drawn = top
            c.drawRect(left, top.toFloat(), right, (top + matchMarkHeight).toFloat(), fill)
        }
        if (matchSelectedRow >= 0) {
            val top = at(matchSelectedRow)
            fill.color = SEARCH_SELECTED
            c.drawRect(left, top.toFloat(), right, (top + matchMarkHeight).toFloat(), fill)
        }
    }

    // ---- Input ----

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        hoverModifiers(event)
        return key(event) || super.onKeyDown(keyCode, event)
    }

    /** Releases matter only to programs using the Kitty keyboard protocol's event types. */
    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        hoverModifiers(event)
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

    /**
     * Copy and paste keys: Ctrl+Shift+C and V (held, or from the extra keys) and the dedicated
     * keys; and with a selection, Shift with an arrow, Page Up or Down, Home or End moves its end
     * ([ADJUSTMENTS]), as Ghostty's adjust_selection keys do (without one, they reach the program).
     */
    private fun shortcut(event: KeyEvent): Boolean {
        val both = event.isCtrlPressed && event.isShiftPressed
        val shiftOnly = event.isShiftPressed && !event.isCtrlPressed && !event.isAltPressed && !event.isMetaPressed
        val adjustment = if (selected && shiftOnly) ADJUSTMENTS[event.keyCode] else null
        return when {
            event.keyCode == KeyEvent.KEYCODE_PASTE || both && event.keyCode == KeyEvent.KEYCODE_V -> {
                pasteClipboard()
                true
            }
            event.keyCode == KeyEvent.KEYCODE_COPY || both && event.keyCode == KeyEvent.KEYCODE_C -> {
                if (selected) copySelection()
                true
            }
            adjustment != null && VtNative.selectAdjust(tab, adjustment) -> {
                // It may have scrolled the viewport by rows to show the end, which rests on a row
                scroller.forceFinished(true)
                scrollPx = 0
                // The menu follows the selection once the next frame is drawn
                awakenScrollBars()
                invalidate()
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
            scrollPx = 0
            awakenScrollBars()
            invalidate()
        }
        host.onInput(data)
    }

    override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
        super.onWindowFocusChanged(hasWindowFocus)
        // A fling not drawn to its end would jump there when the window draws again; a viewport it
        // leaves between rows comes to rest on the nearest (a short scroll that may end unseen)
        if (!hasWindowFocus) {
            scroller.forceFinished(true)
            if (scrollPx != 0) settle()
        }
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
        stopPasswordCheck()
    }

    /** A hidden window asks the kernel nothing; the frame drawn when it shows again resumes it. */
    override fun onWindowVisibilityChanged(visibility: Int) {
        super.onWindowVisibilityChanged(visibility)
        if (visibility != VISIBLE) {
            stopPasswordCheck()
        } else {
            invalidate()
        }
    }

    override fun onCheckIsTextEditor() = true

    /** Text without suggestions, or a password while the console reads one ([imePassword]). */
    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = if (imePassword) {
            InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_PASSWORD
        } else {
            InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        }
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_ACTION_NONE or (if (imePassword) EditorInfo.IME_FLAG_NO_PERSONALIZED_LEARNING else 0)
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
        val top = (cursorY * cellHeight - scrollPx).toFloat()
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
        dragOffsetY = if (cy < 0) 0f else (cy + 0.5f) * cellHeight - scrollPx - y
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
        val cy = cellY(py)
        // A mouse's cell drag starts within its cell, once past libghostty-vt's threshold in it
        if (drag != DRAG_POINTER && cx == dragCellX && cy == dragCellY) return
        dragCellX = cx
        dragCellY = cy
        extendTo(cx, cy, px, py + scrollPx)
    }

    /**
     * Extends the selection to viewport cell ([cx], [cy]), at pixel ([px], [py]) from the grid's
     * corner: by words or lines from a gesture's press ([VtNative.gestureDrag]), by a mouse's
     * click's unit from its press ([VtNative.mouseDrag], which may also leave none), by cells from
     * a handle's anchor ([VtNative.selectTo]).
     */
    private fun extendTo(cx: Int, cy: Int, px: Float, py: Float) {
        if (drag == DRAG_POINTER) {
            val dragged = VtNative.mouseDrag(tab, cx, cy, px, py, pointerRectangle)
            if (dragged >= 0) {
                selected = dragged == 1
                invalidate()
            }
            return
        }
        val extended = if (drag == DRAG_WORD || drag == DRAG_TAPS) {
            VtNative.gestureDrag(tab, cx, cy, px, py)
        } else {
            VtNative.selectTo(tab, cx, cy)
        }
        if (extended) {
            selected = true
            invalidate()
        }
    }

    private fun endDrag() {
        if (drag == DRAG_WORD || drag == DRAG_TAPS || drag == DRAG_PRESS) VtNative.gestureEnd(tab)
        drag = DRAG_NONE
        autoscroll = 0
        removeCallbacks(autoscrollTick)
        menuX = -1f
        menuPending = true
        invalidate()
    }

    /**
     * The floating menu: Open link after a long press on one, Copy with a selection, Paste with
     * text on the clipboard, Select output after a long press in a command's output, Select all,
     * and with a selection Rectangle (its ends become opposite corners) or, for a rectangle, Lines
     * (back to a linear selection).
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
                    if (menuOutput) m.add(Menu.NONE, MENU_SELECT_OUTPUT, 3, R.string.select_output)
                    m.add(Menu.NONE, MENU_SELECT_ALL, 4, android.R.string.selectAll)
                    if (selected) {
                        m.add(Menu.NONE, MENU_RECTANGLE, 5, if (rectangle) R.string.select_lines else R.string.select_rectangle)
                    }
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
                        MENU_SELECT_OUTPUT -> {
                            menuOutput = false
                            if (VtNative.selectOutput(tab)) {
                                selected = true
                                menuLink = null
                                menuX = -1f
                            }
                            // The menu comes back, for the output once the frame has it, or without
                            // this item when the output has gone
                            menuPending = true
                            mode.finish()
                            invalidate()
                        }
                        MENU_SELECT_ALL -> if (VtNative.selectAll(tab)) {
                            selected = true
                            menuLink = null
                            menuOutput = false
                            menuX = -1f
                            menuPending = true
                            mode.finish()
                            invalidate()
                        }
                        // The menu comes back with the other item once the frame has the new shape
                        MENU_RECTANGLE -> if (VtNative.selectRectangle(tab, !rectangle)) {
                            rectangle = !rectangle
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
                    outRect.set(l, first * cellHeight - scrollPx, r, (last + 1) * cellHeight - scrollPx)
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
        if (selected && !pointerSelection) {
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
        val top = (y + 1) * cellHeight - scrollPx
        return d to Rect(l, top, l + d.intrinsicWidth, top + d.intrinsicHeight)
    }

    /** The handle under a touch, with some slop: DRAG_START, DRAG_END or DRAG_NONE (none shown). */
    private fun handleAt(x: Float, y: Float): Int {
        if (!selected || pointerSelection) return DRAG_NONE
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
        menuOutput = false
        menu?.finish()
        invalidate()
    }

    // ---- Gestures ----

    /**
     * A touch, then: a viewport left between rows comes to rest on the nearest whole row once the
     * gesture ends without a fling, which settles it when it stops.
     */
    @SuppressLint("ClickableViewAccessibility") // Taps open the keyboard; selection has its own menu
    override fun onTouchEvent(event: MotionEvent): Boolean {
        val handled = touch(event)
        val action = event.actionMasked
        if ((action == MotionEvent.ACTION_UP || action == MotionEvent.ACTION_CANCEL) && scrollPx != 0 && scroller.isFinished) {
            settle()
        }
        return handled
    }

    private fun touch(event: MotionEvent): Boolean {
        // A touch stops a fling; settling to a whole row is no fling, so its tap still acts
        if (event.actionMasked == MotionEvent.ACTION_DOWN) {
            stoppedFling = !scroller.isFinished && !settling
            scroller.forceFinished(true)
        }
        // A mouse's buttons and drags go to a program tracking it as they are, from the press on,
        // unless Shift is held (Ghostty's mouse-shift-capture = false)
        val down = event.actionMasked == MotionEvent.ACTION_DOWN && drag == DRAG_NONE
        val mouse = tab != 0L && event.isFromSource(InputDevice.SOURCE_MOUSE) && !touchpadGesture(event)
        if (mouse && (down && mouseTracking && !event.held(KeyEvent.META_SHIFT_ON) || mouseButton != 0)) {
            mouseButtons(event)
            return true
        }
        // Otherwise they select, as in a desktop terminal
        if (mouse && (down || pointerButton != 0)) {
            pointer(event)
            return true
        }
        // A handle drag owns the whole gesture
        if (event.actionMasked == MotionEvent.ACTION_DOWN && tab != 0L) {
            val handle = handleAt(event.x, event.y)
            // A finger brings the handles back
            pointerSelection = false
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
        // So does a double or triple tap's press that selected: moved on, it drags the selection
        if (event.actionMasked == MotionEvent.ACTION_DOWN && tapPress(event)) return true
        if (drag == DRAG_TAPS) {
            when (event.actionMasked) {
                MotionEvent.ACTION_MOVE -> if (pressMoved || hypot(event.x - pressX, event.y - pressY) > touchSlop) {
                    pressMoved = true
                    dragTo(event.x, event.y)
                }
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> endTaps(event)
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

    /**
     * A touchpad's swipe or pinch, which Android may deliver as touches from the mouse source
     * (Android 14 and later classify them): not a press of a button.
     */
    private fun touchpadGesture(e: MotionEvent) = Build.VERSION.SDK_INT >= 34 &&
        (e.classification == MotionEvent.CLASSIFICATION_TWO_FINGER_SWIPE || e.classification == MotionEvent.CLASSIFICATION_PINCH)

    /**
     * A mouse (or touchpad) button pressed, dragged and released to select, as Ghostty does on
     * Linux, the first button pressed owning the rest: the left one (a touchpad's tap) clicks
     * through libghostty-vt's selection gesture, which counts the clicks (within the double-tap
     * timeout and a cell's width): a first click clears the selection and dragging from it
     * selects by cells (a rectangle while Ctrl+Alt is held), a second selects the word and a third
     * the line (the command's output with Ctrl), each dragging on by its unit and scrolling past
     * the grid's edges; Shift+press after the timeout extends the selection from the last press
     * instead. A click that did not drag opens the link under it with Ctrl, else moves the shell's
     * cursor there (at its prompt) and opens the keyboard, as a tap does. The right button selects
     * the word under it unless the selection holds it and brings up the menu there. No menu comes
     * up otherwise, and the selection shows no handles.
     */
    private fun pointer(e: MotionEvent) {
        when (e.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                requestFocus()
                pointerButton = when {
                    e.buttonState and MotionEvent.BUTTON_SECONDARY != 0 -> MOUSE_RIGHT
                    e.buttonState and MotionEvent.BUTTON_TERTIARY != 0 -> MOUSE_MIDDLE
                    else -> MOUSE_LEFT // Also a touchpad's tap, which holds no button
                }
                if (pointerButton == MOUSE_MIDDLE) return
                menu?.finish()
                menuLink = null
                menuOutput = false
                pointerSelection = true
                if (pointerButton == MOUSE_LEFT) pointerPress(e) else contextMenu(e)
            }
            MotionEvent.ACTION_MOVE -> if (pointerButton == MOUSE_LEFT && drag == DRAG_POINTER) {
                pointerRectangle = rectangleHeld(e)
                dragTo(e.x, e.y)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                if (pointerButton == MOUSE_LEFT) pointerRelease(e)
                pointerButton = 0
            }
        }
    }

    private fun pointerPress(e: MotionEvent) {
        val interval = ViewConfiguration.getDoubleTapTimeout().toLong()
        pointerRectangle = rectangleHeld(e)
        startDrag(DRAG_POINTER, e.x, e.y)
        val cx = cellX(e.x)
        val cy = cellY(e.y)
        val py = e.y + scrollPx
        // Shift+press extends the selection from the last press, unless it may be its next click
        if (e.held(KeyEvent.META_SHIFT_ON) && selected && e.eventTime - lastPointerPress > interval) {
            val extended = VtNative.mouseDrag(tab, cx, cy, e.x, py, pointerRectangle)
            if (extended >= 0) {
                selected = extended == 1
                invalidate()
                return
            }
        }
        lastPointerPress = e.eventTime
        val ctrl = e.held(CTRL_OR_META)
        val clicks = VtNative.mousePress(tab, cx, cy, e.x, py, e.eventTime * 1_000_000, interval * 1_000_000, ctrl)
        // The button still owns the rest of its stream, which then does nothing
        if (clicks == 0) drag = DRAG_NONE
        // The next frame tells whether a word or line was there
        selected = clicks >= 2
        invalidate()
    }

    private fun pointerRelease(e: MotionEvent) {
        val dragging = drag == DRAG_POINTER
        drag = DRAG_NONE
        autoscroll = 0
        removeCallbacks(autoscrollTick)
        if (!dragging) return
        val inside = e.x >= 0 && e.y >= 0 && e.x < cols * cellWidth && e.y < rows * cellHeight
        val clicks = VtNative.mouseRelease(tab, if (inside) cellX(e.x) else -1, if (inside) cellY(e.y) else -1)
        // A click that did not drag: Ctrl opens a link, else as a tap
        if (clicks != 1 || e.actionMasked != MotionEvent.ACTION_UP) return
        val link = if (e.held(CTRL_OR_META)) linkAt(e.x, e.y) else null
        if (link != null) {
            host.onLink(link)
            return
        }
        send(VtNative.promptClick(tab, cellX(e.x), cellY(e.y)))
        context.getSystemService(InputMethodManager::class.java).showSoftInput(this, 0)
    }

    /** Ctrl (or Meta) and Alt held: Ghostty's rectangle selection on Linux. */
    private fun rectangleHeld(e: MotionEvent) = e.held(CTRL_OR_META) && e.held(KeyEvent.META_ALT_ON)

    /** Any of the [meta] modifiers is held during the event. */
    private fun MotionEvent.held(meta: Int) = metaState and meta != 0

    /**
     * A right-click: as Ghostty's before its context menu, the word under it is selected unless
     * the selection holds it, and the menu comes up there once the frame has the selection.
     */
    private fun contextMenu(e: MotionEvent) {
        selected = VtNative.selectContext(tab, cellX(e.x), cellY(e.y))
        menuLink = linkAt(e.x, e.y)
        menuOutput = VtNative.outputAt(tab, cellX(e.x), cellY(e.y))
        menuX = e.x
        menuY = e.y
        menuPending = true
        invalidate()
    }

    /**
     * The press of the second or third tap in a row (each within the double-tap timeout and slop
     * of the last tap's lift) without mouse tracking: selects the word or the line under it at
     * once, and a drag on from it extends the selection by words or lines, as Ghostty's double-
     * and triple-click drags do. False when it is no such press, or there is nothing to select
     * there (blank cells): it then goes on as any touch, and its tap acts as a single one.
     */
    private fun tapPress(e: MotionEvent): Boolean {
        if (tab == 0L || mouseTracking || stoppedFling) return false
        val again = e.downTime - lastTapTime <= ViewConfiguration.getDoubleTapTimeout() &&
            hypot(e.x - lastTapX, e.y - lastTapY) <= doubleTapSlop
        if (!again) return false
        val behavior = if (taps + 1 == 3) VtNative.GESTURE_LINE else VtNative.GESTURE_WORD
        if (!VtNative.gesturePress(tab, behavior, cellX(e.x), cellY(e.y), e.x, e.y + scrollPx)) return false
        taps++
        selected = true
        menuLink = null
        menuOutput = false
        startDrag(DRAG_TAPS, e.x, e.y)
        pressX = e.x
        pressY = e.y
        pressMoved = false
        invalidate()
        return true
    }

    /**
     * The finger lifted from a double or triple tap's press: the selection's menu comes up, and a
     * press that did not drag counts as a tap, which the next may follow (after a third, a tap
     * starts over).
     */
    private fun endTaps(e: MotionEvent) {
        val tapped = e.actionMasked == MotionEvent.ACTION_UP && !pressMoved
        endDrag()
        lastTapTime = if (tapped && taps < 3) e.eventTime else 0L
        lastTapX = e.x
        lastTapY = e.y
    }

    /** The finger moved on after a long press in mouse mode: the program gets a left-button drag. */
    private fun startMouseDrag(e: MotionEvent) {
        if (selected) clearSelection()
        menuLink = null
        menuOutput = false
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

    /**
     * Taps without a delay: each tap acts at once, and the press of a double tap's second selects
     * the word, of a triple tap's third the line ([tapPress], before the detector sees it).
     */
    private val gestures = GestureDetector(
        context,
        object : GestureDetector.OnGestureListener {
            override fun onDown(e: MotionEvent) = true

            override fun onShowPress(e: MotionEvent) = Unit

            override fun onSingleTapUp(e: MotionEvent): Boolean {
                // Only stops the fling, as a tap on a flinging ScrollView does
                if (stoppedFling) return true
                val again = e.downTime - lastTapTime <= ViewConfiguration.getDoubleTapTimeout() &&
                    hypot(e.x - lastTapX, e.y - lastTapY) <= doubleTapSlop
                taps = if (again) taps + 1 else 1
                // A fourth tap starts over
                lastTapTime = if (taps < 3) e.eventTime else 0L
                lastTapX = e.x
                lastTapY = e.y
                // In mouse mode every tap is a click, for the program's own double and triple
                // clicks; without it a double or triple tap reaches here only on blank cells
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
                // A tap on the input typed at the shell's prompt moves the shell's cursor there, as
                // Ghostty's cursor-click-to-move does; the keyboard comes up all the same
                if (tab != 0L && mouseTracking) {
                    click(e)
                } else if (tab != 0L) {
                    send(VtNative.promptClick(tab, cellX(e.x), cellY(e.y)))
                }
                requestFocus()
                context.getSystemService(InputMethodManager::class.java).showSoftInput(this@VtView, 0)
                return true
            }

            override fun onScroll(e1: MotionEvent?, e2: MotionEvent, distanceX: Float, distanceY: Float): Boolean {
                if (tab == 0L) return false
                // The viewport follows the finger by pixels; wheel events and arrow keys go by rows
                if (scrollWay() == WAY_VIEWPORT) {
                    pixelRemainder += distanceY
                    val px = pixelRemainder.toInt()
                    pixelRemainder -= px
                    if (px != 0) scrollPixels(px)
                    return true
                }
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
                val word = VtNative.gesturePress(tab, VtNative.GESTURE_WORD, cellX(e.x), cellY(e.y), e.x, e.y + scrollPx)
                menuLink = linkAt(e.x, e.y)
                menuOutput = VtNative.outputAt(tab, cellX(e.x), cellY(e.y))
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
     * The pointer [updatePointer] set, also when the system resolves it again after it was set
     * while the mouse rests (a program's OSC 22): that event comes from no mouse, and the view's
     * own resolution returns the pointer for mouse events only, which would leave the default
     * arrow. A stylus keeps its own.
     */
    override fun onResolvePointerIcon(event: MotionEvent, pointerIndex: Int): PointerIcon? {
        val resolved = super.onResolvePointerIcon(event, pointerIndex)
        return if (resolved != null || event.isFromSource(InputDevice.SOURCE_STYLUS)) resolved else pointerIcon
    }

    /**
     * A mouse wheel (or touchpad scroll) scrolls like a drag, and its sideways scrolling goes to a
     * program tracking the mouse as wheel buttons 6 and 7 (left and right), one a notch, as in
     * Ghostty (nothing else scrolls sideways); a mouse moving with no button held goes to a
     * program tracking all motion (mode 1003).
     */
    override fun onGenericMotionEvent(event: MotionEvent): Boolean {
        if (tab != 0L && event.isFromSource(InputDevice.SOURCE_CLASS_POINTER)) {
            when (event.actionMasked) {
                MotionEvent.ACTION_SCROLL -> {
                    val v = event.getAxisValue(MotionEvent.AXIS_VSCROLL)
                    val h = event.getAxisValue(MotionEvent.AXIS_HSCROLL)
                    if (v != 0f) scrollBy(if (v > 0) -WHEEL_ROWS else WHEEL_ROWS, event.x, event.y)
                    val sideways = h != 0f && mouseTracking
                    if (sideways) {
                        val button = if (h > 0) MOUSE_WHEEL_RIGHT else MOUSE_WHEEL_LEFT
                        send(VtNative.mouse(tab, MOUSE_PRESS, button, 0, event.x, event.y, false), scrolling = true)
                    }
                    if (v != 0f || sideways) return true
                }
                MotionEvent.ACTION_HOVER_MOVE -> {
                    hoverAt(event.x, event.y, event.held(CTRL_OR_META))
                    if (mouseTracking) {
                        send(VtNative.mouse(tab, MOUSE_MOTION, MOUSE_NONE, VtKeys.mods(event.metaState), event.x, event.y, false))
                        return true
                    }
                }
                MotionEvent.ACTION_HOVER_EXIT -> {
                    hoverCell = -1
                    hoverLink = false
                    updatePointer()
                }
            }
        }
        return super.onGenericMotionEvent(event)
    }

    /**
     * A mouse hovers at ([x], [y]) with [ctrl] (or Meta) held or not: on coming to another cell,
     * whether it is over a link is looked up again, for the pointer.
     */
    private fun hoverAt(x: Float, y: Float, ctrl: Boolean) {
        val cell = cellY(y) * cols + cellX(x)
        if (cell == hoverCell && ctrl == hoverCtrl) return
        if (cell != hoverCell) {
            hoverCell = cell
            hoverLink = linkAt(x, y) != null
        }
        hoverCtrl = ctrl
        updatePointer()
    }

    /** Ctrl (or Meta) pressed or let go while a mouse hovers: a link under it shows a hand or not. */
    private fun hoverModifiers(event: KeyEvent) {
        val ctrl = event.isCtrlPressed || event.isMetaPressed
        if (hoverCell < 0 || ctrl == hoverCtrl) return
        hoverCtrl = ctrl
        updatePointer()
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
            else -> scrollPixels(n * cellHeight)
        }
    }

    /**
     * Scrolls the viewport by [dy] pixels (positive: towards the newest output): within a row by
     * the [scrollPx], across rows by the terminal's viewport, as far as either end, where the grid
     * rests on a whole row (at the bottom there is no row after the viewport to show). The ends
     * are those of the last frame, kept up with the rows moved since; false when it could not
     * move at all.
     */
    private fun scrollPixels(dy: Int): Boolean {
        val last = max(0, scrollTotal - scrollLen)
        val to = (scrollOffset.toLong() * cellHeight + scrollPx + dy).coerceIn(0L, last.toLong() * cellHeight)
        val top = (to / cellHeight).toInt()
        val s = (to % cellHeight).toInt()
        if (top == scrollOffset && s == scrollPx) return false
        if (top != scrollOffset) {
            VtNative.scroll(tab, top - scrollOffset)
            scrollOffset = top
            atBottom = top == last
        }
        scrollPx = s
        awakenScrollBars()
        invalidate()
        return true
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
     * output), slowing down as a ScrollView does ([computeScroll] takes the pixels, or rows): the
     * viewport up to either end, wheel events and arrow keys for at most a screen of rows.
     */
    private fun fling(x: Float, y: Float, velocity: Int) {
        flingWay = scrollWay()
        flingX = x
        flingY = y
        flingLast = 0
        settling = false
        val reach = (if (flingWay == WAY_VIEWPORT) scrollTotal else rows) * cellHeight
        scroller.fling(0, 0, 0, velocity, 0, 0, -reach, reach)
        postInvalidateOnAnimation()
    }

    /**
     * Brings a viewport left between rows to the nearest whole row, scrolling there in
     * [SETTLE_MS] as the scroller's fling does.
     */
    private fun settle() {
        val to = if (scrollPx < cellHeight / 2) -scrollPx else cellHeight - scrollPx
        flingWay = WAY_VIEWPORT
        flingLast = 0
        settling = true
        scroller.startScroll(0, 0, 0, to, SETTLE_MS)
        postInvalidateOnAnimation()
    }

    /**
     * A fling's next pixels or rows, before each frame while it lasts, and its settling to a whole
     * row once it stops in the viewport between two. It stops when the way to scroll changes (a
     * program turning mouse tracking on, the alternate screen) or the viewport reaches the end it
     * moves to.
     */
    override fun computeScroll() {
        if (!scroller.computeScrollOffset()) return
        val delta = scroller.currY - flingLast
        flingLast = scroller.currY
        if (tab == 0L || scrollWay() != flingWay) {
            scroller.forceFinished(true)
            return
        }
        if (flingWay == WAY_VIEWPORT) {
            if (delta != 0 && !scrollPixels(delta)) scroller.forceFinished(true)
            if (scroller.isFinished) {
                if (scrollPx != 0) settle()
            } else {
                postInvalidateOnAnimation()
            }
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

    /** The row of the viewport at [y], counting the [scrollPx] (the overscan row's part as the last row). */
    private fun cellY(y: Float) = floor((y + scrollPx) / cellHeight).toInt().coerceIn(0, rows - 1)

    private companion object {
        /** Shift with these keys moves a selection's end ([VtNative.selectAdjust]), as in Ghostty. */
        val ADJUSTMENTS = mapOf(
            KeyEvent.KEYCODE_DPAD_LEFT to VtNative.ADJUST_LEFT,
            KeyEvent.KEYCODE_DPAD_RIGHT to VtNative.ADJUST_RIGHT,
            KeyEvent.KEYCODE_DPAD_UP to VtNative.ADJUST_UP,
            KeyEvent.KEYCODE_DPAD_DOWN to VtNative.ADJUST_DOWN,
            KeyEvent.KEYCODE_PAGE_UP to VtNative.ADJUST_PAGE_UP,
            KeyEvent.KEYCODE_PAGE_DOWN to VtNative.ADJUST_PAGE_DOWN,
            KeyEvent.KEYCODE_MOVE_HOME to VtNative.ADJUST_HOME,
            KeyEvent.KEYCODE_MOVE_END to VtNative.ADJUST_END,
        )

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

        // GhosttyCursorStyle (and vt.c's lock)
        const val CURSOR_BAR = 0
        const val CURSOR_UNDERLINE = 2
        const val CURSOR_HOLLOW = 3
        const val CURSOR_LOCK = 4

        /** `GhosttyMouseShape`'s text, the pointer's shape until a program asks for another. */
        const val POINTER_TEXT = 8

        /**
         * Android's system pointer closest to a `GhosttyMouseShape` (OSC 22's names, CSS's
         * cursors): progress shows as waiting, move as all-scroll, not-allowed as no-drop and the
         * resize arrows by their axis, which Android's pointers do not tell apart by edge.
         */
        fun pointerType(shape: Int): Int = when (shape) {
            0 -> PointerIcon.TYPE_ARROW
            1 -> PointerIcon.TYPE_CONTEXT_MENU
            2 -> PointerIcon.TYPE_HELP
            3 -> PointerIcon.TYPE_HAND
            4, 5 -> PointerIcon.TYPE_WAIT
            6 -> PointerIcon.TYPE_CELL
            7 -> PointerIcon.TYPE_CROSSHAIR
            9 -> PointerIcon.TYPE_VERTICAL_TEXT
            10 -> PointerIcon.TYPE_ALIAS
            11 -> PointerIcon.TYPE_COPY
            12, 17 -> PointerIcon.TYPE_ALL_SCROLL
            13, 14 -> PointerIcon.TYPE_NO_DROP
            15 -> PointerIcon.TYPE_GRAB
            16 -> PointerIcon.TYPE_GRABBING
            18, 21, 23, 28 -> PointerIcon.TYPE_HORIZONTAL_DOUBLE_ARROW
            19, 20, 22, 29 -> PointerIcon.TYPE_VERTICAL_DOUBLE_ARROW
            24, 27, 30 -> PointerIcon.TYPE_TOP_RIGHT_DIAGONAL_DOUBLE_ARROW
            25, 26, 31 -> PointerIcon.TYPE_TOP_LEFT_DIAGONAL_DOUBLE_ARROW
            32 -> PointerIcon.TYPE_ZOOM_IN
            33 -> PointerIcon.TYPE_ZOOM_OUT
            else -> PointerIcon.TYPE_TEXT
        }

        // The frame format (vt.c)
        const val HEADER_BYTES = 64
        const val IMAGE_BYTES = 56
        const val ROW_HEADER_BYTES = 12
        const val DIRTY_NONE = 0
        const val DIRTY_FULL = 2
        const val FLAG_MOUSE = 1
        const val FLAG_ALT_SCREEN = 1 shl 1
        const val FLAG_ALT_SCROLL = 1 shl 2
        const val FLAG_SELECTION = 1 shl 4
        const val FLAG_RECTANGLE = 1 shl 5
        const val FLAG_SEARCH_PENDING = 1 shl 6

        // An image placement's layer (GhosttyImageLayer)
        const val LAYER_BELOW_BACKGROUND = 0
        const val LAYER_BELOW_TEXT = 1
        const val LAYER_ABOVE_TEXT = 2

        /**
         * The Kitty graphics placeholder (U+10EEEE): its cells show part of an image, which the
         * frame places, and are drawn blank, as Ghostty draws them.
         */
        const val KITTY_PLACEHOLDER = 0x10EEEE

        // A cell's search mark (attrs bits 14 and 15)
        const val MARK_NONE = 0
        const val MARK_MATCH = 1
        const val MARK_SELECTED = 2

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
        const val MOUSE_WHEEL_LEFT = 6
        const val MOUSE_WHEEL_RIGHT = 7

        /** Rows per mouse wheel notch. */
        const val WHEEL_ROWS = 3

        /** How long the viewport takes to come to rest on a whole row after scrolling stops between two. */
        const val SETTLE_MS = 150

        /** A blinking cursor's half period, as Android's text fields. */
        const val BLINK_MS = 500L

        /** How often the console is asked whether it still reads a password, as Ghostty polls. */
        const val PASSWORD_CHECK_MS = 200L

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
        const val DRAG_TAPS = 6
        const val DRAG_POINTER = 7

        /** Ctrl or Meta (Super): Ghostty's ctrlOrSuper, the link and output modifier on Linux. */
        const val CTRL_OR_META = KeyEvent.META_CTRL_ON or KeyEvent.META_META_ON
        const val AUTOSCROLL_MS = 50L

        const val MENU_COPY = 1
        const val MENU_PASTE = 2
        const val MENU_SELECT_ALL = 3
        const val MENU_OPEN_LINK = 4
        const val MENU_RECTANGLE = 5
        const val MENU_SELECT_OUTPUT = 6

        /**
         * The schemes a tapped link may have: web pages, and from an OSC 8 link mail and guest
         * files (`ls --hyperlink`).
         */
        val LINK_SCHEMES = setOf("http", "https", "mailto", "file")

        /** Characters of a paste shown when asking whether to paste it. */
        const val PASTE_PREVIEW = 300

        /** The selection's highlight, over cell backgrounds and under text. */
        const val SELECTION = 0x604A90E2

        /**
         * Search matches, as Ghostty draws them (its search-background, search-selected-background
         * and search-foreground): black text on yellow, the selected match on orange.
         */
        const val SEARCH_MATCH = 0xFFFFE082.toInt()
        const val SEARCH_SELECTED = 0xFFF2A57E.toInt()
        const val SEARCH_TEXT = 0xFF000000.toInt()

        /** How often the rows of the search's matches are taken again while output goes on. */
        const val MATCH_ROWS_MS = 500L
    }
}
