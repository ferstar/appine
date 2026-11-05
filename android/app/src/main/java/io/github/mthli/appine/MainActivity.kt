package io.github.mthli.appine

import android.Manifest
import android.app.Activity
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
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
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.appcompat.widget.PopupMenu
import androidx.core.content.edit
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import com.google.android.material.color.DynamicColors
import com.google.android.material.color.MaterialColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder

/**
 * The terminal screen: the tab bar on top, the terminal ([VtView]) in the middle, and below it the
 * keys the soft keyboard lacks (Esc, Ctrl, arrows and so on).
 * Sessions belong to [TerminalService]; this only handles display and input: the view shows the
 * current tab's [VtSession].
 *
 * The tab bar is one row in any window: a scrolling row of tabs, each a square with its number
 * (a long press shows its title), then `×` (closes the current tab), `+` and `⋮`.
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
    private var fontSize = 0

    /** The last tab title shown by a long press, cancelled by the next. */
    private var toast: Toast? = null

    /** The key of the shortcut last taken ([dispatchKeyEvent]), whose release is taken too. */
    private var shortcutKey = 0

    // The theme's colours, from the wallpaper on Android 12 and later, so read after onCreate has
    // applied them. The tab bar, the extra keys and what lies under the system bars are black, as
    // the terminal is

    /** The bar's buttons, other tabs' numbers and the status text. */
    private val mutedColor by lazy { themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant) }

    /** The current tab's number and its box. */
    private val currentTabColor by lazy { themeColor(androidx.appcompat.R.attr.colorPrimary) }

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
        tabs = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        strip = HorizontalScrollView(this).apply {
            isHorizontalScrollBarEnabled = false
            addView(tabs, FrameLayout.LayoutParams(ViewGroup.LayoutParams.WRAP_CONTENT, ViewGroup.LayoutParams.MATCH_PARENT))
        }
        // The multiplication sign, which Roboto draws like its +; U+2715 (✕) is not in Roboto, and
        // the fallback font's is larger and thinner than + and ⋮
        closeButton = barButton("×") { current?.let { confirmClose(listOf(it)) } }.apply {
            contentDescription = getString(R.string.close_tab)
        }
        keys = ExtraKeysView(this, Keys())

        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(Color.BLACK)
            addView(strip, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.MATCH_PARENT, 1f))
            addView(closeButton, barItem())
            addView(barButton("+") { newSession() }.apply { contentDescription = getString(R.string.new_tab) }, barItem())
            addView(barButton("⋮") { showMenu(it) }, barItem())
        }
        val body = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(screen)
            addView(status)
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
            addView(bar, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, dp(BAR_DP)))
            addView(body, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(keys)
        }
        renderTabs()
        // Content is drawn behind the system bars (the default for targetSdk 35 from Android 15 on;
        // done here on every version): leave room for the system bars and the IME
        WindowCompat.setDecorFitsSystemWindows(window, false)
        ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.ime())
            v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            WindowInsetsCompat.CONSUMED
        }
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
     * Shows the tabs, a number each, the current one boxed and scrolled into view. Called whenever
     * tabs, titles or the current tab change.
     */
    private fun renderTabs() {
        val sessions = service?.sessions.orEmpty().toList()
        tabs.removeAllViews()
        sessions.forEachIndexed { i, session -> tabs.addView(tabButton(i, session), barItem()) }

        val shown = current?.takeIf { it in sessions }
        closeButton.visibility = if (shown == null) View.INVISIBLE else View.VISIBLE
        if (shown != null) currentIndex = sessions.indexOf(shown)

        // Bring the current tab into view once the new row is laid out (this runs after the
        // layout pass that adding the tabs requested)
        val tab = tabs.getChildAt(sessions.indexOf(shown)) ?: return
        strip.post {
            when {
                tab.left < strip.scrollX -> strip.smoothScrollTo(tab.left, 0)
                tab.right > strip.scrollX + strip.width -> strip.smoothScrollTo(tab.right - strip.width, 0)
            }
        }
    }

    /**
     * A tab in the row: its number in a square as tall as the bar, the current one in the primary
     * colour and an outlined box, an ended one faded. Tap shows it; a long press shows its title.
     */
    private fun tabButton(i: Int, session: VtSession) = TextView(this).apply {
        text = "%d".format(i + 1)
        contentDescription = getString(R.string.tab_description, i + 1, title(session))
        setTextSize(TypedValue.COMPLEX_UNIT_DIP, TAB_NUMBER_DP)
        typeface = Typeface.DEFAULT_BOLD
        gravity = Gravity.CENTER
        if (session == current) {
            setTextColor(currentTabColor)
            background = InsetDrawable(
                GradientDrawable().apply {
                    setStroke(dp(2), currentTabColor)
                    cornerRadius = dp(4).toFloat()
                },
                dp(TAB_BOX_INSET_DP),
            )
        } else {
            setTextColor(mutedColor)
        }
        if (!session.isRunning) alpha = ENDED_ALPHA
        foreground = touchFeedback()
        setOnClickListener { show(session) }
        setOnLongClickListener {
            toast?.cancel()
            toast = Toast.makeText(this@MainActivity, title(session), Toast.LENGTH_SHORT).apply { show() }
            true
        }
    }

    /** The program's title (`sh` when it set none), marked once the shell has ended. */
    private fun title(session: VtSession): String {
        val text = session.title?.takeIf { it.isNotBlank() } ?: "sh"
        return if (session.isRunning) text else getString(R.string.tab_ended, text)
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

        /** How far the current tab's box lies inside its square (dp), leaving a 20 dp box. */
        private const val TAB_BOX_INSET_DP = 6

        /** The opacity of an ended tab's number (Material's disabled content). */
        private const val ENDED_ALPHA = 0.38f
    }
}
