package io.github.mthli.appine

import android.annotation.SuppressLint
import android.app.Activity
import android.graphics.Color
import android.graphics.Typeface
import android.os.Build
import android.os.Bundle
import android.util.TypedValue
import android.view.Gravity
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.TextView
import android.window.OnBackInvokedCallback
import android.window.OnBackInvokedDispatcher
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat

/**
 * 开源许可（设计文档 §13.4）：构建时生成的 `assets/notices/`（`NoticesTask`）。先是列表，点一项
 * 看全文；全文保持原来的换行（许可证按 70 多列排好），放不下时横向滚动。
 */
class LicensesActivity : Activity() {

    private data class Entry(val title: String, val subtitle: String, val file: String)

    private lateinit var title: TextView

    /** 放列表或全文。 */
    private lateinit var body: FrameLayout
    private lateinit var list: View
    private var showing: Entry? = null

    /** 看全文时返回键回到列表（Android 13 起；之前的版本走 [onBackPressed]）。 */
    private var backCallback: Any? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val entries = assets.open("notices/index.tsv").bufferedReader().readLines()
            .mapNotNull { line -> line.split('\t').takeIf { it.size == 3 }?.let { Entry(it[0], it[1], it[2]) } }

        title = TextView(this).apply {
            setText(R.string.settings_licenses)
            setTextColor(Color.WHITE)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            maxLines = 1
        }
        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(MainActivity.BAR_COLOR)
            addView(TextView(this@LicensesActivity).apply {
                text = "←"
                setTextColor(Color.LTGRAY)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
                setPadding(dp(16), dp(10), dp(16), dp(10))
                setOnClickListener { back() }
            })
            addView(title)
        }
        list = ScrollView(this).apply {
            addView(LinearLayout(this@LicensesActivity).apply {
                orientation = LinearLayout.VERTICAL
                for (e in entries) addView(row(e))
            })
        }
        body = FrameLayout(this).apply {
            setBackgroundColor(Color.BLACK)
            addView(list)
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(MainActivity.BAR_COLOR)
            addView(bar)
            addView(body, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
        }
        WindowCompat.setDecorFitsSystemWindows(window, false)
        ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars())
            v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            WindowInsetsCompat.CONSUMED
        }
        setContentView(root)
    }

    // 只有 Android 13 之前的系统调用它；之后的走 interceptBack 登记的 OnBackInvokedCallback
    @SuppressLint("GestureBackNavigation")
    @Deprecated("Deprecated in Java")
    override fun onBackPressed() {
        if (showing != null) back() else @Suppress("DEPRECATION") super.onBackPressed()
    }

    private fun interceptBack(on: Boolean) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
        val current = backCallback as OnBackInvokedCallback?
        if (on && current == null) {
            val cb = OnBackInvokedCallback { back() }
            onBackInvokedDispatcher.registerOnBackInvokedCallback(OnBackInvokedDispatcher.PRIORITY_DEFAULT, cb)
            backCallback = cb
        } else if (!on && current != null) {
            onBackInvokedDispatcher.unregisterOnBackInvokedCallback(current)
            backCallback = null
        }
    }

    private fun row(e: Entry) = LinearLayout(this).apply {
        orientation = LinearLayout.VERTICAL
        setPadding(dp(20), dp(12), dp(20), dp(12))
        addView(TextView(this@LicensesActivity).apply {
            text = e.title
            setTextColor(Color.WHITE)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 15f)
        })
        addView(TextView(this@LicensesActivity).apply {
            text = e.subtitle
            setTextColor(Color.GRAY)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 13f)
        })
        val ripple = TypedValue()
        theme.resolveAttribute(android.R.attr.selectableItemBackground, ripple, true)
        setBackgroundResource(ripple.resourceId)
        setOnClickListener { show(e) }
    }

    /** 看一项的全文。 */
    private fun show(e: Entry) {
        val text = assets.open("notices/${e.file}").bufferedReader().readText()
        showing = e
        interceptBack(true)
        title.text = e.title
        body.removeAllViews()
        body.addView(ScrollView(this).apply {
            addView(HorizontalScrollView(this@LicensesActivity).apply {
                addView(TextView(this@LicensesActivity).apply {
                    this.text = text
                    setTextColor(Color.LTGRAY)
                    typeface = Typeface.MONOSPACE
                    fitColumns(this)
                    setTextIsSelectable(true)
                    setPadding(dp(12), dp(12), dp(12), dp(12))
                })
            })
        })
    }

    /**
     * 字号取 [COLUMNS] 列正好放得下屏幕宽度的大小（许可证原文和生成的声明都按 72 列排），最大
     * 13sp：手机上不用横向滚动，平板上也不会太大。
     */
    private fun fitColumns(view: TextView) {
        val width = resources.displayMetrics.widthPixels - dp(24)
        view.setTextSize(TypedValue.COMPLEX_UNIT_PX, 100f)
        val perChar = view.paint.measureText("m".repeat(COLUMNS)) / COLUMNS / 100f
        val max = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, 13f, resources.displayMetrics)
        view.setTextSize(TypedValue.COMPLEX_UNIT_PX, minOf(max, width / (COLUMNS * perChar)))
    }

    /** 从全文回到列表；在列表上时关掉。 */
    private fun back() {
        if (showing == null) {
            finish()
            return
        }
        showing = null
        interceptBack(false)
        title.setText(R.string.settings_licenses)
        body.removeAllViews()
        body.addView(list)
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    companion object {
        private const val COLUMNS = 74
    }
}
