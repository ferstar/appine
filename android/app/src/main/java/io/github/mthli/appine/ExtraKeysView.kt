package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.graphics.Color
import android.util.TypedValue
import android.view.Gravity
import android.view.KeyEvent
import android.widget.LinearLayout
import android.widget.TextView
import com.termux.terminal.KeyHandler
import com.termux.view.TerminalView

/**
 * 软键盘上没有的按键。Ctrl 和 Alt 是一次性的修饰键：按一下之后作用于下一个输入的键。
 * Termux 的扩展键栏属于 GPLv3 的主模块，这里是重新写的（android/terminal 的 map）。
 */
@SuppressLint("ViewConstructor") // 只在代码里创建，不出现在布局文件里
internal class ExtraKeysView(
    context: Context,
    private val terminal: () -> TerminalView,
) : LinearLayout(context) {

    private var ctrl = false
    private var alt = false
    private val ctrlKey: TextView
    private val altKey: TextView

    init {
        orientation = HORIZONTAL
        setBackgroundColor(MainActivity.BAR_COLOR)
        key("ESC") { send(KeyEvent.KEYCODE_ESCAPE) }
        key("TAB") { send(KeyEvent.KEYCODE_TAB) }
        ctrlKey = key("CTRL") {
            ctrl = !ctrl
            refresh()
        }
        altKey = key("ALT") {
            alt = !alt
            refresh()
        }
        key("-") { type("-") }
        key("/") { type("/") }
        key("|") { type("|") }
        key("←") { send(KeyEvent.KEYCODE_DPAD_LEFT) }
        key("↓") { send(KeyEvent.KEYCODE_DPAD_DOWN) }
        key("↑") { send(KeyEvent.KEYCODE_DPAD_UP) }
        key("→") { send(KeyEvent.KEYCODE_DPAD_RIGHT) }
    }

    /** TerminalView 读修饰键时调用：读一次就复位。 */
    fun consumeCtrl(): Boolean = ctrl.also {
        if (it) {
            ctrl = false
            post { refresh() }
        }
    }

    fun consumeAlt(): Boolean = alt.also {
        if (it) {
            alt = false
            post { refresh() }
        }
    }

    private fun refresh() {
        ctrlKey.setTextColor(if (ctrl) Color.CYAN else Color.LTGRAY)
        altKey.setTextColor(if (alt) Color.CYAN else Color.LTGRAY)
    }

    private fun key(label: String, onClick: () -> Unit): TextView {
        val view = TextView(context).apply {
            text = label
            gravity = Gravity.CENTER
            setTextColor(Color.LTGRAY)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
            val pad = (10 * resources.displayMetrics.density).toInt()
            setPadding(0, pad, 0, pad)
            setOnClickListener { onClick() }
        }
        addView(view, LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f))
        return view
    }

    /** 特殊键：经过 TerminalView，修饰键和终端的按键模式（比如应用光标键）都会生效。 */
    private fun send(keyCode: Int) {
        var mod = 0
        if (consumeCtrl()) mod = mod or KeyHandler.KEYMOD_CTRL
        if (consumeAlt()) mod = mod or KeyHandler.KEYMOD_ALT
        terminal().handleKeyCode(keyCode, mod)
    }

    /** 普通字符：Ctrl 和 Alt 由 TerminalView 读取（[consumeCtrl]、[consumeAlt]）。 */
    private fun type(text: String) {
        val view = terminal()
        for (c in text) view.inputCodePoint(KeyEvent.KEYCODE_UNKNOWN, c.code, false, false)
    }
}
