package io.github.mthli.appine

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.graphics.Color
import android.graphics.Typeface
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.util.Log
import android.util.TypedValue
import android.view.Gravity
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.View
import android.view.ViewGroup
import android.view.inputmethod.InputMethodManager
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.PopupMenu
import android.widget.TextView
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import com.termux.terminal.KeyHandler
import com.termux.terminal.TerminalSession
import com.termux.view.TerminalView
import com.termux.view.TerminalViewClient

/**
 * The terminal screen: the tab bar on top, the terminal in the middle, and below it the keys the
 * soft keyboard lacks (Esc, Ctrl, arrows and so on).
 * Sessions belong to [TerminalService]; this only handles display and input. The terminal is a
 * [VtView] when the native library has libghostty-vt ([VtNative.available], as the service's
 * sessions then are [VtSession]s), else Termux's [TerminalView].
 */
class MainActivity : Activity(), TerminalService.Listener {

    /** Exactly one of the two views exists, the one the service's sessions need. */
    private var terminal: TerminalView? = null
    private var vtView: VtView? = null
    private lateinit var screen: View
    private lateinit var tabs: LinearLayout
    private lateinit var status: TextView
    private lateinit var keys: ExtraKeysView

    private var service: TerminalService? = null
    private var current: Session? = null
    private var fontSize = 0

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
        super.onCreate(savedInstanceState)
        fontSize = getPreferences(MODE_PRIVATE).getInt(PREF_FONT_SIZE, dp(12))
        screen = if (VtNative.available) {
            VtView(this, VtHost()).apply {
                setFontSize(fontSize.toFloat())
                setBackgroundColor(Color.BLACK)
            }.also { vtView = it }
        } else {
            TerminalView(this, null).apply {
                setTerminalViewClient(ViewClient())
                setTextSize(fontSize)
                setTypeface(Typeface.MONOSPACE)
                setBackgroundColor(Color.BLACK)
                isFocusable = true
                isFocusableInTouchMode = true
            }.also { terminal = it }
        }
        status = TextView(this).apply {
            setTextColor(Color.LTGRAY)
            gravity = Gravity.CENTER
            setText(R.string.starting)
        }
        tabs = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        keys = ExtraKeysView(this, Keys())

        val tabBar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(BAR_COLOR)
            addView(HorizontalScrollView(this@MainActivity).apply {
                isHorizontalScrollBarEnabled = false
                addView(tabs)
            }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            addView(barButton("+") { newSession() })
            addView(barButton("⋮") { showMenu(it) })
        }
        val body = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(screen)
            addView(status)
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(BAR_COLOR)
            addView(tabBar)
            addView(body, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(keys)
        }
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
        vtView?.attach(null)
        super.onDestroy()
    }

    // ---- TerminalService.Listener ----

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

    override fun onProgress(text: String) {
        screen.visibility = View.INVISIBLE
        status.visibility = View.VISIBLE
        status.text = text
    }

    /**
     * A distro switch failed (the download failed, or the import failed and fell back to Alpine).
     */
    override fun onNotice() {
        val text = service?.takeNotice() ?: return
        AlertDialog.Builder(this)
            .setMessage(text)
            .setPositiveButton(android.R.string.ok, null)
            .show()
    }

    override fun onSessionsChanged() {
        val s = service ?: return
        if (current != null && current !in s.sessions) {
            current = null
            s.sessions.lastOrNull()?.let { show(it) }
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

    override fun onTextChanged(session: Session) {
        if (session === current) terminal?.onScreenUpdated()
    }

    override fun onExitAll() {
        finish()
    }

    override fun onRestarting() {
        current = null
        vtView?.attach(null)
        screen.visibility = View.INVISIBLE
        status.visibility = View.VISIBLE
        status.setText(R.string.restarting)
    }

    private fun showMenu(anchor: View) {
        PopupMenu(this, anchor).apply {
            menu.add(R.string.menu_settings).setOnMenuItemClickListener {
                startActivity(Intent(this@MainActivity, SettingsActivity::class.java))
                true
            }
            show()
        }
    }

    // ---- Sessions ----

    private fun newSession() {
        val s = service ?: return
        if (s.machine == 0L) return
        show(s.createSession())
    }

    private fun show(session: Session) {
        current = session
        service?.current = session
        when (session) {
            is TermuxSession -> terminal?.apply {
                attachSession(session.terminal)
                onScreenUpdated()
            }
            // Starts the shell once the view knows the grid (VtHost.onGrid)
            is VtSession -> vtView?.attach(session)
        }
        screen.requestFocus()
        renderTabs()
    }

    private fun close(session: Session) {
        service?.removeSession(session)
    }

    private fun renderTabs() {
        val s = service ?: return
        tabs.removeAllViews()
        s.sessions.forEachIndexed { i, session ->
            val title = session.title?.takeIf { it.isNotBlank() } ?: "sh"
            val label = "${i + 1}: $title" + if (session.isRunning) "" else " ✕"
            tabs.addView(barButton(label) { show(session) }.apply {
                if (session == current) setTextColor(Color.WHITE) else setTextColor(Color.GRAY)
                setOnLongClickListener {
                    close(session)
                    true
                }
            })
        }
    }

    private fun barButton(label: String, onClick: (View) -> Unit) = TextView(this).apply {
        text = label
        setTextColor(Color.LTGRAY)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        setPadding(dp(12), dp(10), dp(12), dp(10))
        setOnClickListener(onClick)
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    private fun showKeyboard() {
        screen.requestFocus()
        getSystemService(InputMethodManager::class.java).showSoftInput(screen, 0)
    }

    /** The extra keys go to the tab's view. */
    private inner class Keys : ExtraKeysView.Target {
        override fun key(keyCode: Int) {
            vtView?.let {
                it.press(keyCode)
                return
            }
            // Through TerminalView, so modifiers and the terminal's key modes (application cursor
            // keys, say) take effect; it needs a session
            val view = terminal?.takeIf { it.currentSession != null } ?: return
            var mod = 0
            if (keys.consumeCtrl()) mod = mod or KeyHandler.KEYMOD_CTRL
            if (keys.consumeAlt()) mod = mod or KeyHandler.KEYMOD_ALT
            view.handleKeyCode(keyCode, mod)
        }

        override fun text(text: String) {
            vtView?.let {
                it.type(text)
                return
            }
            // TerminalView reads Ctrl and Alt itself (ViewClient.readControlKey)
            val view = terminal?.takeIf { it.currentSession != null } ?: return
            for (c in text) view.inputCodePoint(KeyEvent.KEYCODE_UNKNOWN, c.code, false, false)
        }
    }

    /** The [VtView]'s side: starting the shell, input, zoom and the extra keys' modifiers. */
    private inner class VtHost : VtView.Host {
        override fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
            (current as? VtSession)?.start(cols, rows, cellWidth, cellHeight)
        }

        override fun onInput(data: ByteArray) {
            val session = current as? VtSession ?: return
            if (session.isRunning) {
                session.write(data)
            } else if (data.any { it == '\r'.code.toByte() || it == '\n'.code.toByte() }) {
                // When the session has ended, Enter closes it, as in Termux (from the IME too)
                close(session)
            }
        }

        override fun onZoom(fontPx: Int) {
            fontSize = fontPx
            getPreferences(MODE_PRIVATE).edit().putInt(PREF_FONT_SIZE, fontSize).apply()
        }

        override val ctrl: Boolean get() = keys.ctrl
        override val alt: Boolean get() = keys.alt

        override fun modifiersUsed() = keys.modifiersUsed()
    }

    /** TerminalView callbacks: input, zoom, key modifiers. */
    private inner class ViewClient : TerminalViewClient {
        override fun onScale(scale: Float): Float {
            if (scale in 0.9f..1.1f) return scale
            fontSize = (fontSize + if (scale > 1f) dp(1) else -dp(1)).coerceIn(dp(6), dp(32))
            terminal?.setTextSize(fontSize)
            getPreferences(MODE_PRIVATE).edit().putInt(PREF_FONT_SIZE, fontSize).apply()
            return 1f
        }

        override fun onSingleTapUp(e: MotionEvent) = showKeyboard()

        override fun shouldBackButtonBeMappedToEscape() = false

        override fun shouldEnforceCharBasedInput() = false

        override fun shouldUseCtrlSpaceWorkaround() = false

        override fun isTerminalViewSelected() = true

        override fun copyModeChanged(copyMode: Boolean) {}

        override fun onKeyDown(keyCode: Int, e: KeyEvent, session: TerminalSession): Boolean {
            // When the session has ended, Enter closes it, as in Termux
            if (!session.isRunning && keyCode == KeyEvent.KEYCODE_ENTER) {
                current?.let { close(it) }
                return true
            }
            return false
        }

        override fun onKeyUp(keyCode: Int, e: KeyEvent) = false

        override fun onLongPress(event: MotionEvent) = false

        override fun readControlKey() = keys.consumeCtrl()

        override fun readAltKey() = keys.consumeAlt()

        override fun readShiftKey() = false

        override fun readFnKey() = false

        override fun onCodePoint(codePoint: Int, ctrlDown: Boolean, session: TerminalSession) = false

        override fun onEmulatorSet() {}

        override fun logError(tag: String?, message: String?) {
            Log.e(tag, message.orEmpty())
        }

        override fun logWarn(tag: String?, message: String?) {
            Log.w(tag, message.orEmpty())
        }

        override fun logInfo(tag: String?, message: String?) {
            Log.i(tag, message.orEmpty())
        }

        override fun logDebug(tag: String?, message: String?) {
            Log.d(tag, message.orEmpty())
        }

        override fun logVerbose(tag: String?, message: String?) {
            Log.v(tag, message.orEmpty())
        }

        override fun logStackTraceWithMessage(tag: String?, message: String?, e: Exception?) {
            Log.e(tag, message.orEmpty(), e)
        }

        override fun logStackTrace(tag: String?, e: Exception?) {
            Log.e(tag, "", e)
        }
    }

    companion object {
        val BAR_COLOR = Color.rgb(0x20, 0x20, 0x20)
        private const val PREF_FONT_SIZE = "font_size"
    }
}
