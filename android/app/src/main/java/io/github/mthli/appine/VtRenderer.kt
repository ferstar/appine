package io.github.mthli.appine

import android.content.Context
import android.graphics.Canvas
import android.graphics.DashPathEffect
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RenderNode
import android.graphics.Typeface
import androidx.core.graphics.withClip
import kotlin.math.ceil
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/**
 * How [VtView] draws its [VtScreen] (the android/vtrender map): with JetBrains Mono on a fixed cell
 * grid, each row a frame brings recorded into a per-row hardware [RenderNode] display list (one per
 * grid row plus the overscan row, with direct software [Canvas] fallback when hardware acceleration
 * is unavailable), and the cursor and the input method's composing text drawn over it.
 *
 * Glyphs are placed cell by cell: runs of ASCII share one draw call (the font size is chosen so
 * their advance is exactly one cell), every other glyph is centred in its one or two cells,
 * enlarged evenly to fill two cells when it is wide (the setting, for CJK fallback glyphs narrower
 * than two cells) or shrunk evenly when it is wider, never stretched. Box drawing, block elements
 * and the Powerline triangles are drawn as shapes ([VtGlyphs]). Images programs show
 * ([VtImages]) are painted into the rows they reach, below the backgrounds, below the text or above
 * it as the frame places them.
 */
internal class VtRenderer(
    private val context: Context,
    private val screen: VtScreen,
    private val images: VtImages,
) {
    private val paints = arrayOf(
        textPaint(font("fonts/JetBrainsMono.ttf", 400)),
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

    var cellWidth = 1
        private set
    var cellHeight = 1
        private set
    var baseline = 0
        private set
    private var light = 1
    private var underlineY = 0
    private var strikeY = 0
    private var dotted = DashPathEffect(floatArrayOf(1f, 1f), 0f)
    private var dashed = DashPathEffect(floatArrayOf(3f, 2f), 0f)

    /**
     * How much glyphs in wide cells are enlarged: so that an ideograph's advance fills two cells
     * when the setting asks for it ([setFont]), else 1. The system's CJK fallback draws ideographs
     * one em wide, short of two cells of JetBrains Mono (1.2 em), which leaves wide gaps between
     * them; measured, so a font with its own ideographs two cells wide gets 1.
     */
    private var wideScale = 1f

    /** Advances of single codepoints, per paint (unboxed open-addressed cache). */
    private val widths = GlyphWidthCache()

    /** Per-row hardware display lists (plus the overscan row), null before the first [resize]. */
    private var rowNodes: Array<RenderNode>? = null
    private var gridWidth = 0
    private var gridHeight = 0
    private val rowState = RowDisplayState()
    private val runChars = CharArray(4096)
    /** Reusable UTF-16 scratch for a single codepoint (avoids per-cell String allocation). */
    private val glyphChars = CharArray(2)
    // Scratch state for drawRowContent / drawGlyph (UI thread only; avoids per-row Ref allocations).
    private var runStart = -1
    private var runLength = 0
    private var runPaint: Paint? = null
    private var runColor = 0
    private var runDecor = 0
    private var activeScale = 1f

    private fun font(path: String, weight: Int): Typeface =
        Typeface.Builder(context.assets, path).setFontVariationSettings("'wght' $weight").build()

    private fun textPaint(typeface: Typeface) =
        Paint(Paint.ANTI_ALIAS_FLAG or Paint.SUBPIXEL_TEXT_FLAG).apply {
            this.typeface = typeface
            // Ligatures would join cells
            fontFeatureSettings = "'calt' 0, 'liga' 0"
        }

    /**
     * Cell size for a font of [px] pixels: the width is the font's advance rounded to whole pixels,
     * and the text size is then adjusted so the advance is exactly that width (ASCII runs stay on
     * the grid). With [fillWide], glyphs in wide cells are enlarged to fill them.
     */
    fun setFont(px: Float, fillWide: Boolean) {
        for (p in paints) p.textSize = px
        val advance = paints[0].measureText("M")
        cellWidth = max(1, advance.roundToInt())
        val size = px * cellWidth / advance
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

    /**
     * Fits the screen and the per-row [RenderNode]s to a grid of [cols] × [rows] cells, and the
     * overscan row; false when they fit it already.
     */
    fun resize(cols: Int, rows: Int): Boolean {
        val bw = cols * cellWidth
        val bh = (rows + 1) * cellHeight
        if (cols == screen.cols && rows == screen.rows && gridWidth == bw && gridHeight == bh && rowNodes != null) {
            return false
        }
        screen.resize(cols, rows)
        gridWidth = bw
        gridHeight = bh
        rowState.resize(rows)
        // Allocate fresh nodes instead of discarding the old ones in place so an active
        // height-shrink preview RenderNode referencing the prior grid stays intact until cleared.
        rowNodes = Array(rows + 1) { y ->
            RenderNode("vt-row-$y").apply {
                val top = y * cellHeight
                setPosition(0, top, bw, top + cellHeight)
                setClipToBounds(true)
            }
        }
        return true
    }

    /** Whether there is a grid to draw ([resize]). */
    val hasGrid: Boolean get() = rowNodes != null

    /** Takes [frame]'s rows into the screen and marks them dirty for the next draw. */
    fun takeRows(frame: VtFrame) {
        if (rowNodes == null) return
        frame.takeRows(screen) { y -> rowState.markDirty(y) }
    }

    /** Marks every painted row dirty so it re-records on the next draw (default colour or image changes). */
    fun invalidateRows() {
        rowState.invalidateAll()
    }

    /** Discards cached row display lists so references to dropped [VtImages] bitmaps are released. */
    fun discardDisplayLists() {
        rowNodes?.forEach(RenderNode::discardDisplayList)
    }

    private fun recordRow(node: RenderNode, y: Int) {
        val rc = node.beginRecording()
        try {
            val top = y * cellHeight
            if (top != 0) rc.translate(0f, -top.toFloat())
            drawRowContent(rc, y, screen.cols, top)
        } finally {
            node.endRecording()
        }
    }

    private fun drawRow(c: Canvas, y: Int) {
        val cols = screen.cols
        val top = y * cellHeight
        c.withClip(0, top, cols * cellWidth, top + cellHeight) {
            drawRowContent(c, y, cols, top)
        }
    }

    private fun drawRowContent(c: Canvas, y: Int, cols: Int, top: Int) {
        val cp = screen.cp
        val attrs = screen.attrs
        val clusters = screen.clusters
        val base = y * cols
        val rowMarked = screen.rowHasMarks[y]
        // The view canvas already holds opaque screen.defaultBg; images below the backgrounds
        // show through cells of the default one without an extra full-row background fill.
        images.draw(c, VtImages.LAYER_BELOW_BACKGROUND, top, cellHeight)
        if (cols > 0) {
            var x = 0
            var color = bgOf(base, 0, rowMarked)
            while (x < cols) {
                var end = x + 1
                var nextColor = color
                while (end < cols) {
                    nextColor = bgOf(base + end, end, rowMarked)
                    if (nextColor != color) break
                    end++
                }
                if (color != screen.defaultBg) {
                    fill.color = color
                    c.drawRect((x * cellWidth).toFloat(), top.toFloat(), (end * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
                }
                x = end
                color = nextColor
            }
        }
        images.draw(c, VtImages.LAYER_BELOW_TEXT, top, cellHeight)
        val from = screen.selFrom[y]
        if (from >= 0) {
            // The highlight covers a wide character whose first cell ends the selection
            val to = screen.selTo[y] + screen.span(base + screen.selTo[y])
            fill.color = SELECTION
            c.drawRect((from * cellWidth).toFloat(), top.toFloat(), (min(to, cols) * cellWidth).toFloat(), (top + cellHeight).toFloat(), fill)
        }

        // Runs of plain ASCII in one style go out in one call; scaled non-ASCII glyphs share
        // one canvas scale around (0, midY) across consecutive cells with the same scale.
        runStart = -1
        runLength = 0
        runPaint = null
        runColor = 0
        runDecor = 0
        val baselineY = (top + baseline).toFloat()
        var x = 0
        while (x < cols) {
            val i = base + x
            val a = attrs[i]
            val wide = (a shr 12) and 3
            if (wide == SPACER_TAIL || wide == SPACER_HEAD) {
                x++
                continue
            }
            val span = if (wide == WIDE) 2 else 1
            if (!drawsForeground(a)) {
                flushRun(c, top, baselineY)
                x += span
                continue
            }
            val code = cp[i]
            val decor = a and DECORATIONS
            val cluster = clusters[i]
            val plain = cluster == null && wide == NARROW &&
                (code == 0 || code in 0x20..0x7e)
            if (plain) {
                val blank = decor == 0 && (code == 0 || code == ' '.code)
                if (runLength == 0 && blank) {
                    x++
                    continue
                }
                val color = fgOf(i, x, rowMarked)
                val paint = paints[a and (BOLD or ITALIC)]
                if (runLength > 0 && (paint !== runPaint || color != runColor || decor != runDecor || runLength == runChars.size)) {
                    flushRun(c, top, baselineY)
                    if (blank) {
                        x++
                        continue
                    }
                }
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
            flushRun(c, top, baselineY)
            val color = fgOf(i, x, rowMarked)
            drawGlyph(c, i, x, span, top, color, a and (BOLD or ITALIC))
            if (decor != 0) {
                endScale(c)
                decorate(c, decor, x, x + span, top, color)
            }
            x += span
        }
        flushRun(c, top, baselineY)
        endScale(c)
        images.draw(c, VtImages.LAYER_ABOVE_TEXT, top, cellHeight)
    }

    private fun endScale(c: Canvas) {
        if (activeScale != 1f) {
            c.restore()
            activeScale = 1f
        }
    }

    private fun setScale(c: Canvas, s: Float, midY: Float) {
        if (s == activeScale) return
        endScale(c)
        if (s != 1f) {
            c.save()
            c.scale(s, s, 0f, midY)
            activeScale = s
        }
    }

    private fun flushRun(c: Canvas, top: Int, baselineY: Float) {
        if (runLength > 0) {
            endScale(c)
            if (runDecor == 0) {
                while (runLength > 0 && runChars[runLength - 1] == ' ') runLength--
            }
            if (runLength > 0) {
                val paint = runPaint!!
                paint.color = runColor
                c.drawText(runChars, 0, runLength, (runStart * cellWidth).toFloat(), baselineY, paint)
                decorate(c, runDecor, runStart, runStart + runLength, top, runColor)
            }
        }
        runStart = -1
        runLength = 0
    }

    /** A cell's background: a search match's colour, else its own (inverse swaps it with the text's). */
    private fun bgOf(i: Int, col: Int, rowMarked: Boolean): Int {
        if (rowMarked) {
            when (markOf(screen.attrs, i, col, true)) {
                MARK_MATCH -> return SEARCH_MATCH
                MARK_SELECTED -> return SEARCH_SELECTED
            }
        }
        val s = screen
        return if (s.attrs[i] and INVERSE != 0) s.fg[i].takeIf { it != 0 } ?: s.defaultFg else s.bg[i].takeIf { it != 0 } ?: s.defaultBg
    }

    /** A cell's text colour: black on a search match, as in Ghostty, else its own. */
    private fun fgOf(i: Int, col: Int, rowMarked: Boolean): Int {
        if (rowMarked && markOf(screen.attrs, i, col, true) != MARK_NONE) return SEARCH_TEXT
        val s = screen
        val a = s.attrs[i]
        val color = if (a and INVERSE != 0) s.bg[i].takeIf { it != 0 } ?: s.defaultBg else s.fg[i].takeIf { it != 0 } ?: s.defaultFg
        return if (a and FAINT != 0) (color and 0x00ffffff) or 0x80000000.toInt() else color
    }

    private fun drawGlyphText(
        c: Canvas,
        cluster: String?,
        charCount: Int,
        x: Float,
        y: Float,
        paint: Paint,
    ) {
        if (cluster != null) {
            c.drawText(cluster, x, y, paint)
        } else {
            c.drawText(glyphChars, 0, charCount, x, y, paint)
        }
    }

    /**
     * One cell's glyph, centred in its [span] cells: a wide one enlarged by [wideScale], and any
     * shrunk evenly to fit them. May leave a batched canvas scale active in [activeScale]; callers
     * must call [endScale] before unscaled drawing or leaving the row/clip.
     */
    private fun drawGlyph(c: Canvas, i: Int, x: Int, span: Int, top: Int, color: Int, paintIdx: Int) {
        val code = screen.cp[i]
        // With or without the diacritics that say which part of the image the cell shows
        if (code == KITTY_PLACEHOLDER) return
        val cluster = screen.clusters[i]
        val left = x * cellWidth
        val w = span * cellWidth
        if (cluster == null && glyphs.covers(code)) {
            endScale(c)
            glyphs.draw(c, code, left, top, w, cellHeight, light, color)
            return
        }
        if (cluster == null && (code == 0 || code == ' '.code)) return
        val paint = paints[paintIdx]
        var charCount = 0
        val advance = if (cluster != null) {
            paint.measureText(cluster)
        } else {
            charCount = encodeCodepoint(code, glyphChars)
            widths.getOrPut(paintIdx, code) { paint.measureText(glyphChars, 0, charCount) }
        }
        paint.color = color
        val s = computeGlyphScale(advance, span, w, wideScale)
        val l = left + (w - advance * s) / 2f
        val baselineY = (top + baseline).toFloat()
        setScale(c, s, top + cellHeight / 2f)
        drawGlyphText(c, cluster, charCount, if (s == 1f) l else l / s, baselineY, paint)
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

    /** The rows as last painted, [scrollPx] above their place, clipped to the grid's whole cells. */
    fun drawGrid(c: Canvas, scrollPx: Int) {
        val nodes = rowNodes ?: return
        val visibleRows = RowDisplayState.visibleRowCount(screen.rows, scrollPx)
        c.withClip(0, 0, screen.cols * cellWidth, screen.rows * cellHeight) {
            if (scrollPx != 0) c.translate(0f, -scrollPx.toFloat())
            if (c.isHardwareAccelerated) {
                for (y in 0 until visibleRows) {
                    if (!rowState.isValid(y)) continue
                    val node = nodes[y]
                    if (rowState.needsRecord(y, node.hasDisplayList())) {
                        recordRow(node, y)
                        rowState.markRecorded(y)
                    }
                    c.drawRenderNode(node)
                }
            } else {
                for (y in 0 until visibleRows) {
                    if (rowState.isValid(y)) drawRow(c, y)
                }
            }
        }
    }

    /**
     * The cursor in its shape (a block hollow without the view's [focused] focus), or the lock
     * while a password is read, unless the IME is [composing]; nothing when it is hidden or outside
     * the grid.
     */
    fun drawCursor(c: Canvas, focused: Boolean, composing: Boolean) {
        val s = screen
        if (!s.cursorShown || s.cursorX >= s.cols || s.cursorY >= s.rows) return
        val i = s.cursorY * s.cols + s.cursorX
        val span = s.span(i)
        val l = s.cursorX * cellWidth
        val t = s.cursorY * cellHeight
        val r = l + span * cellWidth
        val b = t + cellHeight
        val color = if (s.cursorColor != 0) s.cursorColor else s.defaultFg
        fill.color = color
        stroke.color = color
        when {
            // Composing text shows a block, as in Ghostty
            s.cursorStyle == VtNative.CURSOR_LOCK && !composing -> drawLock(c, l, t, r, b, color)
            s.cursorStyle == VtNative.CURSOR_BAR -> c.drawRect(l.toFloat(), t.toFloat(), (l + 2 * light).toFloat(), b.toFloat(), fill)
            s.cursorStyle == VtNative.CURSOR_UNDERLINE -> c.drawRect(l.toFloat(), (b - 2 * light).toFloat(), r.toFloat(), b.toFloat(), fill)
            s.cursorStyle == VtNative.CURSOR_HOLLOW || !focused ->
                c.drawRect(l + light / 2f, t + light / 2f, r - light / 2f, b - light / 2f, stroke)
            else -> {
                // A block: the cell's glyph in the background colour on top
                c.withClip(l, t, r, b) {
                    c.drawRect(l.toFloat(), t.toFloat(), r.toFloat(), b.toFloat(), fill)
                    val a = s.attrs[i]
                    if (drawsForeground(a)) {
                        drawGlyph(
                            c, i, s.cursorX, span, t,
                            bgOf(i, s.cursorX, s.rowHasMarks[s.cursorY]),
                            a and (BOLD or ITALIC),
                        )
                        endScale(c)
                    }
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

    /** How wide [text] is in the regular paint (the composing text). */
    fun advance(text: String): Float = paints[0].measureText(text)

    /** The input method's composing [text] at the cursor, underlined. */
    fun drawPreedit(c: Canvas, text: String) {
        val s = screen
        if (text.isEmpty() || s.cursorY >= s.rows) return
        val p = paints[0]
        val w = p.measureText(text)
        // Shifted left when it would run past the right edge
        val l = min((s.cursorX * cellWidth).toFloat(), s.cols * cellWidth - w).coerceAtLeast(0f)
        val t = (s.cursorY * cellHeight).toFloat()
        fill.color = s.defaultBg
        c.drawRect(l, t, l + w, t + cellHeight, fill)
        p.color = s.defaultFg
        c.drawText(text, l, t + baseline, p)
        fill.color = s.defaultFg
        c.drawRect(l, t + underlineY, l + w, t + underlineY + light, fill)
    }

    companion object {
        /** SGR 8 conceals the glyph and every decoration over the cell background. */
        fun drawsForeground(attrs: Int): Boolean = attrs and INVISIBLE == 0

        /**
         * A cell's search mark (vt.c's attrs bits 14 and 15): [MARK_MATCH], [MARK_SELECTED] or
         * [MARK_NONE]. Skips all lookups when [rowMarked] is false. The second cell of a wide
         * character takes the first's, so a match ending on one covers the whole character.
         */
        fun markOf(attrs: IntArray, i: Int, col: Int, rowMarked: Boolean): Int {
            if (!rowMarked) return MARK_NONE
            val a = attrs[i]
            val own = (a shr 14) and 3
            if (own != MARK_NONE || (a shr 12) and 3 != SPACER_TAIL || col == 0) return own
            return (attrs[i - 1] shr 14) and 3
        }

        /**
         * Uniform scale factor for a glyph of measured [advance] in a cell span of [w] pixels:
         * wide cells enlarge by [wideScale], and any glyph wider than [w] + 0.5 px shrinks evenly
         * to fit.
         */
        fun computeGlyphScale(advance: Float, span: Int, w: Int, wideScale: Float): Float {
            var s = if (span == 2) wideScale else 1f
            if (advance * s > w + 0.5f) s = w / advance
            return s
        }

        /**
         * Encodes [codepoint] into [dst] without allocating a [String], returning the UTF-16
         * code-unit count (1 for BMP, 2 for supplementary surrogate pairs).
         */
        fun encodeCodepoint(codepoint: Int, dst: CharArray): Int =
            Character.toChars(codepoint, dst, 0)

        // Cell attributes (vt.c)
        private const val BOLD = 1
        private const val ITALIC = 2
        private const val FAINT = 1 shl 2
        private const val INVERSE = 1 shl 4
        private const val INVISIBLE = 1 shl 5
        private const val STRIKE = 1 shl 6
        private const val OVERLINE = 1 shl 7
        private const val DECORATIONS = STRIKE or OVERLINE or (7 shl 8)
        private const val NARROW = 0
        const val WIDE = 1
        private const val SPACER_TAIL = 2
        private const val SPACER_HEAD = 3

        // GhosttySgrUnderline
        private const val UNDERLINE_DOUBLE = 2
        private const val UNDERLINE_CURLY = 3
        private const val UNDERLINE_DOTTED = 4
        private const val UNDERLINE_DASHED = 5

        /**
         * The Kitty graphics placeholder (U+10EEEE): its cells show part of an image, which the
         * frame places, and are drawn blank, as Ghostty draws them.
         */
        private const val KITTY_PLACEHOLDER = 0x10EEEE

        // A cell's search mark (attrs bits 14 and 15)
        internal const val MARK_NONE = 0
        internal const val MARK_MATCH = 1
        internal const val MARK_SELECTED = 2

        /** The selection's highlight, over cell backgrounds and under text. */
        private const val SELECTION = 0x604A90E2

        /**
         * Search matches, as Ghostty draws them (its search-background, search-selected-background
         * and search-foreground): black text on yellow, the selected match on orange.
         */
        const val SEARCH_MATCH = 0xFFFFE082.toInt()
        const val SEARCH_SELECTED = 0xFFF2A57E.toInt()
        private const val SEARCH_TEXT = 0xFF000000.toInt()
    }
}

/**
 * Primitive open-addressed cache of single-codepoint glyph advances, keyed by
 * `(paintIdx shl 21) or codepoint` (2-bit paint index + 21-bit Unicode scalar value).
 * Avoids `Long`/`Float` boxing, clears on font change, and bounds capacity at [MAX_CAPACITY].
 */
internal class GlyphWidthCache(initialCapacity: Int = INITIAL_CAPACITY) {
    private var keys: IntArray
    private var values: FloatArray
    private var mask: Int
    private var growAt: Int
    var size = 0
        private set

    init {
        val cap = powerOfTwo(initialCapacity.coerceIn(MIN_CAPACITY, MAX_CAPACITY))
        keys = IntArray(cap) { EMPTY }
        values = FloatArray(cap)
        mask = cap - 1
        growAt = (cap * 7) / 10
    }

    fun clear() {
        if (size == 0) return
        keys.fill(EMPTY)
        size = 0
    }

    inline fun getOrPut(paintIdx: Int, codepoint: Int, measure: () -> Float): Float {
        val key = packKey(paintIdx, codepoint)
        val existing = get(key)
        if (!existing.isNaN()) return existing
        val measured = measure()
        put(key, measured)
        return measured
    }

    fun get(key: Int): Float {
        val k = keys
        val m = mask
        var slot = mix(key) and m
        while (true) {
            val cur = k[slot]
            if (cur == key) return values[slot]
            if (cur == EMPTY) return Float.NaN
            slot = (slot + 1) and m
        }
    }

    fun put(key: Int, value: Float) {
        if (size >= growAt) {
            if (keys.size >= MAX_CAPACITY) {
                clear()
            } else {
                rehash(keys.size shl 1)
            }
        }
        val k = keys
        val m = mask
        var slot = mix(key) and m
        while (true) {
            val cur = k[slot]
            if (cur == EMPTY) {
                k[slot] = key
                values[slot] = value
                size++
                return
            }
            if (cur == key) {
                values[slot] = value
                return
            }
            slot = (slot + 1) and m
        }
    }

    private fun rehash(newCap: Int) {
        val oldKeys = keys
        val oldValues = values
        val newKeys = IntArray(newCap) { EMPTY }
        val newValues = FloatArray(newCap)
        val newMask = newCap - 1
        for (i in oldKeys.indices) {
            val key = oldKeys[i]
            if (key == EMPTY) continue
            var slot = mix(key) and newMask
            while (newKeys[slot] != EMPTY) slot = (slot + 1) and newMask
            newKeys[slot] = key
            newValues[slot] = oldValues[i]
        }
        keys = newKeys
        values = newValues
        mask = newMask
        growAt = (newCap * 7) / 10
    }

    companion object {
        private const val EMPTY = -1
        private const val MIN_CAPACITY = 16
        private const val INITIAL_CAPACITY = 256
        internal const val MAX_CAPACITY = 16384

        fun packKey(paintIdx: Int, codepoint: Int): Int =
            ((paintIdx and 3) shl 21) or (codepoint and 0x1FFFFF)

        private fun mix(key: Int): Int {
            val h = key * -1640531527
            return h xor (h ushr 16)
        }

        private fun powerOfTwo(n: Int): Int {
            var v = 1
            while (v < n) v = v shl 1
            return v
        }
    }
}

/**
 * Per-row state for [VtRenderer]'s [RenderNode] display lists (one slot per viewport row plus the
 * overscan row). Slots start [EMPTY] after [resize], become [DIRTY] as frames deliver them, stay
 * [DIRTY] while unrecorded (e.g. the overscan row while `scrollPx == 0` or after software-only
 * draws), and re-record whenever [DIRTY] or after `discardDisplayList`.
 */
internal class RowDisplayState {
    private var states = ByteArray(0)

    fun resize(rows: Int) {
        states = ByteArray(rows + 1)
    }

    fun markDirty(y: Int) {
        if (y in states.indices) states[y] = DIRTY
    }

    fun invalidateAll() {
        for (y in states.indices) {
            if (states[y] == CLEAN) states[y] = DIRTY
        }
    }

    fun isValid(y: Int): Boolean = y in states.indices && states[y] != EMPTY

    fun needsRecord(y: Int, hasDisplayList: Boolean): Boolean =
        y in states.indices && (states[y] == DIRTY || (states[y] == CLEAN && !hasDisplayList))

    fun markRecorded(y: Int) {
        if (y in states.indices && states[y] != EMPTY) states[y] = CLEAN
    }

    companion object {
        private const val EMPTY: Byte = 0
        private const val DIRTY: Byte = 1
        private const val CLEAN: Byte = 2

        fun visibleRowCount(rows: Int, scrollPx: Int): Int =
            if (scrollPx > 0) rows + 1 else rows
    }
}
