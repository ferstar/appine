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
 * Open source licenses from `assets/notices/`, generated at build time
 * (`NoticesTask`). A list first, and tapping an entry shows its full text; the full text keeps its
 * original line breaks (licenses are laid out at seventy-some columns) and scrolls sideways when it
 * does not fit. The same Material 3 page as Settings (page.xml, list_item.xml).
 */
class LicensesActivity : AppCompatActivity() {

    private data class Entry(val title: String, val subtitle: String, val file: String)

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
        setContentView(R.layout.page)
        appBar = findViewById(R.id.app_bar)
        content = findViewById(R.id.content)
        toolbar = findViewById<MaterialToolbar>(R.id.toolbar).apply {
            setNavigationOnClickListener { back() }
        }
        val entries = readEntries()
        list = createList(entries)
        showPage(null, list)
        onBackPressedDispatcher.addCallback(this, backToList)
        applyInsets()

        val file = savedInstanceState?.getString(STATE_SHOWING)
        entries.firstOrNull { it.file == file }?.let(::show)
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        showing?.let { outState.putString(STATE_SHOWING, it.file) }
    }

    private fun readEntries(): List<Entry> =
        assets.open("notices/index.tsv").bufferedReader().useLines { lines ->
            lines.mapNotNull { line ->
                val fields = line.split('\t')
                if (fields.size == 3) Entry(fields[0], fields[1], fields[2]) else null
            }.toList()
        }

    private fun createList(entries: List<Entry>): NestedScrollView {
        val rows = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL }
        for (entry in entries) rows.addView(row(rows, entry))
        return scrolling(rows)
    }

    private fun row(parent: ViewGroup, entry: Entry): View =
        layoutInflater.inflate(R.layout.list_item, parent, false).apply {
            findViewById<TextView>(R.id.title).text = entry.title
            findViewById<TextView>(R.id.summary).text = entry.subtitle
            setOnClickListener { show(entry) }
        }

    private fun applyInsets() {
        ViewCompat.setOnApplyWindowInsetsListener(findViewById(R.id.root)) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            v.setPadding(bars.left, 0, bars.right, 0)
            appBar.updatePadding(top = bars.top)
            bottomInset = bars.bottom
            list.updatePadding(bottom = bottomInset)
            content.getChildAt(0)?.updatePadding(bottom = bottomInset)
            WindowInsetsCompat.CONSUMED
        }
    }

    /** A vertically scrolling page of the content, clear of the navigation bar at its end. */
    private fun scrolling(child: View) = NestedScrollView(this).apply {
        clipToPadding = false
        updatePadding(bottom = bottomInset)
        addView(child)
    }

    /** Shows the full text of an entry. */
    private fun show(entry: Entry) {
        val text = assets.open("notices/${entry.file}").bufferedReader().use { it.readText() }
        val textView = TextView(this).apply {
            this.text = text
            typeface = Typeface.MONOSPACE
            fitColumns(this)
            setTextIsSelectable(true)
            setPadding(dp(TEXT_PADDING_DP), dp(TEXT_PADDING_DP), dp(TEXT_PADDING_DP), dp(TEXT_PADDING_DP))
        }
        val horizontal = HorizontalScrollView(this).apply { addView(textView) }
        showPage(entry, scrolling(horizontal))
    }

    /** Keeps navigation and the app bar in sync with the page being shown. */
    private fun showPage(entry: Entry?, page: NestedScrollView) {
        showing = entry
        backToList.isEnabled = entry != null
        toolbar.title = entry?.title ?: getString(R.string.settings_licenses)
        content.removeAllViews()
        content.addView(page)
        appBar.setLifted(page.scrollY > 0)
    }

    /**
     * Fits [COLUMNS] monospace columns to the screen width, capped at 13sp. Longer lines can
     * still scroll sideways.
     */
    private fun fitColumns(view: TextView) {
        val width = resources.displayMetrics.widthPixels - dp(TEXT_PADDING_DP * 2)
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
        showPage(null, list)
    }

    companion object {
        private const val COLUMNS = 74
        private const val TEXT_PADDING_DP = 12
        private const val STATE_SHOWING = "showing"
    }
}
