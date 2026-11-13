package io.github.mthli.appine

import android.app.Activity
import android.content.ActivityNotFoundException
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import androidx.browser.customtabs.CustomTabsIntent
import androidx.core.net.toUri
import kotlin.concurrent.thread

/** Opens terminal links for one activity, sharing its toast queue with the bars' notices. */
internal class TerminalLinks(
    private val activity: Activity,
    private val showToast: (String, Boolean) -> Unit,
) {
    /**
     * Opens web pages with Custom Tabs, other schemes as browsable intents, and guest files
     * through the Files app's provider ([openFile]).
     */
    fun open(uri: String) {
        val link = uri.toUri()
        if (link.scheme.equals("file", ignoreCase = true)) {
            openFile(link)
        } else if (link.scheme.equals("http", ignoreCase = true) ||
            link.scheme.equals("https", ignoreCase = true)
        ) {
            val tab = CustomTabsIntent.Builder().build()
            startViewer(
                tab.intent.setData(link).addCategory(Intent.CATEGORY_BROWSABLE),
                tab.startAnimationBundle,
            )
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
            val found = Documents.fileLink(activity, link)
            activity.runOnUiThread {
                if (activity.isDestroyed) return@runOnUiThread
                when (found) {
                    is Documents.FileLink.Found -> startViewer(
                        Intent(Intent.ACTION_VIEW).setDataAndType(found.uri, found.mime)
                            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION),
                    )
                    is Documents.FileLink.Remote -> showToast(activity.getString(R.string.link_remote_file, found.host), false)
                    Documents.FileLink.Hidden -> showToast(activity.getString(R.string.link_hidden_file), true)
                    Documents.FileLink.Missing -> showToast(activity.getString(R.string.link_missing_file), false)
                }
            }
        }
    }

    private fun startViewer(intent: Intent, options: Bundle? = null) {
        try {
            activity.startActivity(intent, options)
        } catch (e: ActivityNotFoundException) {
            showToast(activity.getString(R.string.link_no_app), false)
        }
    }
}
