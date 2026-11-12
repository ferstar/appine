package io.github.mthli.appine

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.ClipboardManager
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.graphics.Color
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.view.Gravity
import android.view.KeyEvent
import android.view.KeyboardShortcutGroup
import android.view.KeyboardShortcutInfo
import android.view.Menu
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import android.window.OnBackInvokedCallback
import android.window.OnBackInvokedDispatcher
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.widget.PopupMenu
import androidx.core.content.edit
import androidx.core.net.toUri
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsAnimationCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.isVisible
import com.google.android.material.color.DynamicColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import io.github.mthli.appine.MainActivity.Companion.PREF_FONT_SIZE
import kotlin.concurrent.thread

/**
 * The terminal screen: the tab bar ([TabBarView]) on top, the terminal ([VtView]) in the middle,
 * and below it the keys the soft keyboard lacks ([ExtraKeysView]: Esc, Ctrl, arrows and so on).
 * Sessions belong to [TerminalService]; this only handles display and input: the bar shows every
 * tab, the view the current tab's [VtSession]. While the current tab is searched, the search bar
 * ([SearchBarView]) takes the tab bar's place.
 *
 * Its dialogs and menus are Material 3 (`Theme.Appine.Terminal`, always dark); the tab bar and the
 * extra keys are black like the terminal, with labels in the theme's colours. The activity itself
 * stays a framework one, and so do the views it builds.
 */
class MainActivity : Activity(), TerminalService.Listener {

    private lateinit var screen: VtView

    /** The current tab's progress (OSC 9;4), along the top of the terminal. */
    private lateinit var progressLine: ProgressLine
    private lateinit var tabBar: TabBarView

    /** In the tab bar's place while the current tab is searched ([openSearch]). */
    private lateinit var searchBar: SearchBarView
    private lateinit var status: TextView
    private lateinit var keys: ExtraKeysView

    private var service: TerminalService? = null
    private var current: VtSession? = null

    /** [current]'s position when the tabs were last shown, where its successor is taken from. */
    private var currentIndex = 0

    /**
     * The tab a notification asked to show ([EXTRA_TAB], a [VtSession.id]), -1 for none; shown
     * once the service is bound and the kernel ready ([showRequestedTab]).
     */
    private var requestedTab = -1

    /** The last toast ([showToast]), cancelled by the next. */
    private var toast: Toast? = null

    /**
     * The key of the shortcut or volume key last taken ([dispatchKeyEvent]), whose release is
     * taken too.
     */
    private var shortcutKey = 0

    /**
     * Back's callback while the search bar is open, from Android 16 on (an OnBackInvokedCallback,
     * kept untyped for older versions); null otherwise. See [onBackPressed].
     */
    private var closeSearchOnBack: Any? = null

    /**
     * The question whether a program may read the clipboard ([onClipboardRead]) while it is open,
     * which answers the reads that come meanwhile too.
     */
    private var clipboardQuestion: AlertDialog? = null

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
        // A notification's tab only when it starts this activity: one recreated (a configuration
        // change, or the system bringing it back after killing the app) or opened from Recents
        // still has the intent that first started it, whose tab the user has left since
        if (savedInstanceState == null && intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY == 0) {
            requestedTab = intent.getIntExtra(EXTRA_TAB, -1)
        }
        screen = VtView(this, Host()).apply {
            setFontSize(savedFontSize())
            setBackgroundColor(Color.BLACK)
        }
        status = TextView(this).apply {
            // In the muted colour of the bars' labels, read once onCreate has applied the
            // wallpaper's colours (Android 12 and later)
            setTextColor(themeColor(com.google.android.material.R.attr.colorOnSurfaceVariant))
            gravity = Gravity.CENTER
            setText(R.string.starting)
        }
        progressLine = ProgressLine(this)
        tabBar = TabBarView(this, Bar())
        searchBar = SearchBarView(this, Find()).apply { isVisible = false }
        keys = ExtraKeysView(this, Keys())
        // One bar's place: the search bar, as tall as the tab bar, stands in for it
        val bars = FrameLayout(this).apply {
            addView(tabBar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
            addView(searchBar, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
        }
        val body = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(screen)
            addView(status)
            addView(progressLine, FrameLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, line(), Gravity.TOP))
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
            // The extra keys are drawn in the IME's room while they follow it up (keysFollowIme)
            clipToPadding = false
            addView(bars, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, ViewGroup.LayoutParams.WRAP_CONTENT))
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

    /**
     * The terminal's font size in pixels. It is kept in dp, so that after a display size change
     * (which recreates this activity) the text keeps its size relative to the rest; a size kept in
     * pixels by earlier versions is taken at the current density and kept in dp from then on.
     */
    private fun savedFontSize(): Float {
        val prefs = getPreferences(MODE_PRIVATE)
        val density = resources.displayMetrics.density
        if (prefs.contains(PREF_FONT_SIZE_PX)) {
            val dp = prefs.getInt(PREF_FONT_SIZE_PX, 0) / density
            prefs.edit {
                putFloat(PREF_FONT_SIZE, dp)
                remove(PREF_FONT_SIZE_PX)
            }
        }
        return prefs.getFloat(PREF_FONT_SIZE, DEFAULT_FONT_DP) * density
    }

    /** A notification of a tab was tapped while this activity exists (it is `singleTask`). */
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        requestedTab = intent.getIntExtra(EXTRA_TAB, -1)
        showRequestedTab()
    }

    override fun onStart() {
        super.onStart()
        // Settings may have changed it while this activity was stopped
        screen.setFillWide(Settings.fillWide(this))
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
        if (showRequestedTab()) return
        if (s.sessions.isEmpty()) {
            newSession()
        } else {
            // The tab this activity shows; for a new one (recreated, or opened again after Back
            // finished the last one), the tab the last one showed; after tabs were restored from
            // a kill, the one that was being viewed then; else the last
            show(
                current?.takeIf { it in s.sessions } ?: s.current?.takeIf { it in s.sessions }
                    ?: s.focus?.takeIf { it in s.sessions } ?: s.sessions.last(),
            )
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

    /** The program in the tab shown rang the bell (the service sends only those). */
    override fun onBell() = screen.bell()

    /**
     * A program in the tab shown asks to read the clipboard (OSC 52): the user is asked, with what
     * it holds shown, as Ghostty's clipboard-read = ask does; nothing to read (an empty clipboard,
     * or no text) is answered at once. Denying, or leaving the question unanswered, gives the
     * program an empty clipboard.
     */
    override fun onClipboardRead() {
        val session = current ?: return
        if (clipboardQuestion != null) return
        val clip = getSystemService(ClipboardManager::class.java).primaryClip?.takeIf { it.itemCount > 0 }
        val text = clip?.getItemAt(0)?.coerceToText(this)?.toString().orEmpty()
        if (text.isEmpty()) {
            session.answerClipboardRead("")
            return
        }
        val preview = if (text.length > CLIPBOARD_PREVIEW) text.take(CLIPBOARD_PREVIEW) + "…" else text
        var allowed = false
        clipboardQuestion = MaterialAlertDialogBuilder(this)
            .setTitle(R.string.clipboard_read_title)
            .setMessage(getString(R.string.clipboard_read, preview))
            .setPositiveButton(R.string.allow) { _, _ -> allowed = true }
            .setNegativeButton(R.string.deny, null)
            .setOnDismissListener {
                clipboardQuestion = null
                session.answerClipboardRead(if (allowed) text else null)
                // The terminal takes the keyboard again
                screen.requestFocus()
            }
            .show()
    }

    /** A tab's progress or program status changed: its marks in the bar, the progress line. */
    override fun onTabStatus() {
        tabBar.refreshStatus()
        progressLine.show(current?.progress, jump = false)
    }

    /** Settings changed while this activity is visible too (split screen). */
    override fun onSettingsChanged() {
        screen.setFillWide(Settings.fillWide(this))
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
        closeSearch()
        current = null
        progressLine.show(null, jump = true)
        screen.attach(null)
        screen.visibility = View.INVISIBLE
        status.visibility = View.VISIBLE
        status.setText(R.string.restarting)
    }

    override fun onExitAll() {
        finish()
    }

    // ---- Sessions ----

    /**
     * Opens a tab in the current tab's directory (that of the program in its foreground, the shell
     * at its prompt), as Ghostty's new tabs inherit the working directory; with no current tab, or
     * one whose shell has ended, in the home directory.
     */
    private fun newSession() {
        val s = service ?: return
        if (s.machine == 0L) return
        show(s.createSession(cwd = current?.cwd()))
    }

    /**
     * Shows the tab a notification named ([requestedTab]) once the kernel is ready; false when
     * there is none, or it has closed.
     */
    private fun showRequestedTab(): Boolean {
        val s = service ?: return false
        if (requestedTab < 0 || s.machine == 0L) return false
        val session = s.sessions.firstOrNull { it.id == requestedTab }
        requestedTab = -1
        session?.let(::show)
        return session != null
    }

    private fun show(session: VtSession) {
        // A search belongs to the tab it was opened in
        closeSearch()
        current = session
        progressLine.show(session.progress, jump = true)
        service?.current = session
        // Starts the shell once the view knows the grid (Host.onGrid)
        screen.attach(session)
        screen.requestFocus()
        renderTabs()
    }

    /**
     * Shows the tabs in the bar, the current one marked ([TabBarView.render]). Called whenever
     * tabs, titles or the current tab change.
     */
    private fun renderTabs() {
        val sessions = service?.sessions.orEmpty().toList()
        val shown = current?.takeIf { it in sessions }
        if (shown != null) currentIndex = sessions.indexOf(shown)
        tabBar.render(sessions, shown)
    }

    // ---- The menu and closing tabs ----

    /**
     * `⋮`: finding text in the current tab (none: disabled), clearing its screen (also disabled on
     * the alternate screen, where Ghostty's clear_screen does nothing) or resetting its terminal
     * (disabled before its shell starts), closing the other tabs (none other: disabled) or all of
     * them, and Settings.
     */
    private fun showMenu(anchor: View) {
        val sessions = service?.sessions.orEmpty().toList()
        val others = sessions.filter { it != current }
        PopupMenu(this, anchor, Gravity.END, 0, R.style.Widget_Appine_MorePopup).apply {
            menu.add(R.string.find).apply { isEnabled = current != null }.setOnMenuItemClickListener {
                openSearch()
                true
            }
            val terminal = current?.takeIf { it.tab != 0L }
            menu.add(R.string.clear_screen).apply { isEnabled = terminal != null && !screen.altScreen }
                .setOnMenuItemClickListener {
                    terminal?.clear()
                    true
                }
            menu.add(R.string.reset_terminal).apply { isEnabled = terminal != null }.setOnMenuItemClickListener {
                terminal?.reset()
                true
            }
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
     * `×` and the menu: closes [targets] at once when none is [busy][VtSession.busy] (their shells
     * have ended or wait at their prompts), else after asking, as closing hangs a shell up and with
     * it, usually, the programs it runs. A shell without prompt marks always counts as busy.
     */
    private fun confirmClose(targets: List<VtSession>) {
        if (targets.none { it.busy }) {
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

    // ---- Finding text ----

    /**
     * Puts the search bar in the tab bar's place, with the keyboard on it, to search the current
     * tab (the same height, so the terminal keeps its rows); Back closes it.
     */
    private fun openSearch() {
        if (current == null) return
        tabBar.isVisible = false
        searchBar.open()
        if (closeSearchOnBack == null && Build.VERSION.SDK_INT >= Build.VERSION_CODES.BAKLAVA) {
            val callback = OnBackInvokedCallback { closeSearch() }
            onBackInvokedDispatcher.registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_DEFAULT, callback)
            closeSearchOnBack = callback
        }
    }

    /** Ends the search and brings the tab bar back; the terminal takes the keyboard again. */
    private fun closeSearch() {
        if (!searchBar.isVisible) return
        searchBar.isVisible = false
        tabBar.isVisible = true
        screen.endSearch()
        screen.requestFocus()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.BAKLAVA) {
            (closeSearchOnBack as? OnBackInvokedCallback)?.let { onBackInvokedDispatcher.unregisterOnBackInvokedCallback(it) }
        }
        closeSearchOnBack = null
    }

    /**
     * Back closes the search bar first. Up to Android 15 the system calls this, as the app does
     * not opt in to predictive back in its manifest (an OnBackInvokedCallback would be ignored
     * there); from Android 16 on, apps targeting it get predictive back anyway, never call this,
     * and [closeSearchOnBack] is called instead.
     */
    @SuppressLint("GestureBackNavigation") // Android 16 and later: closeSearchOnBack
    @Suppress("OVERRIDE_DEPRECATION", "DEPRECATION")
    override fun onBackPressed() {
        if (searchBar.isVisible) {
            closeSearch()
            return
        }
        super.onBackPressed()
    }

    // ---- Keyboard shortcuts ----

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (event.action == KeyEvent.ACTION_DOWN && (shortcut(event) || volumeKey(event))) {
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
     * The shortcuts of a hardware keyboard, as in desktop terminals: Ctrl+Shift+T opens a tab,
     * Ctrl+Shift+W closes the current one, Ctrl+Tab and Ctrl+Shift+Tab show the next and the
     * previous one (going round), Ctrl+Shift+F finds text in it and Ctrl+Shift+↑ and ↓ scroll to
     * the previous and the next prompt (as in Ghostty on Linux; on the alternate screen, which has
     * no prompts, they go to the program). They are taken before the terminal sees them; false for
     * any other key.
     */
    private fun shortcut(event: KeyEvent): Boolean {
        if (!event.isCtrlPressed || event.isAltPressed || event.isMetaPressed) return false
        // Holding Ctrl+Tab or a jump keeps going; holding the others acts once
        val first = event.repeatCount == 0
        when {
            event.keyCode == KeyEvent.KEYCODE_TAB -> switchBy(if (event.isShiftPressed) -1 else 1)
            !event.isShiftPressed -> return false
            event.keyCode == KeyEvent.KEYCODE_T -> if (first) newSession()
            event.keyCode == KeyEvent.KEYCODE_W -> if (first) current?.let { close(it) }
            event.keyCode == KeyEvent.KEYCODE_F -> if (first) openSearch()
            event.keyCode == KeyEvent.KEYCODE_DPAD_UP -> return screen.jumpToPrompt(-1)
            event.keyCode == KeyEvent.KEYCODE_DPAD_DOWN -> return screen.jumpToPrompt(1)
            else -> return false
        }
        return true
    }

    /**
     * Shows the tab [delta] places after the current one, going round from the last to the first;
     * false when there is no other tab to show.
     */
    private fun switchBy(delta: Int): Boolean {
        val sessions = service?.sessions ?: return false
        val i = sessions.indexOf(current)
        if (i < 0 || sessions.size < 2) return false
        show(sessions[Math.floorMod(i + delta, sessions.size)])
        return true
    }

    /**
     * The volume keys switch tabs while Settings has them do so ([Settings.volumeKeysSwitchTabs], on
     * by default): up shows the previous tab and down the next, going round, once per press (a held
     * key's repeats are swallowed). With no other tab to show, or with the setting off, they are
     * left to the system, which changes the volume. False for any other key.
     */
    private fun volumeKey(event: KeyEvent): Boolean {
        val delta = when (event.keyCode) {
            KeyEvent.KEYCODE_VOLUME_UP -> -1
            KeyEvent.KEYCODE_VOLUME_DOWN -> 1
            else -> return false
        }
        // Repeats follow the press: swallowed after a switch, else the volume keeps changing
        if (event.repeatCount > 0) return shortcutKey == event.keyCode
        return Settings.volumeKeysSwitchTabs(this) && switchBy(delta)
    }

    /** Lists the shortcuts in the system's keyboard shortcuts screen (Meta+/). */
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
        data.add(
            KeyboardShortcutGroup(
                getString(R.string.shortcuts_terminal),
                listOf(
                    KeyboardShortcutInfo(getString(R.string.find), KeyEvent.KEYCODE_F, ctrlShift),
                    KeyboardShortcutInfo(getString(R.string.previous_prompt), KeyEvent.KEYCODE_DPAD_UP, ctrlShift),
                    KeyboardShortcutInfo(getString(R.string.next_prompt), KeyEvent.KEYCODE_DPAD_DOWN, ctrlShift),
                ),
            ),
        )
    }

    // ---- The views' targets ----

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
            getPreferences(MODE_PRIVATE).edit { putFloat(PREF_FONT_SIZE, fontPx / resources.displayMetrics.density) }
        }

        override val ctrl: Boolean get() = keys.ctrl
        override val alt: Boolean get() = keys.alt
        override val shift: Boolean get() = keys.shift

        override fun modifiersUsed() = keys.modifiersUsed()

        override fun onSearchCount(total: Int, selected: Int) = searchBar.setCount(total, selected)

        /**
         * Opens it in the app the system picks: a web page or mail as a browsable intent, which
         * reaches only apps that take links from the web, a guest file through the Files app's
         * provider ([openFile]).
         */
        override fun onLink(uri: String) {
            val link = uri.toUri()
            if (link.scheme.equals("file", ignoreCase = true)) {
                openFile(link)
            } else {
                startViewer(Intent(Intent.ACTION_VIEW, link).addCategory(Intent.CATEGORY_BROWSABLE))
            }
        }

        /**
         * A guest file from a program's `file:` link, opened as the Files app opens one, read-only;
         * looked up off the main thread, since that reads the store ([Documents.fileLink]).
         */
        private fun openFile(link: Uri) {
            thread(name = "appine-link") {
                val found = Documents.fileLink(this@MainActivity, link)
                runOnUiThread {
                    if (isDestroyed) return@runOnUiThread
                    when (found) {
                        is Documents.FileLink.Found -> startViewer(
                            Intent(Intent.ACTION_VIEW).setDataAndType(found.uri, found.mime)
                                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION),
                        )
                        is Documents.FileLink.Remote -> showToast(getString(R.string.link_remote_file, found.host))
                        Documents.FileLink.Hidden -> showToast(getString(R.string.link_hidden_file), long = true)
                        Documents.FileLink.Missing -> showToast(getString(R.string.link_missing_file))
                    }
                }
            }
        }

        private fun startViewer(intent: Intent) {
            try {
                startActivity(intent)
            } catch (e: ActivityNotFoundException) {
                showToast(getString(R.string.link_no_app))
            }
        }
    }

    /** Taps on the tab bar: a tab, its title, `×`, `+` and `⋮`. */
    private inner class Bar : TabBarView.Target {
        override fun show(session: VtSession) = this@MainActivity.show(session)

        override fun closeCurrent() {
            current?.let { confirmClose(listOf(it)) }
        }

        override fun newTab() = newSession()

        override fun menu(anchor: View) = showMenu(anchor)

        override fun notice(text: String) = showToast(text)
    }

    /** The search bar's text and buttons go to the view, which searches the current tab. */
    private inner class Find : SearchBarView.Target {
        override fun search(needle: String) = screen.search(needle)

        override fun step(older: Boolean) = screen.searchStep(older)

        override fun close() = closeSearch()
    }

    /** The extra keys go to the view, which reads the one-shot modifiers through [Host]. */
    private inner class Keys : ExtraKeysView.Target {
        override fun key(keyCode: Int) = screen.press(keyCode)

        override fun text(text: String) = screen.type(text)

        override fun notice(text: String) = showToast(text)
    }

    /**
     * A tab's title, a notice from the extra keys or why a link did not open, in place of the last
     * one, so that quick long presses do not queue toasts; [long] for text that takes a while to read.
     */
    private fun showToast(text: String, long: Boolean = false) {
        toast?.cancel()
        toast = Toast.makeText(this, text, if (long) Toast.LENGTH_LONG else Toast.LENGTH_SHORT).apply { show() }
    }

    companion object {
        /** A [VtSession.id]: the tab to show (a tab's notification, from the service). */
        const val EXTRA_TAB = "tab"

        /** The terminal's font size in dp (a float). */
        private const val PREF_FONT_SIZE = "font_size_dp"

        /** The font size in pixels, as versions before [PREF_FONT_SIZE] kept it (an int). */
        private const val PREF_FONT_SIZE_PX = "font_size"

        private const val DEFAULT_FONT_DP = 12f

        /**
         * Characters of the clipboard shown when asking whether a program may read it, as many as
         * of a paste (VtView).
         */
        private const val CLIPBOARD_PREVIEW = 300
    }
}
