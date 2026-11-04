package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.DashPathEffect
import android.graphics.Paint
import android.graphics.Path
import android.graphics.Typeface
import android.os.SystemClock
import android.text.InputType
import android.util.Log
import android.util.TypedValue
import android.view.GestureDetector
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.ScaleGestureDetector
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.math.ceil
import kotlin.math.max
import kotlin.math.roundToInt

/**
 * The experimental terminal view (android/terminal map): draws one tab's libghostty-vt terminal
 * ([VtNative]) with JetBrains Mono on a fixed cell grid.
 *
 * Each frame takes the rows that changed ([VtNative.frame]) into a mirror of the screen and paints
 * them into a bitmap the size of the grid; the cursor and the input method's composing text are
 * drawn over it. Glyphs are placed cell by cell: runs of ASCII share one draw call (the font size
 * is chosen so their advance is exactly one cell), every other glyph is centred in its one or two
 * cells, or shrunk evenly when it is wider, never stretched. Box drawing, block elements and the
 * Powerline triangles are drawn as shapes ([VtGlyphs]).
 */
@SuppressLint("ViewConstructor") // Created only in code
internal class VtView(context: Context, private val host: Host) : View(context) {

    interface Host {
        /** The grid's size, first known or changed (main thread). */
        fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int)

        /** Bytes for the program: keys and text (main thread). */
        fun onInput(data: ByteArray)
    }

    /** The tab shown, 0 for none; frames are taken only once it is set. */
    var tab = 0L
        set(value) {
            field = value
            full = true
            invalidate()
        }

    /** One-shot modifiers from the extra keys: they apply to the next key or character. */
    var ctrlOnce = false
    var altOnce = false
    var onModifiersUsed: (() -> Unit)? = null

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

    /** The same default as the main screen's Termux view (12 dp), for comparing the two. */
    private var fontPx = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, 12f, resources.displayMetrics)
    private var cellWidth = 1
    private var cellHeight = 1
    private var baseline = 0
    private var light = 1
    private var underlineY = 0
    private var strikeY = 0
    private var dotted = DashPathEffect(floatArrayOf(1f, 1f), 0f)
    private var dashed = DashPathEffect(floatArrayOf(3f, 2f), 0f)

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
    private var cursorX = 0
    private var cursorY = 0
    private var atBottom = true

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

    init {
        isFocusable = true
        isFocusableInTouchMode = true
        measureCells()
    }

    private fun font(path: String, weight: Int): Typeface =
        Typeface.Builder(context.assets, path).setFontVariationSettings("'wght' $weight").build()

    private fun textPaint(typeface: Typeface) =
        Paint(Paint.ANTI_ALIAS_FLAG or Paint.SUBPIXEL_TEXT_FLAG).apply {
            this.typeface = typeface
            // Ligatures would join cells
            fontFeatureSettings = "'calt' 0, 'liga' 0"
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
        bitmap?.recycle()
        bitmap = Bitmap.createBitmap(bw, bh, Bitmap.Config.ARGB_8888).also { canvas = Canvas(it) }
        full = true
        if (tab != 0L) VtNative.resize(tab, c, r, cellWidth, cellHeight)
        host.onGrid(c, r, cellWidth, cellHeight)
        invalidate()
    }

    private fun zoom(step: Int) {
        val density = resources.displayMetrics.density
        val px = (fontPx + step * density).coerceIn(6 * density, 32 * density)
        if (px == fontPx) return
        fontPx = px
        measureCells()
        regrid()
    }

    // ---- Frames ----

    /** Called by [VtNative.pump] on its thread when the screen changed. */
    @Suppress("unused") // From vt.rs
    fun onOutput() = postInvalidateOnAnimation()

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
        cursorShown = b.get(5).toInt() != 0
        cursorStyle = b.get(6).toInt()
        cursorX = b.getShort(8).toInt() and 0xffff
        cursorY = b.getShort(10).toInt() and 0xffff
        val newFg = b.getInt(12)
        val newBg = b.getInt(16)
        cursorColor = b.getInt(20)
        val count = b.getShort(24).toInt() and 0xffff
        atBottom = b.get(26).toInt() != 0
        if (newFg != defaultFg || newBg != defaultBg) {
            // Rows that did not change still have the old default colours
            defaultFg = newFg
            defaultBg = newBg
            full = true
            postInvalidateOnAnimation()
        }
        frameRows += count
        val target = canvas ?: return
        var pos = 32
        repeat(count) {
            val y = b.getShort(pos).toInt() and 0xffff
            val n = b.getShort(pos + 2).toInt() and 0xffff
            val extra = b.getInt(pos + 4)
            pos += 8
            var e = pos + n * 16
            if (y < rows) {
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

    // ---- Painting ----

    private fun fgOf(i: Int): Int {
        val a = attrs[i]
        val color = if (a and INVERSE != 0) bg[i].takeIf { it != 0 } ?: defaultBg else fg[i].takeIf { it != 0 } ?: defaultFg
        return if (a and FAINT != 0) (color and 0x00ffffff) or 0x80000000.toInt() else color
    }

    private fun bgOf(i: Int): Int =
        if (attrs[i] and INVERSE != 0) fg[i].takeIf { it != 0 } ?: defaultFg else bg[i].takeIf { it != 0 } ?: defaultBg

    private fun paintOf(i: Int): Paint = paints[(attrs[i] and BOLD) or ((attrs[i] and ITALIC))]

    private fun drawRow(c: Canvas, y: Int) {
        val top = y * cellHeight
        val base = y * cols
        c.save()
        c.clipRect(0, top, cols * cellWidth, top + cellHeight)
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
        c.restore()
    }

    /** One cell's glyph, centred in its [span] cells, or shrunk evenly to fit them. */
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
        if (advance <= w + 0.5f) {
            c.drawText(text, left + (w - advance) / 2, (top + baseline).toFloat(), paint)
        } else {
            val s = w / advance
            c.save()
            c.scale(s, s, left.toFloat(), top + cellHeight / 2f)
            c.drawText(text, left.toFloat(), (top + baseline).toFloat(), paint)
            c.restore()
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
                c.save()
                c.clipRect(l, t, r, b)
                c.drawRect(l.toFloat(), t.toFloat(), r.toFloat(), b.toFloat(), fill)
                if (attrs[i] and INVISIBLE == 0) drawGlyph(c, i, cursorX, span, t, bgOf(i), paintOf(i))
                c.restore()
            }
        }
    }

    private fun drawPreedit(c: Canvas) {
        if (preedit.isEmpty() || cursorY >= rows) return
        val p = paints[0]
        val l = (cursorX * cellWidth).toFloat()
        val t = (cursorY * cellHeight).toFloat()
        val w = p.measureText(preedit)
        fill.color = defaultBg
        c.drawRect(l, t, l + w, t + cellHeight, fill)
        p.color = defaultFg
        c.drawText(preedit, l, t + baseline, p)
        fill.color = defaultFg
        c.drawRect(l, t + underlineY, l + w, t + underlineY + light, fill)
    }

    // ---- Input ----

    private fun send(data: ByteArray) {
        if (data.isEmpty()) return
        if (!atBottom && tab != 0L) {
            VtNative.scroll(tab, 0)
            invalidate()
        }
        host.onInput(data)
    }

    /** Text from the input method or the extra keys; one character with a one-shot modifier is a key. */
    fun type(text: String) {
        if ((ctrlOnce || altOnce) && text.codePointCount(0, text.length) == 1) {
            val events = KeyCharacterMap.load(KeyCharacterMap.VIRTUAL_KEYBOARD).getEvents(text.toCharArray())
            val down = events?.firstOrNull { it.action == KeyEvent.ACTION_DOWN && it.keyCode != KeyEvent.KEYCODE_SHIFT_LEFT }
            if (down != null && key(down)) return
        }
        send(text.replace('\n', '\r').toByteArray())
    }

    /** A key from the extra keys row. */
    fun press(keyCode: Int) {
        key(KeyEvent(KeyEvent.ACTION_DOWN, keyCode))
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean =
        key(event) || super.onKeyDown(keyCode, event)

    /** Encodes a key press for the program; false for keys it does not take (Back, volume). */
    private fun key(event: KeyEvent): Boolean {
        if (tab == 0L || event.isSystem) return false
        val key = VtKeys.of(event.keyCode)
        var mods = VtKeys.mods(event.metaState)
        if (ctrlOnce) mods = mods or VtKeys.MOD_CTRL
        if (altOnce) mods = mods or VtKeys.MOD_ALT
        val plain = event.getUnicodeChar(
            event.metaState and (KeyEvent.META_CTRL_MASK or KeyEvent.META_ALT_MASK or KeyEvent.META_META_MASK).inv(),
        )
        val text = if (plain >= 0x20 && plain != 0x7f && plain and KeyCharacterMap.COMBINING_ACCENT == 0) {
            String(Character.toChars(plain))
        } else {
            null
        }
        if (key == 0) {
            if (text == null) return false
            if (mods and (VtKeys.MOD_CTRL or VtKeys.MOD_ALT) == 0) {
                send(text.toByteArray())
                return true
            }
        }
        val unshifted = event.getUnicodeChar(0)
        val consumed = if (mods and VtKeys.MOD_SHIFT != 0 && text != null && plain != unshifted) VtKeys.MOD_SHIFT else 0
        val action = if (event.repeatCount > 0) VtKeys.ACTION_REPEAT else VtKeys.ACTION_PRESS
        val bytes = VtNative.key(tab, key, mods, consumed, action, text, unshifted) ?: return true
        if (ctrlOnce || altOnce) {
            ctrlOnce = false
            altOnce = false
            onModifiersUsed?.invoke()
        }
        send(bytes)
        return true
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
     * arrives as deleteSurroundingText and becomes DEL.
     */
    private inner class Connection : BaseInputConnection(this, true) {
        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
            editable?.clear()
            preedit = ""
            invalidate()
            type(text.toString())
            return true
        }

        override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
            super.setComposingText(text, newCursorPosition)
            preedit = text.toString()
            invalidate()
            return true
        }

        override fun finishComposingText(): Boolean {
            val text = editable?.toString().orEmpty()
            super.finishComposingText()
            editable?.clear()
            preedit = ""
            invalidate()
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
    }

    // ---- Gestures ----

    private val gestures = GestureDetector(
        context,
        object : GestureDetector.SimpleOnGestureListener() {
            override fun onSingleTapUp(e: MotionEvent): Boolean {
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
                    VtNative.scroll(tab, n)
                    invalidate()
                }
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

    @SuppressLint("ClickableViewAccessibility") // Taps only open the keyboard
    override fun onTouchEvent(event: MotionEvent): Boolean {
        scaler.onTouchEvent(event)
        if (!scaler.isInProgress) gestures.onTouchEvent(event)
        return true
    }

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
    }
}
