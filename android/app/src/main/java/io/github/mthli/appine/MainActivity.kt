package io.github.mthli.appine

import android.Manifest
import android.animation.ValueAnimator
import android.app.Activity
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Rect
import android.graphics.drawable.Drawable
import android.graphics.drawable.InsetDrawable
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.util.TypedValue
import android.view.Gravity
import android.view.KeyEvent
import android.view.KeyboardShortcutGroup
import android.view.KeyboardShortcutInfo
import android.view.Menu
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.ViewTreeObserver
import android.view.animation.PathInterpolator
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.widget.PopupMenu
import androidx.core.animation.doOnEnd
import androidx.core.content.edit
import androidx.core.graphics.ColorUtils
import androidx.core.graphics.drawable.toDrawable
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsAnimationCompat
import androidx.core.view.WindowInsetsCompat
import com.google.android.material.color.DynamicColors
import com.google.android.material.color.MaterialColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import kotlin.math.abs
import kotlin.math.roundToInt

/**
 * The terminal screen: the tab bar on top, the terminal ([VtView]) in the middle, and below it the
 * keys the soft keyboard lacks (Esc, Ctrl, arrows and so on).
 * Sessions belong to [TerminalService]; this only handles display and input: the view shows the
 * current tab's [VtSession].
 *
 * The tab bar is one row in any window: a scrolling row of tabs, each a square with its number
 * (a long press shows its title), with `×` (closes the current tab), `+` and `⋮` over its end; the
 * tabs fade out into the left part of `×` while more of them lie beyond. Scrolled away from the
 * current tab, the row brings it back into view a few seconds after it comes to rest. When tabs
 * close, the ones after them slide into their places and then change their numbers.
 *
 * Its dialogs and menus are Material 3 (`Theme.Appine.Terminal`, always dark); the tab bar and the
 * extra keys are black like the terminal, with labels in the theme's colours. The activity itself
 * stays a framework one, and so do the views it builds.
 */
class MainActivity : Activity(), TerminalService.Listener {

    private lateinit var screen: VtView
    private lateinit var strip: HorizontalScrollView
    private lateinit var tabs: LinearLayout
    private lateinit var closeButton: TextView
    private lateinit var status: TextView
    private lateinit var keys: ExtraKeysView

    private var service: TerminalService? = null
    private var current: VtSession? = null

    /** [current]'s position when the tabs were last shown, where its successor is taken from. */
    private var currentIndex = 0

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
            tabs.viewTreeObserver.removeOnPreDrawListener(this)
            val from = slideFrom ?: return true
            slideFrom = null
            for ((tab, x) in from) {
                val shift = x - (tab.view.left - strip.scrollX)
                tab.view.translationX += shift
                // A tab whose place stays on screen but whose number changes waits as long as
                // those that slide (a 0 to 0 slide); one still sliding renumbers at its end
                if (tab in rowTabs && (shift != 0 || tab.stale && tab.view.translationX == 0f)) tab.slide()
            }
            moveIndicator(closed = from.keys.any { it !in rowTabs })
            return true
        }
    }

    /** Whether a finger is on the tab row, which holds off [returnToCurrentTab]. */
    private var stripTouched = false

    /** Brings the current tab back into view a while after the row was scrolled away from it. */
    private val returnToCurrentTab = Runnable { revealCurrentTab() }

    private var fontSize = 0

    /** The last toast ([showToast]), cancelled by the next. */
    private var toast: Toast? = null

    /** The key of the shortcut last taken ([dispatchKeyEvent]), whose release is taken too. */
    private var shortcutKey = 0

    // The theme's colours, from the wallpaper on Android 12 and later, so read after onCreate has
    // applied them. The tab bar, the extra keys and what lies under the system bars are black, as
    // the terminal is

    /** The bar's buttons, other tabs' numbers and the status text. */
    private val mutedColor by lazy { themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant) }

    /** The current tab's number and indicator. */
    private val currentTabColor by lazy { themeColor(androidx.appcompat.R.attr.colorPrimary) }
    private val indicatorPaint by lazy { ExtraKeysView.indicatorPaint(currentTabColor) }

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val s = (binder as TerminalService.LocalBinder).service
            service = s
            s.addListener(this@MainActivity)
            when {
                s.machine != 0L || s.startError != null -> onMachineReady(s.startError)
                s.progress != null -> onProgress(s.progress!!)
            }
            onNotice()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            service = null
        }
    }

    /**
     * Moves the extra keys with the IME as it shows or hides. The system lays the root out for the
     * end as the animation starts (so the terminal regrids once, at the start), and the keys are
     * drawn from where they were on screen to their new place along the IME's own curve: as it
     * shows, they rise through the IME's room (black) to the terminal's new bottom; as it hides,
     * they sink over the terminal's new rows, which they hide while passing, being black.
     */
    private val keysFollowIme = object : WindowInsetsAnimationCompat.Callback(DISPATCH_MODE_STOP) {
        /** The IME animation being followed; an earlier one ending late leaves the keys alone. */
        private var following: WindowInsetsAnimationCompat? = null

        /** The keys' bottom on screen before the change (relative to the root). */
        private var startBottom = 0f

        /** How far from their place the keys start: below it as the IME shows, above as it hides. */
        private var shift = 0f

        override fun onPrepare(animation: WindowInsetsAnimationCompat) {
            if (!isIme(animation)) return
            following = animation
            // Where they are drawn, which is not their place while an earlier animation ran
            startBottom = keys.bottom + keys.translationY
        }

        override fun onStart(
            animation: WindowInsetsAnimationCompat,
            bounds: WindowInsetsAnimationCompat.BoundsCompat,
        ): WindowInsetsAnimationCompat.BoundsCompat {
            if (animation === following) {
                shift = startBottom - keys.bottom
                keys.translationY = shift
            }
            return bounds
        }

        override fun onProgress(
            insets: WindowInsetsCompat,
            runningAnimations: List<WindowInsetsAnimationCompat>,
        ): WindowInsetsCompat {
            following?.takeIf { it in runningAnimations }?.let {
                keys.translationY = shift * (1 - it.interpolatedFraction)
            }
            return insets
        }

        override fun onEnd(animation: WindowInsetsAnimationCompat) {
            if (animation !== following) return
            following = null
            keys.translationY = 0f
        }

        private fun isIme(animation: WindowInsetsAnimationCompat) =
            animation.typeMask and WindowInsetsCompat.Type.ime() != 0
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        DynamicColors.applyToActivityIfAvailable(this)
        super.onCreate(savedInstanceState)
        fontSize = getPreferences(MODE_PRIVATE).getInt(PREF_FONT_SIZE, dp(12))
        screen = VtView(this, Host()).apply {
            setFontSize(fontSize.toFloat())
            setBackgroundColor(Color.BLACK)
        }
        status = TextView(this).apply {
            setTextColor(mutedColor)
            gravity = Gravity.CENTER
            setText(R.string.starting)
        }
        tabs = object : LinearLayout(this) {
            // Over the tabs, as the extra keys draw theirs
            override fun dispatchDraw(canvas: Canvas) {
                super.dispatchDraw(canvas)
                currentTab?.let { ExtraKeysView.drawIndicator(canvas, this, it, indicatorPaint, indicatorShift) }
            }
        }.apply { orientation = LinearLayout.HORIZONTAL }
        // The row runs under the buttons to the bar's end. Its end padding, as wide as the buttons,
        // lets the last tab scroll out from under them. While tabs lie beyond the padding, the
        // right fading edge hints that the row scrolls: a padding offset carries it into the blank
        // left part of ×, so the tabs fade out under the button rather than at its edge. Scrolled
        // away from the current tab, the row brings it back once it has rested for a while
        strip = object : HorizontalScrollView(this) {
            override fun getLeftFadingEdgeStrength() = 0f
            override fun isPaddingOffsetRequired() = true
            override fun getRightPaddingOffset() = dp(FADE_INTO_CLOSE_DP)

            // A finger on the row, on a tab too, holds the return off; lifting it starts the wait
            override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
                when (ev.actionMasked) {
                    MotionEvent.ACTION_DOWN -> {
                        stripTouched = true
                        removeCallbacks(returnToCurrentTab)
                    }
                    MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> {
                        stripTouched = false
                        returnToCurrentTabLater()
                    }
                }
                return super.dispatchTouchEvent(ev)
            }

            // Every move without a finger (a fling, TalkBack's scroll actions) starts it again
            override fun onScrollChanged(l: Int, t: Int, oldl: Int, oldt: Int) {
                super.onScrollChanged(l, t, oldl, oldt)
                if (!stripTouched) returnToCurrentTabLater()
            }
        }.apply {
            isHorizontalScrollBarEnabled = false
            isHorizontalFadingEdgeEnabled = true
            setFadingEdgeLength(dp(ExtraKeysView.FADE_DP))
            clipToPadding = false
            // A closed last tab fades out past the end of the shortened row (Tab.leave)
            clipChildren = false
            addView(tabs, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.MATCH_PARENT))
        }
        // The multiplication sign, which Roboto draws like its +; U+2715 (✕) is not in Roboto, and
        // the fallback font's is larger and thinner than + and ⋮
        closeButton = barButton("×") { current?.let { confirmClose(listOf(it)) } }.apply {
            contentDescription = getString(R.string.close_tab)
        }
        keys = ExtraKeysView(this, Keys())

        // Black but for the left part of × the fade reaches into, so the tabs under the buttons are
        // hidden, and taking every touch there, so a tab under the hidden × cannot be tapped;
        // TalkBack skips the group and reads each button
        val buttons = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            background = InsetDrawable(Color.BLACK.toDrawable(), dp(FADE_INTO_CLOSE_DP), 0, 0, 0)
            // The inset comes as padding with the background, which would widen the group to the
            // left by as much and keep the black at the left of ×
            setPadding(0, 0, 0, 0)
            isClickable = true
            importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
            addView(closeButton, barItem())
            addView(barButton("+") { newSession() }.apply { contentDescription = getString(R.string.new_tab) }, barItem())
            addView(barButton("⋮") { showMenu(it) }, barItem())
        }
        strip.setPadding(0, 0, buttons.childCount * dp(BAR_DP), 0)
        val bar = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(strip, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.MATCH_PARENT))
            addView(buttons, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.MATCH_PARENT, Gravity.END))
            // With gesture navigation, a slide from the screen's left edge over the tabs scrolls
            // them instead of going back; the buttons, which do not scroll, leave the right edge's
            // gesture as it is
            addOnLayoutChangeListener { v, _, _, _, _, _, _, _, _ ->
                v.systemGestureExclusionRects = listOf(Rect(0, 0, v.width - strip.paddingRight, v.height))
            }
        }
        val body = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(screen)
            addView(status)
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
            // The extra keys are drawn in the IME's room while they follow it up (keysFollowIme)
            clipToPadding = false
            addView(bar, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(BAR_DP)))
            addView(body, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(keys)
        }
        renderTabs()
        // Content is drawn behind the system bars (the default for targetSdk 35 from Android 15 on;
        // done here on every version): leave room for the system bars, the display cutout and the
        // IME. From Android 15 on the window also reaches into the cutout, which in landscape is
        // at a side, over the terminal's first or last columns
        WindowCompat.setDecorFitsSystemWindows(window, false)
        ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
            val room = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout() or
                    WindowInsetsCompat.Type.ime(),
            )
            v.setPadding(room.left, room.top, room.right, room.bottom)
            WindowInsetsCompat.CONSUMED
        }
        ViewCompat.setWindowInsetsAnimationCallback(root, keysFollowIme)
        setContentView(root)

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), 0)
        }
    }

    override fun onStart() {
        super.onStart()
        // Settings may have changed it while this activity was stopped
        screen.setFillWide(TerminalService.fillWide(this))
        val intent = Intent(this, TerminalService::class.java)
        startForegroundService(intent)
        bindService(intent, connection, BIND_AUTO_CREATE)
    }

    override fun onStop() {
        service?.removeListener(this)
        unbindService(connection)
        service = null
        super.onStop()
    }

    override fun onDestroy() {
        // Sessions outlive the Activity: do not leave them pointing at this view
        screen.attach(null)
        super.onDestroy()
    }

    // ---- TerminalService.Listener ----

    override fun onProgress(text: String) {
        screen.visibility = View.INVISIBLE
        status.visibility = View.VISIBLE
        status.text = text
    }

    override fun onMachineReady(error: String?) {
        val s = service ?: return
        if (error != null) {
            screen.visibility = View.INVISIBLE
            status.visibility = View.VISIBLE
            status.text = getString(R.string.start_failed, error)
            return
        }
        status.visibility = View.GONE
        screen.visibility = View.VISIBLE
        if (s.sessions.isEmpty()) {
            newSession()
        } else {
            // Tabs restored after a kill: show the one that was being viewed
            show(current?.takeIf { it in s.sessions } ?: s.focus?.takeIf { it in s.sessions } ?: s.sessions.last())
        }
    }

    override fun onSessionsChanged() {
        val s = service ?: return
        if (current != null && current !in s.sessions) {
            // The current tab was closed: show the one after it, now in its place (the one before
            // when it was the last)
            current = null
            s.sessions.getOrNull(currentIndex.coerceAtMost(s.sessions.lastIndex))?.let { show(it) }
        }
        if (s.sessions.isEmpty() && s.machine != 0L) {
            // The last tab was closed: the UI exits. The service stays in the background while it
            // still has guest processes (services started by OpenRC and so on), and exits by itself
            // when there are none (the android/service map)
            finish()
            return
        }
        renderTabs()
    }

    /** Settings changed while this activity is visible too (split screen). */
    override fun onSettingsChanged() {
        screen.setFillWide(TerminalService.fillWide(this))
    }

    /**
     * A distro switch failed (the download failed, or the import failed and fell back to Alpine).
     */
    override fun onNotice() {
        val text = service?.takeNotice() ?: return
        MaterialAlertDialogBuilder(this)
            .setMessage(text)
            .setPositiveButton(android.R.string.ok, null)
            .show()
    }

    override fun onRestarting() {
        current = null
        screen.attach(null)
        screen.visibility = View.INVISIBLE
        status.visibility = View.VISIBLE
        status.setText(R.string.restarting)
    }

    override fun onExitAll() {
        finish()
    }

    // ---- Sessions ----

    private fun newSession() {
        val s = service ?: return
        if (s.machine == 0L) return
        show(s.createSession())
    }

    private fun show(session: VtSession) {
        current = session
        service?.current = session
        // Starts the shell once the view knows the grid (Host.onGrid)
        screen.attach(session)
        screen.requestFocus()
        renderTabs()
    }

    /**
     * Shows the tabs, a number each, the current one marked and scrolled into view. Called whenever
     * tabs, titles or the current tab change.
     *
     * Closed tabs shrink and fade out in place, and the tabs after them slide into the places left,
     * keeping their numbers until they are there ([slideTabs]): closing the current tab shows the
     * next one in its place, under the same number, and it is seen arriving.
     */
    private fun renderTabs() {
        val sessions = service?.sessions.orEmpty().toList()
        if (slideFrom == null && tabs.isLaidOut && rowTabs.isNotEmpty()) {
            slideFrom = rowTabs.associateWith { it.view.left - strip.scrollX }
            indicatorFrom = currentTab?.let { it.left - strip.scrollX }
            tabs.viewTreeObserver.addOnPreDrawListener(slideTabs)
        }
        // Out of the list first, so that they renumber no more. Sessions are added only at the end,
        // so the tabs left keep their order
        val closed = rowTabs.filter { it.session !in sessions }
        rowTabs.removeAll(closed)
        closed.forEach { it.leave() }
        sessions.forEachIndexed { i, session ->
            if (i == rowTabs.size) rowTabs.add(Tab(session, i).also { tabs.addView(it.view, barItem()) })
            rowTabs[i].update(i)
        }

        val shown = current?.takeIf { it in sessions }
        closeButton.visibility = if (shown == null) View.INVISIBLE else View.VISIBLE
        if (shown != null) currentIndex = sessions.indexOf(shown)

        // Indicate the current tab (the row draws the indicator), and bring it into view once the
        // new row is laid out (this runs after the layout pass that changing the tabs requested).
        // Not through returnToCurrentTab: a scroll in that layout (closing tabs can shorten the row)
        // would put it off
        currentTab = rowTabs.getOrNull(sessions.indexOf(shown))?.view
        tabs.invalidate()
        if (currentTab != null) strip.post { revealCurrentTab() }
    }

    /**
     * Scrolls the row to bring the current tab into view, clear of the fade towards the buttons;
     * nothing when it already is, or when there is none.
     */
    private fun revealCurrentTab() {
        val tab = currentTab ?: return
        val fade = strip.horizontalFadingEdgeLength
        val shownWidth = strip.width - strip.paddingRight
        val end = tabs.width - shownWidth
        // Where the fade begins, from the row's visible left edge
        val clearWidth = shownWidth + dp(FADE_INTO_CLOSE_DP) - fade
        when {
            tab.left < strip.scrollX -> strip.smoothScrollTo(tab.left, 0)
            tab.right > strip.scrollX + clearWidth -> {
                val x = tab.right - clearWidth
                // Less than a fade's width of tabs left beyond the padding would fade only inside
                // ×, leaving the last tab barely faded against it: go to the row's end, where
                // nothing fades
                strip.smoothScrollTo(if (end - x < fade) end else x, 0)
            }
        }
    }

    /** Starts, or starts again, the wait before [returnToCurrentTab]. */
    private fun returnToCurrentTabLater() {
        strip.removeCallbacks(returnToCurrentTab)
        strip.postDelayed(returnToCurrentTab, RETURN_TO_CURRENT_TAB_MS)
    }

    /**
     * A tab in the row: its number (from [i]) in a square as tall as the bar, the current one in
     * the primary colour above the row's indicator ([currentTab]), an ended one faded. Tap shows
     * it; a long press shows its title.
     *
     * The indicator goes by the current tab's place in the row ([moveIndicator]), not by where the
     * tab is while it slides, and only the number's colour fades when it changes, not the view
     * (whose opacity the indicator takes), so a changing number leaves the indicator as it is.
     */
    private inner class Tab(val session: VtSession, i: Int) {
        val view = TextView(this@MainActivity).apply {
            text = number(i)
            setTextSize(TypedValue.COMPLEX_UNIT_DIP, TAB_NUMBER_DP)
            gravity = Gravity.CENTER
            foreground = touchFeedback()
            setOnClickListener { show(session) }
            setOnLongClickListener {
                showToast(title(session))
                true
            }
        }

        /** How much of the number's colour shows: 1 but while it changes ([renumber]). */
        private var shown = 1f
        private var renumbering: ValueAnimator? = null

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
            view.contentDescription = getString(R.string.tab_description, i + 1, title(session))
            view.alpha = if (session.isRunning) 1f else ENDED_ALPHA
            if (slideFrom?.containsKey(this) != true) view.text = number(i)
            paint()
        }

        /** Slides to its place in the row, then takes the number of that place. */
        fun slide() {
            view.animate().translationX(0f).setDuration(SLIDE_MS).setInterpolator(STANDARD).withEndAction { renumber() }
        }

        /**
         * Fades its number out and the number of its place in, when they differ. A change while
         * one runs is taken at its end.
         */
        fun renumber() {
            if (renumbering != null || !stale) return
            renumbering = ValueAnimator.ofFloat(-1f, 1f).apply {
                duration = RENUMBER_MS
                addUpdateListener {
                    val f = it.animatedValue as Float
                    // The new number from halfway on, from its place then
                    if (f >= 0f && stale) view.text = number(rowTabs.indexOf(this@Tab))
                    shown = abs(f)
                    paint()
                }
                doOnEnd {
                    renumbering = null
                    renumber()
                }
                start()
            }
        }

        /** Out of the row, into its overlay, where it shrinks and fades out in its place. */
        fun leave() {
            renumbering?.cancel()
            // A slide stops where it is
            view.animate().cancel()
            tabs.overlay.add(view)
            view.animate().alpha(0f).scaleX(LEAVE_SCALE).scaleY(LEAVE_SCALE)
                .setDuration(LEAVE_MS).setInterpolator(ACCELERATE)
                .withEndAction { tabs.overlay.remove(view) }
        }

        private fun paint() {
            val color = if (session == current) currentTabColor else mutedColor
            view.setTextColor(ColorUtils.setAlphaComponent(color, (Color.alpha(color) * shown).roundToInt()))
        }
    }

    private fun number(i: Int) = "%d".format(i + 1)

    /**
     * Moves the indicator to the current tab's new place on screen ([slideTabs]): sliding there
     * from where it was drawn when tabs [closed], at once when another tab was shown. Closing the
     * current tab of a row that does not scroll leaves its place where it was, so the indicator
     * stays while the next tab slides in above it; a row scrolled to its end scrolls back as it
     * shortens, and the indicator slides on to the next tab.
     */
    private fun moveIndicator(closed: Boolean) {
        val tab = currentTab
        val from = indicatorFrom
        indicatorFrom = null
        val shift = if (tab == null || from == null) null else from - (tab.left - strip.scrollX)
        when {
            shift == 0 -> {} // the same place: a slide still running goes on
            shift != null && closed -> {
                indicatorSlide?.cancel()
                indicatorSlide = ValueAnimator.ofFloat(indicatorShift + shift, 0f).apply {
                    duration = SLIDE_MS
                    interpolator = STANDARD
                    addUpdateListener {
                        indicatorShift = it.animatedValue as Float
                        tabs.invalidate()
                    }
                    start()
                }
            }
            else -> {
                indicatorSlide?.cancel()
                indicatorShift = 0f
            }
        }
    }

    /** The program's title (`sh` when it set none), marked once the shell has ended. */
    private fun title(session: VtSession): String {
        val text = session.title?.takeIf { it.isNotBlank() } ?: "sh"
        return if (session.isRunning) text else getString(R.string.tab_ended, text)
    }

    /**
     * A tab's title or a notice from the extra keys, in place of the last one, so that quick long
     * presses do not queue toasts.
     */
    private fun showToast(text: String) {
        toast?.cancel()
        toast = Toast.makeText(this, text, Toast.LENGTH_SHORT).apply { show() }
    }

    /** A button in the bar: a square as tall as the bar ([barItem]) with its label centred. */
    private fun barButton(label: String, onClick: (View) -> Unit) = TextView(this).apply {
        text = label
        setTextColor(mutedColor)
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, BAR_LABEL_DP)
        gravity = Gravity.CENTER
        background = touchFeedback()
        setOnClickListener(onClick)
    }

    /** A tab or a button: a square as tall as the bar. */
    private fun barItem() = LinearLayout.LayoutParams(dp(BAR_DP), ViewGroup.LayoutParams.MATCH_PARENT)

    /** The theme's ripple for something that can be tapped. */
    private fun touchFeedback(): Drawable? {
        val a = obtainStyledAttributes(intArrayOf(android.R.attr.selectableItemBackground))
        return a.getDrawable(0).also { a.recycle() }
    }

    /** `⋮`: closing the other tabs (none other: disabled) or all of them, and Settings. */
    private fun showMenu(anchor: View) {
        val sessions = service?.sessions.orEmpty().toList()
        val others = sessions.filter { it != current }
        PopupMenu(this, anchor).apply {
            menu.add(R.string.close_other_tabs).apply { isEnabled = others.isNotEmpty() }.setOnMenuItemClickListener {
                confirmClose(others)
                true
            }
            menu.add(R.string.close_all_tabs).apply { isEnabled = sessions.isNotEmpty() }.setOnMenuItemClickListener {
                confirmClose(sessions)
                true
            }
            menu.add(R.string.menu_settings).setOnMenuItemClickListener {
                startActivity(Intent(this@MainActivity, SettingsActivity::class.java))
                true
            }
            show()
        }
    }

    /**
     * `×` and the menu: closes [targets] at once when all their shells have ended, else after
     * asking, as closing hangs a shell up and with it, usually, the programs it runs.
     */
    private fun confirmClose(targets: List<VtSession>) {
        if (targets.none { it.isRunning }) {
            close(targets)
            return
        }
        val one = targets.size == 1
        MaterialAlertDialogBuilder(this)
            .setTitle(
                if (one) getString(R.string.close_tab_title)
                else resources.getQuantityString(R.plurals.close_tabs_title, targets.size, targets.size)
            )
            .setMessage(if (one) R.string.close_tab_message else R.string.close_tabs_message)
            .setPositiveButton(R.string.close) { _, _ ->
                // Some may have been closed meanwhile (their kernel restarting)
                close(targets.filter { service?.sessions?.contains(it) == true })
            }
            .setNegativeButton(android.R.string.cancel, null)
            // The terminal takes the keyboard again
            .setOnDismissListener { screen.requestFocus() }
            .show()
    }

    /**
     * Closes [targets], the current tab last: closed earlier, it would have another tab shown (and
     * its shell started) only to close that one too.
     */
    private fun close(targets: List<VtSession>) {
        targets.sortedBy { it == current }.forEach { close(it) }
    }

    private fun close(session: VtSession) {
        service?.removeSession(session)
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (event.action == KeyEvent.ACTION_DOWN && shortcut(event)) {
            shortcutKey = event.keyCode
            return true
        }
        // Its release too, or the terminal would send a release (Kitty protocol) for a key it never
        // saw pressed
        if (event.action == KeyEvent.ACTION_UP && event.keyCode == shortcutKey) {
            shortcutKey = 0
            return true
        }
        return super.dispatchKeyEvent(event)
    }

    /**
     * The tab shortcuts of a hardware keyboard, as in desktop terminals: Ctrl+Shift+T opens a tab,
     * Ctrl+Shift+W closes the current one, Ctrl+Tab and Ctrl+Shift+Tab show the next and the
     * previous one (going round). They are taken before the terminal sees them; false for any other
     * key.
     */
    private fun shortcut(event: KeyEvent): Boolean {
        if (!event.isCtrlPressed || event.isAltPressed || event.isMetaPressed) return false
        // Holding Ctrl+Tab keeps switching; holding the others acts once
        val first = event.repeatCount == 0
        when {
            event.keyCode == KeyEvent.KEYCODE_TAB -> switchBy(if (event.isShiftPressed) -1 else 1)
            !event.isShiftPressed -> return false
            event.keyCode == KeyEvent.KEYCODE_T -> if (first) newSession()
            event.keyCode == KeyEvent.KEYCODE_W -> if (first) current?.let { close(it) }
            else -> return false
        }
        return true
    }

    /** Shows the tab [delta] places after the current one, going round from the last to the first. */
    private fun switchBy(delta: Int) {
        val sessions = service?.sessions ?: return
        val i = sessions.indexOf(current)
        if (i < 0 || sessions.size < 2) return
        show(sessions[Math.floorMod(i + delta, sessions.size)])
    }

    /** Lists the tab shortcuts in the system's keyboard shortcuts screen (Meta+/). */
    override fun onProvideKeyboardShortcuts(data: MutableList<KeyboardShortcutGroup>, menu: Menu?, deviceId: Int) {
        val ctrl = KeyEvent.META_CTRL_ON
        val ctrlShift = KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON
        data.add(
            KeyboardShortcutGroup(
                getString(R.string.shortcuts_tabs),
                listOf(
                    KeyboardShortcutInfo(getString(R.string.new_tab), KeyEvent.KEYCODE_T, ctrlShift),
                    KeyboardShortcutInfo(getString(R.string.close_tab), KeyEvent.KEYCODE_W, ctrlShift),
                    KeyboardShortcutInfo(getString(R.string.next_tab), KeyEvent.KEYCODE_TAB, ctrl),
                    KeyboardShortcutInfo(getString(R.string.previous_tab), KeyEvent.KEYCODE_TAB, ctrlShift),
                ),
            ),
        )
    }

    /** The [VtView]'s side: starting the shell, input, zoom and the extra keys' modifiers. */
    private inner class Host : VtView.Host {
        override fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
            current?.start(cols, rows, cellWidth, cellHeight)
        }

        override fun onInput(data: ByteArray) {
            val session = current ?: return
            if (session.isRunning) {
                session.write(data)
            } else if (data.any { it == '\r'.code.toByte() || it == '\n'.code.toByte() }) {
                // When the session has ended, Enter (from a key or the IME) closes it
                close(session)
            }
        }

        override fun onZoom(fontPx: Int) {
            fontSize = fontPx
            getPreferences(MODE_PRIVATE).edit { putInt(PREF_FONT_SIZE, fontSize) }
        }

        override val ctrl: Boolean get() = keys.ctrl
        override val alt: Boolean get() = keys.alt
        override val shift: Boolean get() = keys.shift

        override fun modifiersUsed() = keys.modifiersUsed()
    }

    /** The extra keys go to the view, which reads the one-shot modifiers through [Host]. */
    private inner class Keys : ExtraKeysView.Target {
        override fun key(keyCode: Int) = screen.press(keyCode)

        override fun text(text: String) = screen.type(text)

        override fun notice(text: String) = showToast(text)
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun themeColor(attr: Int) = MaterialColors.getColor(this, attr, Color.WHITE)

    companion object {
        private const val PREF_FONT_SIZE = "font_size"

        /** The tab bar's height, and the side of its tabs and buttons. */
        private const val BAR_DP = 32

        /**
         * The size of the tab bar's buttons' labels. In dp, not sp: the bar's height is fixed, so
         * its labels ignore the system font scale.
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
         * How long the tab row rests, with no finger on it, before it brings a current tab it was
         * scrolled away from back into view; longer than a tab's title stays up after a long press.
         */
        private const val RETURN_TO_CURRENT_TAB_MS = 3000L

        /** How long tabs take to slide into the places of closed ones (Material's short 4). */
        private const val SLIDE_MS = 200L

        /** How long a closed tab takes to shrink to [LEAVE_SCALE] and fade out. */
        private const val LEAVE_MS = 150L
        private const val LEAVE_SCALE = 0.6f

        /** How long a tab takes to fade its number out and the new one in, half each. */
        private const val RENUMBER_MS = 200L

        /** Material's standard easing, for the slide. */
        private val STANDARD = PathInterpolator(0.2f, 0f, 0f, 1f)

        /** Material's standard accelerate easing, for a closed tab on its way out. */
        private val ACCELERATE = PathInterpolator(0.3f, 0f, 1f, 1f)
    }
}
