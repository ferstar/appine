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
import com.termux.terminal.TerminalSession
import com.termux.view.TerminalView
import com.termux.view.TerminalViewClient

/**
 * 终端界面：上面是标签栏，中间是终端，下面是软键盘上没有的按键（Esc、Ctrl、方向键等）。
 * 会话属于 [TerminalService]，这里只负责显示和输入。
 */
class MainActivity : Activity(), TerminalService.Listener {

    private lateinit var terminal: TerminalView
    private lateinit var tabs: LinearLayout
    private lateinit var status: TextView
    private lateinit var keys: ExtraKeysView

    private var service: TerminalService? = null
    private var current: TerminalSession? = null
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
        terminal = TerminalView(this, null).apply {
            setTerminalViewClient(ViewClient())
            setTextSize(fontSize)
            setTypeface(Typeface.MONOSPACE)
            setBackgroundColor(Color.BLACK)
            isFocusable = true
            isFocusableInTouchMode = true
        }
        status = TextView(this).apply {
            setTextColor(Color.LTGRAY)
            gravity = Gravity.CENTER
            setText(R.string.starting)
        }
        tabs = LinearLayout(this).apply { orientation = LinearLayout.HORIZONTAL }
        keys = ExtraKeysView(this) { terminal }

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
            addView(terminal)
            addView(status)
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(BAR_COLOR)
            addView(tabBar)
            addView(body, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(keys)
        }
        // 内容画到系统栏后面（Android 15 起 targetSdk 35 的默认行为，这里在所有版本上都这样
        // 做）：按系统栏和输入法让出位置
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

    // ---- TerminalService.Listener ----

    override fun onMachineReady(error: String?) {
        val s = service ?: return
        if (error != null) {
            terminal.visibility = View.INVISIBLE
            status.visibility = View.VISIBLE
            status.text = getString(R.string.start_failed, error)
            return
        }
        status.visibility = View.GONE
        terminal.visibility = View.VISIBLE
        if (s.sessions.isEmpty()) {
            newSession()
        } else {
            // 被杀之后恢复的标签页：显示原来正在看的那个
            show(current?.takeIf { it in s.sessions } ?: s.focus?.takeIf { it in s.sessions } ?: s.sessions.last())
        }
    }

    override fun onProgress(text: String) {
        terminal.visibility = View.INVISIBLE
        status.visibility = View.VISIBLE
        status.text = text
    }

    /** 换发行版没有成功（下载失败、导入失败退回了 Alpine）。 */
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
            // 最后一个标签页关掉了：界面退出。服务还有 guest 进程（OpenRC 起的服务等）时留在
            // 后台，没有了就自己退出（设计文档 §11.2）
            finish()
            return
        }
        renderTabs()
    }

    override fun onTextChanged(session: TerminalSession) {
        if (session == current) terminal.onScreenUpdated()
    }

    override fun onExitAll() {
        finish()
    }

    override fun onRestarting() {
        current = null
        terminal.visibility = View.INVISIBLE
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

    // ---- 会话 ----

    private fun newSession() {
        val s = service ?: return
        if (s.machine == 0L) return
        show(s.createSession())
    }

    private fun show(session: TerminalSession) {
        current = session
        service?.current = session
        terminal.attachSession(session)
        terminal.onScreenUpdated()
        terminal.requestFocus()
        renderTabs()
    }

    private fun close(session: TerminalSession) {
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
        terminal.requestFocus()
        getSystemService(InputMethodManager::class.java).showSoftInput(terminal, 0)
    }

    /** TerminalView 的回调：输入、缩放、按键修饰键。 */
    private inner class ViewClient : TerminalViewClient {
        override fun onScale(scale: Float): Float {
            if (scale in 0.9f..1.1f) return scale
            fontSize = (fontSize + if (scale > 1f) dp(1) else -dp(1)).coerceIn(dp(6), dp(32))
            terminal.setTextSize(fontSize)
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
            // 会话已经结束时按回车关闭它，与 Termux 相同
            if (!session.isRunning && keyCode == KeyEvent.KEYCODE_ENTER) {
                close(session)
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
