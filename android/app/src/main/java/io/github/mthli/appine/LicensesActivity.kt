package io.github.mthli.appine

import android.graphics.Typeface
import android.os.Bundle
import android.util.TypedValue
import android.view.View
import android.view.ViewGroup
import android.widget.FrameLayout
import android.widget.HorizontalScrollView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updatePadding
import androidx.core.widget.NestedScrollView
import com.google.android.material.appbar.AppBarLayout
import com.google.android.material.appbar.MaterialToolbar
import com.google.android.material.color.DynamicColors

/**
 * Open source licenses (design doc §13.4): the `assets/notices/` generated at build time
 * (`NoticesTask`). A list first, and tapping an entry shows its full text; the full text keeps its
 * original line breaks (licenses are laid out at seventy-some columns) and scrolls sideways when it
 * does not fit. The same Material 3 page as Settings (page.xml, list_item.xml).
 */
class LicensesActivity : AppCompatActivity() {

    private data class Entry(val title: String, val subtitle: String, val file: String)

    private lateinit var entries: List<Entry>
    private lateinit var toolbar: MaterialToolbar
    private lateinit var appBar: AppBarLayout

    /** Holds the list or the full text. */
    private lateinit var content: FrameLayout
    private lateinit var list: NestedScrollView
    private var showing: Entry? = null

    /** The navigation bar's height, kept clear at the end of the list and of a full text. */
    private var bottomInset = 0

    /** While a full text is shown, back returns to the list. */
    private val backToList = object : OnBackPressedCallback(false) {
        override fun handleOnBackPressed() = back()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        DynamicColors.applyToActivityIfAvailable(this)
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        entries = assets.open("notices/index.tsv").bufferedReader().readLines()
            .mapNotNull { line -> line.split('\t').takeIf { it.size == 3 }?.let { Entry(it[0], it[1], it[2]) } }

        setContentView(R.layout.page)
        appBar = findViewById(R.id.app_bar)
        content = findViewById(R.id.content)
        toolbar = findViewById<MaterialToolbar>(R.id.toolbar).apply {
            setTitle(R.string.settings_licenses)
            setNavigationOnClickListener { back() }
        }
        val rows = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        for (e in entries) rows.addView(row(rows, e))
        list = scrolling(rows)
        content.addView(list)
        onBackPressedDispatcher.addCallback(this, backToList)
        ViewCompat.setOnApplyWindowInsetsListener(findViewById(R.id.root)) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            v.setPadding(bars.left, 0, bars.right, 0)
            appBar.updatePadding(top = bars.top)
            bottomInset = bars.bottom
            list.updatePadding(bottom = bottomInset)
            content.getChildAt(0)?.updatePadding(bottom = bottomInset)
            WindowInsetsCompat.CONSUMED
        }
        // Recreated (the system's dark mode changed): the same full text again
        savedInstanceState?.getString(STATE_SHOWING)?.let { file -> entries.firstOrNull { it.file == file }?.let(::show) }
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        showing?.let { outState.putString(STATE_SHOWING, it.file) }
    }

    private fun row(parent: ViewGroup, e: Entry): View = layoutInflater.inflate(R.layout.list_item, parent, false).apply {
        findViewById<TextView>(R.id.title).text = e.title
        findViewById<TextView>(R.id.summary).text = e.subtitle
        setOnClickListener { show(e) }
    }

    /** A vertically scrolling page of the content, clear of the navigation bar at its end. */
    private fun scrolling(child: View) = NestedScrollView(this).apply {
        clipToPadding = false
        updatePadding(bottom = bottomInset)
        addView(child)
    }

    /** Shows the full text of an entry. */
    private fun show(e: Entry) {
        val text = assets.open("notices/${e.file}").bufferedReader().readText()
        showing = e
        backToList.isEnabled = true
        toolbar.title = e.title
        content.removeAllViews()
        content.addView(scrolling(HorizontalScrollView(this).apply {
            addView(TextView(this@LicensesActivity).apply {
                this.text = text
                typeface = Typeface.MONOSPACE
                fitColumns(this)
                setTextIsSelectable(true)
                setPadding(dp(12), dp(12), dp(12), dp(12))
            })
        }))
        appBar.setLifted(false)
    }

    /**
     * The text size is the one at which [COLUMNS] columns just fit the screen width (the license
     * texts and the generated notices are both laid out at 72 columns), at most 13sp: no sideways
     * scrolling on phones, and not too big on tablets.
     */
    private fun fitColumns(view: TextView) {
        val width = resources.displayMetrics.widthPixels - dp(24)
        view.setTextSize(TypedValue.COMPLEX_UNIT_PX, 100f)
        val perChar = view.paint.measureText("m".repeat(COLUMNS)) / COLUMNS / 100f
        val max = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, 13f, resources.displayMetrics)
        view.setTextSize(TypedValue.COMPLEX_UNIT_PX, minOf(max, width / (COLUMNS * perChar)))
    }

    /** From the full text back to the list; closes when on the list. */
    private fun back() {
        if (showing == null) {
            finish()
            return
        }
        showing = null
        backToList.isEnabled = false
        toolbar.setTitle(R.string.settings_licenses)
        content.removeAllViews()
        content.addView(list)
        appBar.setLifted(list.scrollY > 0)
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()

    companion object {
        private const val COLUMNS = 74
        private const val STATE_SHOWING = "showing"
    }
}
