package io.github.mthli.appine

import java.nio.ByteBuffer
import java.nio.ByteOrder
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class VtRendererTest {
    @Test
    fun concealSkipsTheForegroundForEveryDecorationAndSearchMark() {
        val conceal = 1 shl 5
        for (underline in 0..5) {
            for (strikeAndOverline in 0..3) {
                for (mark in 0..2) {
                    val attrs = (underline shl 8) or (strikeAndOverline shl 6) or (mark shl 14)
                    assertTrue(VtRenderer.drawsForeground(attrs))
                    assertFalse(VtRenderer.drawsForeground(attrs or conceal))
                }
            }
        }
    }

    @Test
    fun glyphWidthCacheCachesPerPaintAndCodepointAndClearsOnFontChange() {
        val cache = GlyphWidthCache(initialCapacity = 16)
        val codepoints = intArrayOf(0x00E9, 0x2500, 0x6C34, 0x1F600, 0x10FFFF)
        var measures = 0

        for (paintIdx in 0..3) {
            for (cp in codepoints) {
                val expected = paintIdx * 100f + (cp and 0xFF)
                val first = cache.getOrPut(paintIdx, cp) {
                    measures++
                    expected
                }
                assertEquals(expected, first, 0f)
            }
        }
        assertEquals(4 * codepoints.size, measures)
        assertEquals(4 * codepoints.size, cache.size)

        // Second pass hits the cache without re-measuring.
        for (paintIdx in 0..3) {
            for (cp in codepoints) {
                val expected = paintIdx * 100f + (cp and 0xFF)
                val cached = cache.getOrPut(paintIdx, cp) {
                    measures++
                    -1f
                }
                assertEquals(expected, cached, 0f)
            }
        }
        assertEquals(4 * codepoints.size, measures)

        // Zero-width glyphs are also cached rather than treated as misses.
        var zeroMeasures = 0
        assertEquals(0f, cache.getOrPut(0, 0x200B) { zeroMeasures++; 0f }, 0f)
        assertEquals(0f, cache.getOrPut(0, 0x200B) { zeroMeasures++; 99f }, 0f)
        assertEquals(1, zeroMeasures)

        // Clearing invalidates all entries (used on font / wide-scale changes).
        cache.clear()
        assertEquals(0, cache.size)
        val remeasured = cache.getOrPut(2, 0x6C34) {
            measures++
            42.5f
        }
        assertEquals(42.5f, remeasured, 0f)
        assertEquals(4 * codepoints.size + 1, measures)
    }

    @Test
    fun glyphWidthCacheRehashesAndBoundsCapacity() {
        val cache = GlyphWidthCache(initialCapacity = 16)
        for (cp in 0x4E00 until 0x4E00 + 600) {
            val expected = (cp - 0x4E00) * 0.5f + 1f
            assertEquals(expected, cache.getOrPut(cp and 3, cp) { expected }, 0f)
        }
        assertEquals(600, cache.size)
        for (cp in 0x4E00 until 0x4E00 + 600) {
            val expected = (cp - 0x4E00) * 0.5f + 1f
            assertEquals(expected, cache.getOrPut(cp and 3, cp) { -1f }, 0f)
        }

        // Filling past MAX_CAPACITY resets cleanly without unbounded growth.
        val overflowCount = GlyphWidthCache.MAX_CAPACITY + 64
        for (i in 0 until overflowCount) {
            val cp = 0x2000 + i
            assertEquals(i.toFloat(), cache.getOrPut(0, cp) { i.toFloat() }, 0f)
        }
        assertTrue(cache.size < GlyphWidthCache.MAX_CAPACITY)
    }

    @Test
    fun encodeCodepointHandlesBmpAndSupplementarySurrogatePairs() {
        val buf = CharArray(2)
        val bmpSamples = intArrayOf('A'.code, 0x00E9, 0x6C34, 0xFFFF)
        for (cp in bmpSamples) {
            val count = VtRenderer.encodeCodepoint(cp, buf)
            assertEquals(1, count)
            assertEquals(String(Character.toChars(cp)), String(buf, 0, count))
        }

        val supplementarySamples = intArrayOf(0x10000, 0x1F600, 0x10EEEE, 0x10FFFF)
        for (cp in supplementarySamples) {
            val count = VtRenderer.encodeCodepoint(cp, buf)
            assertEquals(2, count)
            assertTrue(Character.isHighSurrogate(buf[0]))
            assertTrue(Character.isLowSurrogate(buf[1]))
            assertEquals(String(Character.toChars(cp)), String(buf, 0, count))
        }
    }

    @Test
    fun markOfSkipsLookupWhenRowUnmarkedAndInheritsOnWideTail() {
        val wide = VtRenderer.WIDE shl 12
        val spacerTail = 2 shl 12
        val match = VtRenderer.MARK_MATCH shl 14
        val selected = VtRenderer.MARK_SELECTED shl 14

        // Row 0 (cols 0..3): [wide+match, tail, narrow+selected, narrow]
        // Row 1 (cols 0..3): [tail (at col 0!), narrow, wide+selected, tail]
        val attrs = intArrayOf(
            wide or match, spacerTail, selected, 0,
            spacerTail, 0, wide or selected, spacerTail,
        )

        // Fast path: unmarked rows always return MARK_NONE.
        for (i in attrs.indices) {
            assertEquals(VtRenderer.MARK_NONE, VtRenderer.markOf(attrs, i, i % 4, rowMarked = false))
        }

        // Marked row 0: wide tail inherits MARK_MATCH from cell 0.
        assertEquals(VtRenderer.MARK_MATCH, VtRenderer.markOf(attrs, 0, 0, rowMarked = true))
        assertEquals(VtRenderer.MARK_MATCH, VtRenderer.markOf(attrs, 1, 1, rowMarked = true))
        assertEquals(VtRenderer.MARK_SELECTED, VtRenderer.markOf(attrs, 2, 2, rowMarked = true))
        assertEquals(VtRenderer.MARK_NONE, VtRenderer.markOf(attrs, 3, 3, rowMarked = true))

        // Marked row 1: tail at col 0 must NOT read attrs[3] from row 0.
        val attrsWithPrevMark = attrs.copyOf().also { it[3] = selected }
        assertEquals(VtRenderer.MARK_NONE, VtRenderer.markOf(attrsWithPrevMark, 4, 0, rowMarked = true))
        assertEquals(VtRenderer.MARK_SELECTED, VtRenderer.markOf(attrsWithPrevMark, 7, 3, rowMarked = true))
    }

    @Test
    fun computeGlyphScaleEnlargesWideAndShrinksOverflow() {
        assertEquals(1f, VtRenderer.computeGlyphScale(10f, span = 1, w = 10, wideScale = 1.2f), 0f)
        assertEquals(1.2f, VtRenderer.computeGlyphScale(16f, span = 2, w = 20, wideScale = 1.2f), 0f)
        // Overflow past w + 0.5f shrinks evenly to w / advance.
        assertEquals(20f / 25f, VtRenderer.computeGlyphScale(25f, span = 2, w = 20, wideScale = 1.2f), 1e-6f)
        assertEquals(10f / 14f, VtRenderer.computeGlyphScale(14f, span = 1, w = 10, wideScale = 1.2f), 1e-6f)

        // Batched canvas scale around (0, midY) with x = l / s maps every point identically to
        // per-glyph scale around (l, midY) with x = l.
        val s = 1.2f
        val l = 137.5f
        val dx = 11.25f
        val perGlyphX = l + s * ((l + dx) - l)
        val batchedX = s * (l / s + dx)
        assertEquals(perGlyphX, batchedX, 1e-4f)
    }

    @Test
    fun vtFrameTakeRowsTracksRowHasMarksAcrossUpdates() {
        val screen = VtScreen()
        screen.resize(cols = 4, rows = 2)
        assertFalse(screen.rowHasMarks[0])
        assertFalse(screen.rowHasMarks[1])
        assertFalse(screen.rowHasMarks[2])

        fun buildFrame(rowSpecs: List<Pair<Int, IntArray>>): VtFrame {
            val rowBytes = rowSpecs.sumOf { (_, attrs) -> 12 + attrs.size * 16 }
            val buf = ByteBuffer.allocate(64 + rowBytes).order(ByteOrder.LITTLE_ENDIAN)
            buf.putShort(0, 4) // cols
            buf.putShort(2, 2) // rows
            buf.putShort(24, rowSpecs.size.toShort()) // rowCount
            var pos = 64
            for ((y, rowAttrs) in rowSpecs) {
                buf.putShort(pos, y.toShort())
                buf.putShort(pos + 2, rowAttrs.size.toShort())
                buf.putInt(pos + 4, 0) // extra
                buf.putShort(pos + 8, 0xffff.toShort())
                buf.putShort(pos + 10, 0xffff.toShort())
                pos += 12
                for (a in rowAttrs) {
                    buf.putInt(pos, 'a'.code)
                    buf.putInt(pos + 4, 0)
                    buf.putInt(pos + 8, 0)
                    buf.putShort(pos + 12, a.toShort())
                    buf.putShort(pos + 14, 0)
                    pos += 16
                }
            }
            return VtFrame(buf)
        }

        val matchAttr = VtRenderer.MARK_MATCH shl 14
        val frame1 = buildFrame(
            listOf(
                0 to intArrayOf(0, 0, 0, 0),
                1 to intArrayOf(0, matchAttr, 0, 0),
                2 to intArrayOf(0, 0, 0, 0), // overscan row
            ),
        )
        frame1.takeRows(screen) {}
        assertFalse(screen.rowHasMarks[0])
        assertTrue(screen.rowHasMarks[1])
        assertFalse(screen.rowHasMarks[2])

        // Incremental frame clearing row 1's marks resets rowHasMarks[1].
        val frame2 = buildFrame(listOf(1 to intArrayOf(0, 0, 0, 0)))
        frame2.takeRows(screen) {}
        assertFalse(screen.rowHasMarks[0])
        assertFalse(screen.rowHasMarks[1])
    }

    @Test
    fun rowDisplayStateTracksValidityDirtyOverscanAndDiscard() {
        val state = RowDisplayState()
        state.resize(rows = 3)

        // Fresh grid: no row is valid or needs recording yet.
        for (y in 0..3) {
            assertFalse(state.isValid(y))
            assertFalse(state.needsRecord(y, hasDisplayList = false))
            assertFalse(state.needsRecord(y, hasDisplayList = true))
        }
        assertEquals(3, RowDisplayState.visibleRowCount(rows = 3, scrollPx = 0))
        assertEquals(4, RowDisplayState.visibleRowCount(rows = 3, scrollPx = 5))

        // First frame delivers viewport rows 0..2 and overscan row 3.
        for (y in 0..3) state.markDirty(y)
        for (y in 0..3) {
            assertTrue(state.isValid(y))
            assertTrue(state.needsRecord(y, hasDisplayList = true))
        }

        // Hardware draw with scrollPx == 0 records only rows 0..2; overscan row 3 stays dirty.
        for (y in 0 until RowDisplayState.visibleRowCount(3, 0)) state.markRecorded(y)
        for (y in 0..2) {
            assertFalse(state.needsRecord(y, hasDisplayList = true))
            // After RenderNode.discardDisplayList(), hasDisplayList is false -> must re-record.
            assertTrue(state.needsRecord(y, hasDisplayList = false))
        }
        assertTrue(state.needsRecord(3, hasDisplayList = true))

        // Scrolling reveals overscan row 3, which then records and clears dirty.
        state.markRecorded(3)
        assertFalse(state.needsRecord(3, hasDisplayList = true))

        // Incremental frame dirties only row 1; rows 0, 2, 3 remain clean.
        state.markDirty(1)
        assertFalse(state.needsRecord(0, hasDisplayList = true))
        assertTrue(state.needsRecord(1, hasDisplayList = true))
        assertFalse(state.needsRecord(2, hasDisplayList = true))
        assertFalse(state.needsRecord(3, hasDisplayList = true))
        state.markRecorded(1)

        // invalidateAll marks every valid row dirty, while leaving uninitialized rows invalid.
        state.resize(rows = 2)
        state.markRecorded(1) // no-op on EMPTY row
        assertFalse(state.isValid(1))
        state.markDirty(0)
        state.markRecorded(0)
        state.invalidateAll()
        assertTrue(state.needsRecord(0, hasDisplayList = true))
        assertFalse(state.isValid(1))
        assertFalse(state.needsRecord(1, hasDisplayList = false))

        // Out-of-bounds indices are safely ignored.
        state.markDirty(-1)
        state.markDirty(99)
        state.markRecorded(-1)
        state.markRecorded(99)
        assertFalse(state.isValid(-1))
        assertFalse(state.isValid(99))
    }
}
