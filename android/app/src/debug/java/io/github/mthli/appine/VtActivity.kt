package io.github.mthli.appine

import android.app.Activity
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.graphics.Color
import android.os.Bundle
import android.os.IBinder
import android.util.TypedValue
import android.view.Gravity
import android.view.KeyEvent
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.TextView
import android.widget.Toast
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import java.io.IOException
import java.util.concurrent.Executors

/**
 * The experimental terminal (debug builds, opened from the main screen's menu; the android/terminal
 * map): one login shell on its own kernel console, shown by [VtView] instead of Termux's view.
 * It is not a tab of [TerminalService]: it is not saved or restored, and kill reports do not reach
 * it. It needs the native library's `vt` feature ([VtNative.available]).
 */
class VtActivity : Activity(), TerminalService.Listener, VtView.Host {

    private lateinit var view: VtView
    private lateinit var ctrlKey: TextView
    private lateinit var altKey: TextView
    private var service: TerminalService? = null
    private var grid: IntArray? = null
    private var pty: ConsolePty? = null
    private var machine = 0L
    private var tab = 0L
    private val writer = Executors.newSingleThreadExecutor { Thread(it, "vt-writer") }

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val s = (binder as TerminalService.LocalBinder).service
            service = s
            s.addListener(this@VtActivity)
            start()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            service = null
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (!VtNative.available) {
            Toast.makeText(this, R.string.vt_unavailable, Toast.LENGTH_LONG).show()
            finish()
            return
        }
        view = VtView(this, this)
        view.onModifiersUsed = { refreshKeys() }
        val keys = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(MainActivity.BAR_COLOR)
        }
        fun key(label: String, onClick: () -> Unit) = TextView(this).apply {
            text = label
            gravity = Gravity.CENTER
            setTextColor(Color.LTGRAY)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            val pad = (10 * resources.displayMetrics.density).toInt()
            setPadding(0, pad, 0, pad)
            setOnClickListener { onClick() }
            keys.addView(this, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
        }
        key("ESC") { view.press(KeyEvent.KEYCODE_ESCAPE) }
        key("TAB") { view.press(KeyEvent.KEYCODE_TAB) }
        ctrlKey = key("CTRL") {
            view.ctrlOnce = !view.ctrlOnce
            refreshKeys()
        }
        altKey = key("ALT") {
            view.altOnce = !view.altOnce
            refreshKeys()
        }
        key("-") { view.type("-") }
        key("/") { view.type("/") }
        key("|") { view.type("|") }
        key("←") { view.press(KeyEvent.KEYCODE_DPAD_LEFT) }
        key("↓") { view.press(KeyEvent.KEYCODE_DPAD_DOWN) }
        key("↑") { view.press(KeyEvent.KEYCODE_DPAD_UP) }
        key("→") { view.press(KeyEvent.KEYCODE_DPAD_RIGHT) }

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(Color.BLACK)
            addView(view, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
            addView(keys)
        }
        WindowCompat.setDecorFitsSystemWindows(window, false)
        ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.ime())
            v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            WindowInsetsCompat.CONSUMED
        }
        setContentView(root)
        view.requestFocus()
    }

    override fun onStart() {
        super.onStart()
        if (!VtNative.available) return
        bindService(Intent(this, TerminalService::class.java), connection, BIND_AUTO_CREATE)
    }

    override fun onStop() {
        if (VtNative.available) {
            service?.removeListener(this)
            unbindService(connection)
            service = null
        }
        super.onStop()
    }

    override fun onDestroy() {
        // Closing the console ends the pump; the tab's terminal goes when the pump returns
        pty?.close()
        if (tab != 0L) VtNative.destroy(tab)
        writer.shutdown()
        super.onDestroy()
    }

    private fun refreshKeys() {
        ctrlKey.setTextColor(if (view.ctrlOnce) Color.CYAN else Color.LTGRAY)
        altKey.setTextColor(if (view.altOnce) Color.CYAN else Color.LTGRAY)
    }

    override fun onMachineReady(error: String?) = start()

    override fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
        grid = intArrayOf(cols, rows, cellWidth, cellHeight)
        start()
    }

    /** Starts the shell once both the kernel and the grid size are known. */
    private fun start() {
        val s = service ?: return
        val g = grid ?: return
        if (tab != 0L || pty != null || s.machine == 0L) return
        val m = s.machine
        val p = ConsolePty(m, ConsolePty.script(this), s.login)
        try {
            p.start(g[0], g[1])
        } catch (e: IOException) {
            Toast.makeText(this, e.message, Toast.LENGTH_LONG).show()
            return
        }
        pty = p
        machine = m
        val t = VtNative.create(m, p.console, g[0], g[1], g[2], g[3])
        tab = t
        view.tab = t
        Thread({
            VtNative.pump(t, view)
            val code = p.waitFor()
            runOnUiThread {
                if (tab == t) {
                    VtNative.print(t, "\r\n${getString(R.string.vt_exited, code)}\r\n".toByteArray())
                    view.invalidate()
                }
            }
        }, "vt-pump").start()
    }

    override fun onInput(data: ByteArray) {
        val p = pty ?: return
        val m = machine
        writer.execute { Native.write(m, p.console, data, 0, data.size) }
    }
}
