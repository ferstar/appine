package io.github.mthli.appine

import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.graphics.Color
import android.graphics.Typeface
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.Drawable
import android.graphics.drawable.GradientDrawable
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.text.TextUtils
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.BaseAdapter
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.ListPopupWindow
import android.widget.PopupMenu
import android.widget.TextView
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat

/**
 * The terminal screen: the tab bar on top, the terminal ([VtView]) in the middle, and below it the
 * keys the soft keyboard lacks (Esc, Ctrl, arrows and so on).
 * Sessions belong to [TerminalService]; this only handles display and input: the view shows the
 * current tab's [VtSession].
 *
 * The tab bar lists every tab in a row only when the window is at least [WIDE_DP] wide; narrower
 * (a phone held upright) it shows the number of tabs and the current tab's title, and tapping them
 * opens the list of tabs.
 */
class MainActivity : Activity(), TerminalService.Listener {

    private lateinit var screen: VtView
    private lateinit var bar: LinearLayout
    private lateinit var strip: HorizontalScrollView
    private lateinit var tabs: LinearLayout
    private lateinit var switcher: LinearLayout
    private lateinit var tabCount: TextView
    private lateinit var tabTitle: TextView
    private lateinit var status: TextView
    private lateinit var keys: ExtraKeysView

    private var service: TerminalService? = null
    private var current: VtSession? = null
    private var fontSize = 0

    /** The list of tabs while it is open (narrow windows), showing [tabList]. */
    private var list: ListPopupWindow? = null
    private val tabList = TabList()

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
        screen = VtView(this, Host()).apply {
            setFontSize(fontSize.toFloat())
            setBackgroundColor(Color.BLACK)
        }
        status = TextView(this).apply {
            setTextColor(Color.LTGRAY)
            gravity = Gravity.CENTER
            setText(R.string.starting)
        }
        tabs = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        strip = HorizontalScrollView(this).apply {
            isHorizontalScrollBarEnabled = false
            addView(tabs)
        }
        // The number of tabs in a box, like a browser's tab switcher
        tabCount = TextView(this).apply {
            setTextColor(Color.LTGRAY)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 12f)
            typeface = Typeface.DEFAULT_BOLD
            gravity = Gravity.CENTER
            minWidth = dp(20)
            minHeight = dp(20)
            setPadding(dp(4), 0, dp(4), 0)
            background = GradientDrawable().apply {
                setStroke(dp(2), Color.LTGRAY)
                cornerRadius = dp(4).toFloat()
            }
        }
        tabTitle = TextView(this).apply {
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            isSingleLine = true
            ellipsize = TextUtils.TruncateAt.END
        }
        switcher = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(12), dp(10), dp(12), dp(10))
            background = touchFeedback(borderless = false)
            addView(tabCount)
            addView(tabTitle, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f).apply {
                marginStart = dp(12)
            })
            setOnClickListener { openList() }
        }
        keys = ExtraKeysView(this, Keys())

        bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(BAR_COLOR)
            addView(strip, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            addView(switcher, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            addView(barButton("+") { newSession() }.apply { contentDescription = getString(R.string.new_tab) })
            addView(barButton("⋮") { showMenu(it) })
        }
        renderTabs()
        val body = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(screen)
            addView(status)
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(BAR_COLOR)
            addView(bar)
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
        list?.dismiss()
        service?.removeListener(this)
        unbindService(connection)
        service = null
        super.onStop()
    }

    override fun onDestroy() {
        list?.dismiss()
        // Sessions outlive the Activity: do not leave them pointing at this view
        screen.attach(null)
        super.onDestroy()
    }

    /** The activity handles size changes itself: the bar may switch between the row and the list. */
    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // The list is as wide as the bar was when it opened
        list?.dismiss()
        renderTabs()
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
            list?.dismiss()
            finish()
            return
        }
        renderTabs()
    }

    override fun onExitAll() {
        finish()
    }

    override fun onRestarting() {
        current = null
        screen.attach(null)
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

    private fun show(session: VtSession) {
        current = session
        service?.current = session
        // Starts the shell once the view knows the grid (Host.onGrid)
        screen.attach(session)
        screen.requestFocus()
        renderTabs()
    }

    private fun close(session: VtSession) {
        service?.removeSession(session)
    }

    /**
     * Shows the tabs: a row of all of them in wide windows, else their number and the current one's
     * title (and the open list, kept up to date). Called whenever tabs, titles, the current tab or
     * the window's width change.
     */
    private fun renderTabs() {
        val sessions = service?.sessions.orEmpty().toList()
        tabList.items = sessions
        val wide = resources.configuration.screenWidthDp >= WIDE_DP
        strip.visibility = if (wide) View.VISIBLE else View.GONE
        // Invisible rather than gone, so + and ⋮ stay at the right
        switcher.visibility = when {
            wide -> View.GONE
            sessions.isEmpty() -> View.INVISIBLE
            else -> View.VISIBLE
        }
        if (wide || sessions.isEmpty()) list?.dismiss()
        tabs.removeAllViews()

        if (!wide) {
            val i = sessions.indexOf(current)
            tabCount.text = "%d".format(sessions.size)
            tabCount.contentDescription = resources.getQuantityString(R.plurals.tabs, sessions.size, sessions.size)
            tabTitle.text = if (i < 0) "" else label(i, sessions[i])
            tabTitle.setTextColor(if (current?.isRunning == false) Color.GRAY else Color.WHITE)
            return
        }
        sessions.forEachIndexed { i, session ->
            tabs.addView(barButton(label(i, session)) { show(session) }.apply {
                maxWidth = dp(TAB_MAX_DP)
                isSingleLine = true
                ellipsize = TextUtils.TruncateAt.END
                setTextColor(if (session == current) Color.WHITE else Color.GRAY)
                setOnLongClickListener { v ->
                    PopupMenu(this@MainActivity, v).apply {
                        menu.add(R.string.close_tab).setOnMenuItemClickListener {
                            close(session)
                            true
                        }
                        show()
                    }
                    true
                }
            })
        }
        // Bring the current tab into view once the new row is laid out (this runs after the
        // layout pass that adding the tabs requested)
        val shown = tabs.getChildAt(sessions.indexOf(current)) ?: return
        strip.post {
            when {
                shown.left < strip.scrollX -> strip.smoothScrollTo(shown.left, 0)
                shown.right > strip.scrollX + strip.width -> strip.smoothScrollTo(shown.right - strip.width, 0)
            }
        }
    }

    /** `N: title` (`sh` when the program set none), marked once the shell has ended. */
    private fun label(i: Int, session: VtSession): String {
        val text = "${i + 1}: ${session.title?.takeIf { it.isNotBlank() } ?: "sh"}"
        return if (session.isRunning) text else getString(R.string.tab_ended, text)
    }

    /** Opens the list of tabs below the bar: tap a tab to show it, ✕ to close it. */
    private fun openList() {
        if (tabList.items.isEmpty()) return
        list?.dismiss()
        list = ListPopupWindow(this).apply {
            anchorView = bar
            setAdapter(tabList)
            setBackgroundDrawable(ColorDrawable(LIST_COLOR))
            isModal = true
            // The IME stays up (the list may cover it), as the terminal still has the input focus
            inputMethodMode = ListPopupWindow.INPUT_METHOD_NOT_NEEDED
            setOnItemClickListener { _, _, position, _ ->
                tabList.items.getOrNull(position)?.let { show(it) }
                dismiss()
            }
            setOnDismissListener { list = null }
            show()
            setSelection(tabList.items.indexOf(current).coerceAtLeast(0))
        }
    }

    private fun barButton(label: String, onClick: (View) -> Unit) = TextView(this).apply {
        text = label
        setTextColor(Color.LTGRAY)
        setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        setPadding(dp(12), dp(10), dp(12), dp(10))
        background = touchFeedback(borderless = false)
        setOnClickListener(onClick)
    }

    /** The theme's ripple for something that can be tapped. */
    private fun touchFeedback(borderless: Boolean): Drawable? {
        val attr = if (borderless) {
            android.R.attr.selectableItemBackgroundBorderless
        } else {
            android.R.attr.selectableItemBackground
        }
        val a = obtainStyledAttributes(intArrayOf(attr))
        return a.getDrawable(0).also { a.recycle() }
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    /** The rows of the list of tabs: `N: title`, the current one highlighted, and a close button. */
    private inner class TabList : BaseAdapter() {
        var items: List<VtSession> = emptyList()
            set(value) {
                field = value
                notifyDataSetChanged()
            }

        override fun getCount() = items.size

        override fun getItem(position: Int) = items[position]

        override fun getItemId(position: Int) = position.toLong()

        override fun getView(position: Int, convertView: View?, parent: ViewGroup): View {
            val row = convertView as? LinearLayout ?: newRow()
            val text = row.getChildAt(0) as TextView
            val session = items[position]
            text.text = label(position, session)
            text.setTextColor(
                when {
                    !session.isRunning -> Color.GRAY
                    session == current -> Color.WHITE
                    else -> Color.LTGRAY
                }
            )
            row.setBackgroundColor(if (session == current) CURRENT_COLOR else Color.TRANSPARENT)
            // Not focusable, so the rest of the row still takes the list's taps
            row.getChildAt(1).setOnClickListener { close(session) }
            return row
        }

        private fun newRow() = LinearLayout(this@MainActivity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            minimumHeight = dp(48)
            addView(TextView(this@MainActivity).apply {
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
                isSingleLine = true
                ellipsize = TextUtils.TruncateAt.END
                setPadding(dp(16), dp(12), dp(8), dp(12))
            }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            addView(TextView(this@MainActivity).apply {
                text = "✕"
                setTextColor(Color.LTGRAY)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
                gravity = Gravity.CENTER
                contentDescription = getString(R.string.close_tab)
                background = touchFeedback(borderless = true)
            }, LinearLayout.LayoutParams(dp(48), dp(48)))
        }
    }

    /** The extra keys go to the view, which reads the one-shot modifiers through [Host]. */
    private inner class Keys : ExtraKeysView.Target {
        override fun key(keyCode: Int) = screen.press(keyCode)

        override fun text(text: String) = screen.type(text)
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
            getPreferences(MODE_PRIVATE).edit().putInt(PREF_FONT_SIZE, fontSize).apply()
        }

        override val ctrl: Boolean get() = keys.ctrl
        override val alt: Boolean get() = keys.alt

        override fun modifiersUsed() = keys.modifiersUsed()
    }

    companion object {
        val BAR_COLOR = Color.rgb(0x20, 0x20, 0x20)
        private val LIST_COLOR = Color.rgb(0x2a, 0x2a, 0x2a)
        private val CURRENT_COLOR = Color.rgb(0x3a, 0x3a, 0x3a)
        private const val PREF_FONT_SIZE = "font_size"

        /** From this window width (dp, the usual tablet breakpoint) the bar lists every tab in a row. */
        private const val WIDE_DP = 600

        /** The widest a tab in the row gets; longer titles end in "…". */
        private const val TAB_MAX_DP = 220
    }
}
