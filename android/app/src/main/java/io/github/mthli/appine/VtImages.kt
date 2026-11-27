package io.github.mthli.appine

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Paint
import android.graphics.Rect
import androidx.core.graphics.createBitmap

/**
 * The Kitty images a tab's frames show (the android/vtrender map): the last frame's placements in
 * drawing order (from a pool kept across frames), which [VtRenderer] paints into the rows they
 * reach, and the bitmaps of the images they show by id, copied from libghostty-vt
 * ([VtNative.image]) when their pixels change and dropped once no placement shows them.
 */
internal class VtImages {

    /** Where a frame draws an image, or part of it (vt.c's image placements). */
    class Placement {
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

    /** An image's bitmap and the [generation] of its pixels it holds. */
    private class Picture(val bitmap: Bitmap, val generation: Long)

    private val placements = ArrayList<Placement>()
    private var count = 0
    private val pictures = HashMap<Int, Picture>()
    private val paint = Paint(Paint.FILTER_BITMAP_FLAG)

    /**
     * Takes [frame]'s image placements and copies the pixels of the images they show that are not
     * held at their generation yet from [tab]'s terminal; bitmaps no placement shows are dropped.
     * True when new pixels arrived so callers can invalidate cached row display lists.
     */
    fun take(tab: Long, frame: VtFrame): Boolean {
        val n = frame.imageCount
        while (placements.size < n) placements.add(Placement())
        count = n
        var arrived = false
        for (k in 0 until n) {
            val p = placements[k]
            frame.placement(k, p)
            val held = pictures[p.id]
            if (held != null && held.generation == p.generation) continue
            if (copy(tab, p)) arrived = true
        }
        val it = pictures.entries.iterator()
        while (it.hasNext()) {
            val e = it.next()
            if (!shown(e.key)) it.remove()
        }
        return arrived
    }

    /**
     * Copies the pixels of [p]'s image at its generation into a fresh bitmap so any hardware
     * [android.graphics.RenderNode] referencing an earlier generation on RenderThread never races
     * with in-place pixel writes; false when they cannot be had now (a later frame names a newer
     * generation), or the bitmap cannot be made.
     */
    private fun copy(tab: Long, p: Placement): Boolean {
        if (p.imageWidth <= 0 || p.imageHeight <= 0) return false
        val bitmap = try {
            createBitmap(p.imageWidth, p.imageHeight)
        } catch (_: OutOfMemoryError) {
            null
        } catch (_: IllegalArgumentException) {
            null
        } ?: run {
            pictures.remove(p.id)
            return false
        }
        val copied = VtNative.image(tab, p.id, p.generation, bitmap)
        if (!copied) {
            // Safe to recycle: this newly allocated bitmap was never handed to any Canvas/RenderNode.
            bitmap.recycle()
            pictures.remove(p.id)
            return false
        }
        pictures[p.id] = Picture(bitmap, p.generation)
        return true
    }

    /** Whether one of the last frame's placements shows image [id]. */
    private fun shown(id: Int): Boolean {
        for (k in 0 until count) if (placements[k].id == id) return true
        return false
    }

    /**
     * The images of [layer] that reach into the row at [top], [height] pixels high, to which the
     * canvas is clipped, in the frame's order; images whose pixels are not held at the
     * placement's generation are left out.
     */
    fun draw(c: Canvas, layer: Int, top: Int, height: Int) {
        for (k in 0 until count) {
            val p = placements[k]
            if (p.layer != layer || p.dst.bottom <= top || p.dst.top >= top + height) continue
            val picture = pictures[p.id] ?: continue
            if (picture.generation != p.generation) continue
            c.drawBitmap(picture.bitmap, p.src, p.dst, paint)
        }
    }

    /**
     * Forgets the images: when another tab is attached, whose image ids do not name them, or while
     * the window is hidden, as libghostty-vt still holds their pixels, which the next frame copies
     * again.
     */
    fun drop() {
        count = 0
        pictures.clear()
    }

    companion object {
        // An image placement's layer (GhosttyImageLayer)
        const val LAYER_BELOW_BACKGROUND = 0
        const val LAYER_BELOW_TEXT = 1
        const val LAYER_ABOVE_TEXT = 2
    }
}
