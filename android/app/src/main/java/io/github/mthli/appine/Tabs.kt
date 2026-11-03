package io.github.mthli.appine

import android.content.Context
import android.util.Log
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.IOException

/**
 * App 被杀之后恢复标签页（android/service 的 map）。内核运行期间留着"在运行"的标记，标签页的布局、
 * 每个标签页的登录账号、当前目录和回看记录定时存在 `files/tabs/`。正常退出时连同标记一起删掉；
 * 下次服务启动时标记还在，就是上次被系统杀掉了（或者崩溃了），按存下的恢复：每个标签页起新的
 * shell，从原来的目录开始，先显示原来的回看记录和一行提示。不恢复进程。
 */
internal object Tabs {
    private const val TAG = "appine"

    /** 一个回看记录最多存这么多字符（取最后的部分）：2000 行的回看记录一般在这以内。 */
    private const val MAX_TRANSCRIPT = 256 * 1024

    /** 一个标签页：登录账号（[ConsolePty] 的 `login`）、前台进程的当前目录（取不到时是 null）、回看记录。 */
    data class Tab(val login: String, val cwd: String?, val transcript: String)

    /** 存下的标签页、当前是第几个、存的时刻（毫秒）。 */
    data class Saved(val tabs: List<Tab>, val current: Int, val time: Long)

    private fun dir(context: Context) = File(context.filesDir, "tabs")

    private fun marker(context: Context) = File(dir(context), "running")

    /** 已经写过的文件内容的散列：内容没变就不再写（每分钟存一次，不要一直写闪存）。 */
    private val written = HashMap<String, Int>()

    /** 内核起来了：留下"在运行"的标记。 */
    fun markRunning(context: Context) {
        runCatching {
            dir(context).mkdirs()
            marker(context).createNewFile()
        }.onFailure { Log.w(TAG, "cannot mark the tabs as running", it) }
    }

    /** 正常退出：删掉标记和存下的标签页。 */
    fun clear(context: Context) {
        synchronized(this) {
            written.clear()
            dir(context).deleteRecursively()
        }
    }

    /**
     * 服务启动时调用一次：上次没有正常退出（标记还在）时返回存下的标签页，不然删掉旧的、返回
     * null。标记去掉，内核起来之后再留（[markRunning]）。
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
            Tab(t.getString("login"), t.optString("cwd").takeIf { it.isNotEmpty() }, transcript)
        }
        return Saved(tabs, json.optInt("current"), json.optLong("time"))
    }

    /**
     * 存下标签页（在后台线程上调用）：先写各个回看记录，最后写索引，都是写临时文件再改名，
     * 中途被杀时读到的是完整的上一份或者这一份（回看记录可能新一点）。
     */
    fun save(context: Context, tabs: List<Tab>, current: Int) = synchronized(this) {
        try {
            val dir = dir(context).apply { mkdirs() }
            val entries = JSONArray()
            var changed = false
            tabs.forEachIndexed { i, tab ->
                val name = "t$i.txt"
                changed = writeIfChanged(File(dir, name), tab.transcript.takeLast(MAX_TRANSCRIPT)) || changed
                entries.put(JSONObject().put("login", tab.login).put("cwd", tab.cwd ?: "").put("transcript", name))
            }
            // 多出来的旧回看记录（标签页关掉了）
            dir.listFiles { f -> f.name.matches(Regex("t\\d+\\.txt")) }?.forEach { f ->
                if (f.name.removePrefix("t").removeSuffix(".txt").toInt() >= tabs.size) {
                    f.delete()
                    written.remove(f.name)
                }
            }
            // 索引带着存的时刻：回看记录、布局、当前目录都没变时不重写
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

    /** 内容变了才写；返回写了没有。 */
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
