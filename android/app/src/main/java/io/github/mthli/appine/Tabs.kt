package io.github.mthli.appine

import android.content.Context
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.IOException

/**
 * Restoring tabs after the app was killed (the android/service map). While the kernel runs a
 * "running" marker is kept, and the tab layout and each tab's login account, current directory and
 * transcript are saved periodically in `files/tabs/`. A normal exit deletes them together with the
 * marker; if the marker is still there the next time the service starts, the system killed the app
 * last time (or it crashed), and the tabs are restored from what was saved: each tab starts a new
 * shell in its original directory, showing the original transcript and a line of notice first.
 * Processes are not restored.
 */
internal object Tabs {
    private const val TAG = "appine"

    /**
     * The most characters saved per transcript (the last part is kept): a 2000-line transcript
     * usually fits.
     */
    private const val MAX_TRANSCRIPT = 256 * 1024

    /**
     * A tab: the login account ([ConsolePty]'s `login`), the foreground process's current directory
     * (null when unavailable), the transcript, and the name the user gave it (null when none).
     */
    data class Tab(val login: String, val cwd: String?, val transcript: String, val name: String?)

    /** The saved tabs, which one is current, and when they were saved (ms). */
    data class Saved(val tabs: List<Tab>, val current: Int, val time: Long)

    private fun dir(context: Context) = File(context.filesDir, "tabs")

    private fun marker(context: Context) = File(dir(context), "running")

    /**
     * Hashes of the file contents already written: unchanged content is not written again (saved
     * every minute, so as not to keep writing flash).
     */
    private val written = HashMap<String, Int>()

    /** The kernel is up: leave the "running" marker. */
    fun markRunning(context: Context) {
        runCatching {
            dir(context).mkdirs()
            marker(context).createNewFile()
        }.onFailure { Log.w(TAG, "cannot mark the tabs as running", it) }
    }

    /** Normal exit: delete the marker and the saved tabs. */
    fun clear(context: Context) {
        synchronized(this) {
            written.clear()
            dir(context).deleteRecursively()
        }
    }

    /**
     * Called once when the service starts: returns the saved tabs when the last run did not exit
     * normally (the marker is still there), otherwise deletes the old ones and returns null. The
     * marker is removed and left again once the kernel is up ([markRunning]).
     */
    fun takeKilled(context: Context): Saved? {
        if (!marker(context).exists()) {
            clear(context)
            return null
        }
        marker(context).delete()
        return runCatching { load(context) }
            .onFailure { Log.w(TAG, "cannot read the saved tabs", it) }
            .getOrNull()
    }

    private fun load(context: Context): Saved? {
        val index = File(dir(context), "index.json").takeIf { it.isFile } ?: return null
        val json = JSONObject(index.readText())
        val list = json.getJSONArray("tabs")
        val tabs = (0 until list.length()).map { i ->
            val t = list.getJSONObject(i)
            val transcript = File(dir(context), t.getString("transcript")).takeIf { it.isFile }?.readText().orEmpty()
            Tab(
                t.getString("login"),
                t.optString("cwd").takeIf { it.isNotEmpty() },
                transcript,
                // Absent in tabs saved before tabs had names
                t.optString("name").takeIf { it.isNotEmpty() },
            )
        }
        return Saved(tabs, json.optInt("current"), json.optLong("time"))
    }

    /**
     * Saves the tabs (called on a background thread): the transcripts first and the index last,
     * each written to a temporary file and then renamed, so a kill midway leaves either the
     * complete previous copy or this one to read (the transcripts may be slightly newer).
     */
    fun save(context: Context, tabs: List<Tab>, current: Int) = synchronized(this) {
        try {
            val dir = dir(context).apply { mkdirs() }
            val entries = JSONArray()
            var changed = false
            tabs.forEachIndexed { i, tab ->
                val name = "t$i.txt"
                changed = writeIfChanged(File(dir, name), tab.transcript.takeLast(MAX_TRANSCRIPT)) || changed
                entries.put(
                    JSONObject().put("login", tab.login).put("cwd", tab.cwd ?: "").put("transcript", name)
                        .apply { tab.name?.let { put("name", it) } },
                )
            }
            // Leftover old transcripts (tabs that were closed)
            dir.listFiles { f -> f.name.matches(Regex("t\\d+\\.txt")) }?.forEach { f ->
                if (f.name.removePrefix("t").removeSuffix(".txt").toInt() >= tabs.size) {
                    f.delete()
                    written.remove(f.name)
                }
            }
            // The index carries the save time: not rewritten when the transcripts, the layout and
            // the current directories are all unchanged
            val layout = (entries.toString() + current).hashCode()
            val index = File(dir, "index.json")
            if (changed || written[index.name] != layout || !index.isFile) {
                val json = JSONObject()
                    .put("time", System.currentTimeMillis())
                    .put("current", current)
                    .put("tabs", entries)
                replace(index, json.toString())
                written[index.name] = layout
            }
        } catch (e: IOException) {
            Log.w(TAG, "cannot save the tabs", e)
        }
    }

    /** Writes only when the content changed; returns whether it wrote. */
    private fun writeIfChanged(file: File, text: String): Boolean {
        val hash = text.hashCode()
        if (written[file.name] == hash && file.isFile) return false
        replace(file, text)
        written[file.name] = hash
        return true
    }

    private fun replace(file: File, text: String) {
        val tmp = File(file.path + ".tmp")
        tmp.writeText(text)
        if (!tmp.renameTo(file)) throw IOException("cannot move ${tmp.name} into place")
    }
}
