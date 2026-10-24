package io.github.mthli.appine

import android.content.Context
import android.util.Log
import java.io.File
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * rootfs 的首次启动流程（设计文档 §11.4）：把 APK assets 里的 Alpine minirootfs 导入
 * inode 仓库，写入配置文件；之后在后台给镜像测速，选最快的一个写进 apk 的仓库列表。
 */
internal object Rootfs {
    private const val TAG = "appine"

    /** 内置的镜像列表，测速失败时用第一个。 */
    private val MIRRORS = listOf(
        "https://dl-cdn.alpinelinux.org/alpine",
        "https://mirrors.tuna.tsinghua.edu.cn/alpine",
        "https://mirrors.ustc.edu.cn/alpine",
        "https://mirrors.aliyun.com/alpine",
    )

    /** 测速时最多下载的字节数和每个镜像的超时。 */
    private const val PROBE_BYTES = 128 * 1024
    private const val PROBE_TIMEOUT_MS = 5000

    fun store(context: Context): File = File(context.filesDir, "rootfs")

    /** 仓库已经导入并配置好了。 */
    fun isReady(context: Context): Boolean = File(store(context), "meta.db").exists()

    /**
     * 第一次运行时导入 rootfs。先导入到临时目录，完成后再改名，导入中途被杀掉时下次
     * 重新来过。
     */
    @Throws(IOException::class)
    fun prepare(context: Context) {
        if (isReady(context)) return
        val tmp = File(context.filesDir, "rootfs.tmp")
        tmp.deleteRecursively()
        val tarball = File(context.cacheDir, "rootfs.tar.gz")
        context.assets.open("rootfs.tgz").use { input ->
            tarball.outputStream().use { input.copyTo(it) }
        }
        try {
            Native.importRootfs(tmp.path, tarball.path)
        } finally {
            tarball.delete()
        }
        configure(tmp.path)
        if (!tmp.renameTo(store(context))) throw IOException("cannot move the rootfs into place")
    }

    private fun write(store: String, path: String, text: String, overwrite: Boolean = true) =
        Native.writeFile(store, path, text.toByteArray(), 420 /* 0644 */, overwrite)

    private fun repositories(mirror: String) =
        "$mirror/${BuildConfig.ALPINE_BRANCH}/main\n$mirror/${BuildConfig.ALPINE_BRANCH}/community\n"

    @Throws(IOException::class)
    private fun configure(store: String) {
        write(store, "/etc/apk/repositories", repositories(MIRRORS[0]))
        write(store, "/etc/hostname", "localhost\n")
        write(store, "/etc/hosts", "127.0.0.1\tlocalhost\n::1\tlocalhost\n", overwrite = false)
        write(
            store, "/etc/profile.d/appine.sh",
            """
            cat /etc/motd 2>/dev/null
            export TERM=xterm-256color
            export LANG=C.UTF-8
            PS1='\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '
            cd "${'$'}HOME" 2>/dev/null

            """.trimIndent(),
        )
        write(
            store, "/etc/motd",
            """
            Welcome to appine: Alpine Linux, running inside an Android app.

            Install packages with apk, for example:  apk add git python3

            """.trimIndent(),
        )
    }

    /**
     * 并发给镜像测速，把最快的写进 `/etc/apk/repositories`；全部失败时保持原样。
     * 会阻塞几秒，在后台线程上调用。
     */
    fun chooseMirror(context: Context) {
        val pool = Executors.newFixedThreadPool(MIRRORS.size)
        val results = MIRRORS.map { mirror -> pool.submit<Long?> { probe(mirror) } }
        val times = MIRRORS.zip(results.map {
            runCatching { it.get(PROBE_TIMEOUT_MS * 2L, TimeUnit.MILLISECONDS) }.getOrNull()
        })
        pool.shutdownNow()
        val best = times.filter { it.second != null }.minByOrNull { it.second!! } ?: return
        Log.i(TAG, "mirror speeds: $times, using ${best.first}")
        runCatching {
            write(store(context).path, "/etc/apk/repositories", repositories(best.first))
        }.onFailure { Log.w(TAG, "cannot write the repositories", it) }
    }

    /** 下载镜像上 APKINDEX 的开头部分，返回用时（毫秒）；失败时返回 null。 */
    private fun probe(mirror: String): Long? = runCatching {
        val url = URL("$mirror/${BuildConfig.ALPINE_BRANCH}/main/aarch64/APKINDEX.tar.gz")
        val start = System.nanoTime()
        val conn = url.openConnection() as HttpURLConnection
        conn.connectTimeout = PROBE_TIMEOUT_MS
        conn.readTimeout = PROBE_TIMEOUT_MS
        conn.setRequestProperty("Range", "bytes=0-${PROBE_BYTES - 1}")
        try {
            if (conn.responseCode !in 200..299) return@runCatching null
            val buf = ByteArray(16 * 1024)
            var total = 0
            conn.inputStream.use { input ->
                while (total < PROBE_BYTES) {
                    val n = input.read(buf)
                    if (n < 0) break
                    total += n
                }
            }
            (System.nanoTime() - start) / 1_000_000
        } finally {
            conn.disconnect()
        }
    }.getOrNull()
}
