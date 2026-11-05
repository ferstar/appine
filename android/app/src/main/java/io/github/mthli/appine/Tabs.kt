package io.github.mthli.appine

import android.content.Context
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.IOException
import java.security.MessageDigest

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
     * (null when unavailable) and the transcript.
     */
    data class Tab(val login: String, val cwd: String?, val transcript: String)

    /** The saved tabs, which one is current, and when they were saved (ms). */
    data class Saved(val tabs: List<Tab>, val current: Int, val time: Long)

    private fun dir(context: Context) = File(context.filesDir, "tabs")

    private fun marker(context: Context) = File(dir(context), "running")

    /**
     * Hashes of the index contents already written: an unchanged index is not written again (saved
     * every minute, so as not to keep writing flash; transcripts are skipped by their names).
     */
    private val written = HashMap<String, Int>()

    /**
     * Called once when the service starts: returns the saved tabs when the last run did not exit
     * normally (the marker is still there), otherwise deletes the old ones and returns null. The
     * marker and the tabs stay until they are replaced by the restored tabs' own saves or cleared
     * by a normal exit: killed again before the kernel is up, the next start still finds them.
     */
    fun takeKilled(context: Context): Saved? {
        if (!marker(context).exists()) {
            clear(context)
            return null
        }
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
            // A "name" key, from versions that could rename tabs, is ignored
            Tab(t.getString("login"), t.optString("cwd").takeIf { it.isNotEmpty() }, transcript)
        }
        return Saved(tabs, json.optInt("current"), json.optLong("time"))
    }

    /** The kernel is up: leave the "running" marker. */
    fun markRunning(context: Context) {
        runCatching {
            dir(context).mkdirs()
            marker(context).createNewFile()
        }.onFailure { Log.w(TAG, "cannot mark the tabs as running", it) }
    }

    /**
     * Saves the tabs (called on a background thread). Each transcript is a file named by its
     * content, written to a temporary file and then renamed; the index naming them is replaced
     * last, and only then are transcripts no index names any more deleted. A kill at any point
     * leaves a complete index whose transcripts all exist and belong to its own tabs.
     */
    fun save(context: Context, tabs: List<Tab>, current: Int) = synchronized(this) {
        try {
            val dir = dir(context).apply { mkdirs() }
            val entries = JSONArray()
            val names = HashSet<String>()
            tabs.forEach { tab ->
                val text = tab.transcript.takeLast(MAX_TRANSCRIPT)
                val name = transcriptName(text)
                // Unchanged since the last save: the file with this content is already there
                if (name !in names && !File(dir, name).isFile) replace(File(dir, name), text)
                names.add(name)
                entries.put(JSONObject().put("login", tab.login).put("cwd", tab.cwd ?: "").put("transcript", name))
            }
            // The index carries the save time: not rewritten when the transcripts, the layout and
            // the current directories are all unchanged
            val layout = (entries.toString() + current).hashCode()
            val index = File(dir, "index.json")
            if (written[index.name] != layout || !index.isFile) {
                val json = JSONObject()
                    .put("time", System.currentTimeMillis())
                    .put("current", current)
                    .put("tabs", entries)
                replace(index, json.toString())
                written[index.name] = layout
            }
            // Transcripts of closed tabs and old contents, and leftovers of a save that was killed
            // (including the t<i>.txt names of earlier versions)
            dir.listFiles { f -> f.name.matches(TRANSCRIPT) && f.name !in names }?.forEach { it.delete() }
        } catch (e: IOException) {
            Log.w(TAG, "cannot save the tabs", e)
        }
    }

    /** Transcript files (current and earlier names) and their temporary files. */
    private val TRANSCRIPT = Regex("t[-0-9a-f]+\\.txt(\\.tmp)?")

    /** The file name of a transcript: a digest of [text], so equal content keeps its file. */
    private fun transcriptName(text: String): String {
        val digest = MessageDigest.getInstance("SHA-256").digest(text.toByteArray())
        return "t-" + digest.take(16).joinToString("") { "%02x".format(it) } + ".txt"
    }

    private fun replace(file: File, text: String) {
        val tmp = File(file.path + ".tmp")
        tmp.writeText(text)
        if (!tmp.renameTo(file)) throw IOException("cannot move ${tmp.name} into place")
    }

    /** Normal exit: delete the marker and the saved tabs. */
    fun clear(context: Context) {
        synchronized(this) {
            written.clear()
            dir(context).deleteRecursively()
        }
    }
}
