package io.github.mthli.appine

import android.content.Context
import android.graphics.Color
import android.graphics.drawable.Drawable
import android.view.View
import com.google.android.material.color.MaterialColors
import kotlin.math.max

// Small helpers the app's screens and views share (the android/terminal map): sizes in dp and
// the theme's colours and drawables

/**
 * How thick the app's thin lines are (dp): the indicator under the tab bar's and the extra keys'
 * labels, and the terminal's progress line.
 */
internal const val LINE_DP = 1

/** [v] dp in whole pixels, rounded down. */
internal fun Context.dp(v: Int): Int = (v * resources.displayMetrics.density).toInt()

internal fun View.dp(v: Int): Int = context.dp(v)

/** [LINE_DP] in whole pixels, at least one. */
internal fun Context.line(): Int = max(1, dp(LINE_DP))

internal fun View.line(): Int = context.line()

/** The theme's colour [attr], white when the theme has none. */
internal fun Context.themeColor(attr: Int): Int = MaterialColors.getColor(this, attr, Color.WHITE)

internal fun View.themeColor(attr: Int): Int = context.themeColor(attr)

/** The theme's drawable [attr], null when the theme has none. */
internal fun Context.themeDrawable(attr: Int): Drawable? {
    val a = obtainStyledAttributes(intArrayOf(attr))
    try {
        return a.getDrawable(0)
    } finally {
        a.recycle()
    }
}

/** The theme's ripple for something that can be tapped. */
internal fun View.touchFeedback(): Drawable? = context.themeDrawable(android.R.attr.selectableItemBackground)
