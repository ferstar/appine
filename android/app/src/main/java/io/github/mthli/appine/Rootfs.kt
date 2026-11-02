package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.util.Log
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/**
 * rootfs 的导入和配置（设计文档 §11.4）：第一次运行时导入 APK 里的 Alpine，设置里可以换成
 * Debian（选了才下载，版本和 sha256 固定写在这里，与 `scripts/fetch-rootfs` 一样只认摘要对得上
 * 的 tarball）。导入之后写入配置文件，在后台给镜像测速，选最快的一个写进包管理器的源；设置里
 * 也可以手动选。
 */
internal object Rootfs {
    private const val TAG = "appine"

    /** 要下载的 tarball：`urls` 是同一个文件的几个来源，摘要固定，从哪个下载都一样可信。 */
    data class Download(val urls: List<String>, val sha256: String, val size: Long)

    /**
     * 可以选的发行版。`mirrors` 是内置的镜像列表（测速失败时用第一个，官方的），`probe` 是测速时
     * 下载开头一部分的文件（相对镜像）。
     */
    enum class Distro(
        val label: String,
        val download: Download?,
        val mirrors: List<String>,
        val probe: String,
    ) {
        ALPINE(
            "Alpine Linux ${BuildConfig.ALPINE_BRANCH.removePrefix("v")}",
            null,
            listOf(
                "https://dl-cdn.alpinelinux.org/alpine",
                "https://mirrors.tuna.tsinghua.edu.cn/alpine",
                "https://mirrors.ustc.edu.cn/alpine",
                "https://mirrors.aliyun.com/alpine",
            ),
            "${BuildConfig.ALPINE_BRANCH}/main/aarch64/APKINDEX.tar.gz",
        ),

        /**
         * debuerreotype 构建的 `debian:trixie`（Docker 官方镜像用的同一份），固定在一个提交上。只有
         * GitHub 这一个来源，国内直连很慢；没有镜像站带这份构建，暂不自己托管。
         */
        DEBIAN(
            "Debian 13",
            Download(
                listOf(
                    "https://raw.githubusercontent.com/debuerreotype/docker-debian-artifacts/" +
                        "e36653f9967231373aeba7cff2102fd004ac4f4a/trixie/oci/blobs/rootfs.tar.gz",
                ),
                "dfb68c024d5cdf4872bb7f0be8d63874aa6d0b7a0050f1b06169573e34ea543a",
                49_776_179,
            ),
            listOf(
                "deb.debian.org",
                "mirrors.tuna.tsinghua.edu.cn",
                "mirrors.ustc.edu.cn",
                "mirrors.aliyun.com",
            ),
            "debian/dists/trixie/InRelease",
        ),
    }

    /** 镜像的主机名，设置里显示用。 */
    fun host(mirror: String): String = mirror.substringAfter("://").substringBefore('/')

    /** 测速时最多下载的字节数和时间。 */
    private const val PROBE_BYTES = 512 * 1024
    private const val PROBE_TIMEOUT_MS = 5000

    /** 下载的超时：连接和两次读之间。 */
    private const val DOWNLOAD_TIMEOUT_MS = 30_000

    /** 一个来源上下载中断之后最多接着下几次。 */
    private const val RESUMES = 5

    /** 记录（SharedPreferences）：装的发行版，写在源里的镜像，镜像是不是用户在设置里选的。 */
    private const val KEY_DISTRO = "distro"
    private const val KEY_MIRROR = "mirror"
    private const val KEY_MIRROR_MANUAL = "mirror_manual"

    fun store(context: Context): File = File(context.filesDir, "rootfs")

    /** 仓库已经导入并配置好了。 */
    fun isReady(context: Context): Boolean = isReady(store(context))

    private fun isReady(store: File) = File(store, "meta.db").exists()

    private fun prefs(context: Context) = context.getSharedPreferences("rootfs", Context.MODE_PRIVATE)

    /**
     * App 的仓库里装的发行版，还没有仓库时是 null。导入时记下；第 190 轮之前导入的仓库没有记录，
     * 当作 Alpine（App 还没有发布过，只有开发用的安装上有这样的仓库）。
     */
    fun installed(context: Context): Distro? {
        if (!isReady(context)) return null
        val name = prefs(context).getString(KEY_DISTRO, null) ?: return Distro.ALPINE
        return Distro.entries.firstOrNull { it.name == name } ?: Distro.ALPINE
    }

    /** 中止下载（[download] 的 `cancelled` 返回 true）时抛出。 */
    class Cancelled : IOException("cancelled")

    private fun tarball(context: Context, distro: Distro) = File(context.cacheDir, "${distro.name.lowercase()}.tar.gz")

    /**
     * 把 [distro] 要下载的 tarball 下载到缓存目录，缓存里已经有校验过的就不下。下载时 [progress]
     * 收到已经下载和总共的字节数；[cancelled] 返回 true 时中止，抛出 [Cancelled]。会阻塞，在后台
     * 线程上调用。
     */
    @Throws(IOException::class)
    fun download(
        context: Context,
        distro: Distro,
        progress: (Long, Long) -> Unit = { _, _ -> },
        cancelled: () -> Boolean = { false },
    ) {
        val download = distro.download ?: return
        val tarball = tarball(context, distro)
        if (tarball.length() == download.size && sha256(tarball) == download.sha256) return
        fetch(download, tarball, cancelled, progress)
    }

    /**
     * 把 [distro] 导入仓库 [store]（默认是 App 用的那个）。先导入到临时目录，完成后再改名，
     * 中途被杀掉时下次重新来过。要下载的 tarball 校验过之后留在缓存目录里（[download]），导入
     * 成功才删掉，导入失败重来时不必再下载。会阻塞，在后台线程上调用。
     */
    @SuppressLint("ApplySharedPref") // 记录要在仓库改名到位之前写到盘上
    @Throws(IOException::class)
    fun prepare(
        context: Context,
        distro: Distro,
        progress: (Long, Long) -> Unit = { _, _ -> },
        store: File = store(context),
    ) {
        if (isReady(store)) return
        val tmp = File(store.parentFile, "${store.name}.tmp")
        tmp.deleteRecursively()
        val tarball = tarball(context, distro)
        if (distro.download == null) {
            context.assets.open("rootfs.tgz").use { input ->
                tarball.outputStream().use { input.copyTo(it) }
            }
        } else {
            download(context, distro, progress)
        }
        Native.importRootfs(tmp.path, tarball.path)
        tarball.delete()
        configure(tmp.path, distro)
        if (store == store(context)) {
            // 改名之前写到盘上：仓库在了，记录就是它的
            prefs(context).edit()
                .putString(KEY_DISTRO, distro.name)
                .putString(KEY_MIRROR, distro.mirrors[0])
                .putBoolean(KEY_MIRROR_MANUAL, false)
                .commit()
        }
        if (!tmp.renameTo(store)) throw IOException("cannot move the rootfs into place")
    }

    /** 删掉 App 的仓库（换发行版、重置 rootfs）。内核要先停下。 */
    fun reset(context: Context) {
        File(context.filesDir, "rootfs.tmp").deleteRecursively()
        if (!store(context).deleteRecursively()) Log.w(TAG, "cannot delete all of the rootfs")
    }

    /**
     * 下载 [download] 到 [dest]：有几个来源时先测速，从快的开始试，一个失败了换下一个。
     * 边写边算 sha256，对不上（或者长度不对）时删掉，抛出异常；先写到 `.part`，校验过了才改名。
     * [cancelled] 返回 true 时不再试别的来源，抛出 [Cancelled]。
     */
    @Throws(IOException::class)
    fun fetch(
        download: Download,
        dest: File,
        cancelled: () -> Boolean = { false },
        progress: (Long, Long) -> Unit,
    ) {
        var error: IOException? = null
        for (url in fastest(download.urls)) {
            if (cancelled()) throw Cancelled()
            try {
                Log.i(TAG, "downloading $url")
                return fetch(url, download, dest, progress, cancelled)
            } catch (e: Cancelled) {
                throw e
            } catch (e: IOException) {
                Log.w(TAG, "cannot download $url", e)
                error = e
            }
        }
        throw error ?: IOException("nowhere to download from")
    }

    /**
     * 从 [url] 下载。中途超时、连接断开时带 `Range` 从断开的地方接着下（这一次下到了数据才
     * 算，最多 [RESUMES] 次），服务器不支持断点续传时从头再来。
     */
    @Throws(IOException::class)
    private fun fetch(
        url: String,
        download: Download,
        dest: File,
        progress: (Long, Long) -> Unit,
        cancelled: () -> Boolean,
    ) {
        val part = File(dest.path + ".part")
        var digest = MessageDigest.getInstance("SHA-256")
        var done = 0L
        var reported = 0L
        var resumes = 0
        try {
            while (true) {
                val conn = URL(url).openConnection()
                conn.connectTimeout = DOWNLOAD_TIMEOUT_MS
                conn.readTimeout = DOWNLOAD_TIMEOUT_MS
                val http = conn as? HttpURLConnection
                if (done > 0) conn.setRequestProperty("Range", "bytes=$done-")
                val before = done
                try {
                    if (done > 0 && http?.responseCode != HttpURLConnection.HTTP_PARTIAL) {
                        done = 0
                        reported = 0
                        digest = MessageDigest.getInstance("SHA-256")
                    }
                    conn.getInputStream().use { input ->
                        FileOutputStream(part, done > 0).use { output ->
                            val buf = ByteArray(64 * 1024)
                            while (true) {
                                if (cancelled()) throw Cancelled()
                                val n = input.read(buf)
                                if (n < 0) break
                                digest.update(buf, 0, n)
                                output.write(buf, 0, n)
                                done += n
                                // 进度最多报告约两百次
                                if (done - reported >= download.size / 200 || done == download.size) {
                                    reported = done
                                    progress(done, download.size)
                                }
                            }
                        }
                    }
                    break
                } catch (e: IOException) {
                    if (e is Cancelled || http == null || done == before || ++resumes > RESUMES) throw e
                    Log.w(TAG, "download of $url broke off at $done bytes, resuming", e)
                } finally {
                    http?.disconnect()
                }
            }
            val got = hex(digest.digest())
            if (done != download.size || got != download.sha256) {
                throw IOException("$url: got $done bytes with sha256 $got")
            }
            if (!part.renameTo(dest)) throw IOException("cannot move the download into place")
        } finally {
            part.delete()
        }
    }

    private fun sha256(file: File): String {
        val digest = MessageDigest.getInstance("SHA-256")
        file.inputStream().use { input ->
            val buf = ByteArray(64 * 1024)
            while (true) {
                val n = input.read(buf)
                if (n < 0) break
                digest.update(buf, 0, n)
            }
        }
        return hex(digest.digest())
    }

    private fun hex(bytes: ByteArray) = bytes.joinToString("") { "%02x".format(it) }

    private fun write(store: String, path: String, text: String, overwrite: Boolean = true) =
        Native.writeFile(store, path, text.toByteArray(), 420 /* 0644 */, overwrite)

    /**
     * 只有回环接口的 `/etc/network/interfaces`（每次启动内核之前，没有时才写，用户改过的不动），
     * 与 setup-alpine 写的一样。网络是宿主的（设计文档 §10），要网络的服务（nginx 等）由 OpenRC
     * 拉起 networking，没有这个文件时它失败，服务也起不来。不用 rc.conf 里的 `rc_provide`：它
     * 加到每一个服务上，再提供 `clock` 就成了环。没装 OpenRC、ifupdown 时这个文件没人读。
     */
    /**
     * 每次开机之前在 guest 里运行（`sh -c`）：装了 OpenRC、hwclock/osclock/swclock 都不在任何运行
     * 级别里时，把 osclock 加进 boot。busybox 的 syslog `need clock`，OpenRC 默认用 hwclock 提供
     * clock，它读不到 RTC、也设不了宿主的时间，开机日志里总有一个失败；osclock 表示"时间由操作
     * 系统管"，什么也不做（设计文档 §6）。用户自己放了哪个时钟服务就不动。
     */
    const val PREFER_OSCLOCK = "[ -x /sbin/rc-update ] && [ -e /etc/init.d/osclock ] || exit 0; " +
        "for s in hwclock osclock swclock; do for l in /etc/runlevels/*/\$s; do " +
        "[ -e \"\$l\" ] || [ -L \"\$l\" ] && exit 0; done; done; " +
        "exec /sbin/rc-update -q add osclock boot >/dev/null 2>&1"

    @Throws(IOException::class)
    fun ensureNetworkConfig(context: Context) = write(
        store(context).path, "/etc/network/interfaces",
        "auto lo\niface lo inet loopback\n",
        overwrite = false,
    )

    /**
     * Alpine 上把 App 带着的 doas 包放进仓库（`/var/cache/appine/doas.apk`）：普通用户第一次成为
     * 登录账号时登录脚本离线装它（设计文档 §11.1）。每次启动内核之前写，App 升级之后换成新的。
     */
    @Throws(IOException::class)
    fun ensureLoginTools(context: Context) {
        if (installed(context) != Distro.ALPINE) return
        val data = context.assets.open("doas.apk").use { it.readBytes() }
        Native.writeFile(store(context).path, "/var/cache/appine/doas.apk", data, 420 /* 0644 */, true)
    }

    /**
     * 这次开机时没有起来的服务：OpenRC 在 `/var/log/boot.log` 里写 ` * ERROR: <服务> failed to
     * start`（内核每次开机清空这个文件，设计文档 §6）。没有 OpenRC、读不到时是空的。
     */
    fun failedServices(context: Context): List<String> {
        val log = runCatching { Native.readFile(store(context).path, "/var/log/boot.log") }.getOrNull()
            ?: return emptyList()
        return Regex("""ERROR: (\S+) failed to start""").findAll(String(log)).map { it.groupValues[1] }.distinct().toList()
    }

    /** `/etc/passwd` 里的一个账号。 */
    data class Account(val name: String, val uid: Int)

    /**
     * 能作为登录账号的普通用户：UID 1000–65533、有登录 shell（不是 nologin、false），按 UID 排，
     * 第一个就是"自动"登录的那个。规则与 assets 的 `login.sh` 一样。读不到时是空的。
     */
    fun regularAccounts(context: Context): List<Account> {
        val passwd = runCatching { Native.readFile(store(context).path, "/etc/passwd") }.getOrNull()
            ?: return emptyList()
        return String(passwd).lines().mapNotNull { line ->
            val f = line.split(':')
            if (f.size < 7) return@mapNotNull null
            val shell = f[6]
            val uid = f[2].toIntOrNull() ?: return@mapNotNull null
            if (shell.isEmpty() || shell.endsWith("/nologin") || shell.endsWith("/false")) return@mapNotNull null
            if (uid !in 1000..65533) return@mapNotNull null
            Account(f[0], uid)
        }.sortedBy { it.uid }
    }

    /**
     * `/etc/profile.d/appine.sh`：显示 motd、终端和语言的环境变量、彩色的提示符。从哪个目录开始
     * 由 assets 的 `login.sh` 决定（App 被杀之后恢复的标签页从原来的目录开始，设计文档 §11.2）。
     */
    private val PROFILE = """
        cat /etc/motd 2>/dev/null
        export TERM=xterm-256color
        export LANG=C.UTF-8
        export NMAP_UNPRIVILEGED=1
        PS1='\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '

    """.trimIndent()

    /** 第 194 轮之前写的那份：最后还有一行 `cd "$HOME"`，恢复的标签页会被它带回家目录。 */
    private val OLD_PROFILE = PROFILE.trimEnd('\n') + "\ncd \"${'$'}HOME\" 2>/dev/null\n"

    /** 之前导入的仓库里 App 写的 profile 没被改过的，换成现在的。 */
    @Throws(IOException::class)
    fun updateProfile(context: Context) {
        val path = "/etc/profile.d/appine.sh"
        val old = runCatching { Native.readFile(store(context).path, path) }.getOrNull() ?: return
        if (String(old) == OLD_PROFILE) write(store(context).path, path, PROFILE)
    }

    /** 包管理器的源：写在哪个文件里，内容是什么。 */
    private fun sources(distro: Distro, mirror: String): Pair<String, String> = when (distro) {
        Distro.ALPINE -> "/etc/apk/repositories" to
            "$mirror/${BuildConfig.ALPINE_BRANCH}/main\n$mirror/${BuildConfig.ALPINE_BRANCH}/community\n"
        // rootfs 里没有 ca-certificates，apt 用 http（软件包有签名）
        Distro.DEBIAN -> "/etc/apt/sources.list.d/debian.sources" to
            """
            Types: deb
            URIs: http://$mirror/debian
            Suites: trixie trixie-updates
            Components: main
            Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp

            Types: deb
            URIs: http://$mirror/debian-security
            Suites: trixie-security
            Components: main
            Signed-By: /usr/share/keyrings/debian-archive-keyring.pgp

            """.trimIndent()
    }

    @Throws(IOException::class)
    private fun configure(store: String, distro: Distro) {
        val (sources, content) = sources(distro, distro.mirrors[0])
        write(store, sources, content)
        write(store, "/etc/hostname", "localhost\n")
        if (distro == Distro.ALPINE) {
            write(store, "/etc/hosts", "127.0.0.1\tlocalhost\n::1\tlocalhost\n", overwrite = false)
        } else {
            // Debian 的没有 hosts，resolv.conf 指向 1.1.1.1。DNS 要走内核层的存根（设计文档
            // §10），系统的私人 DNS、VPN 才生效
            write(store, "/etc/hosts", "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n")
            write(store, "/etc/resolv.conf", "nameserver 127.0.0.53\n")
        }
        write(store, "/etc/profile.d/appine.sh", PROFILE)
        val install = when (distro) {
            Distro.ALPINE -> "apk add git python3"
            Distro.DEBIAN -> "apt update && apt install git python3"
        }
        write(
            store, "/etc/motd",
            """
            Welcome to appine: ${distro.label}, running inside an Android app.

            Install packages with ${install.substringBefore(' ')}, for example:  $install

            """.trimIndent(),
        )
    }

    /** 写在包管理器的源里的镜像（[Distro.mirrors] 的一项）；还没有仓库时是 null。 */
    fun mirror(context: Context): String? = if (isReady(context)) prefs(context).getString(KEY_MIRROR, null) else null

    /** 镜像是用户在设置里选的（不是测速选的）。 */
    fun isMirrorManual(context: Context): Boolean = prefs(context).getBoolean(KEY_MIRROR_MANUAL, false)

    /**
     * 设置里选了 [mirror]：写进 [distro] 的包管理器的源（用户自己改过的也覆盖），之后不再测速
     * 换掉它。内核运行着也可以写：仓库的提交计数器变了，内核层的缓存随之作废（设计文档 §8.1）。
     */
    @Throws(IOException::class)
    fun setMirror(context: Context, distro: Distro, mirror: String) = synchronized(this) {
        val (path, content) = sources(distro, mirror)
        write(store(context).path, path, content)
        prefs(context).edit().putString(KEY_MIRROR, mirror).putBoolean(KEY_MIRROR_MANUAL, true).apply()
    }

    /**
     * 并发给 [distro] 的镜像测速，把最快的写进包管理器的源；全部失败时保持原样。用户在设置里
     * 选了镜像（测速期间也算）就不动。会阻塞几秒，在后台线程上调用。Debian 的镜像只写主机名，
     * 测速用 https。
     */
    fun chooseMirror(context: Context, distro: Distro) {
        prefs(context).edit().putBoolean(KEY_MIRROR_MANUAL, false).apply()
        val base = { mirror: String -> if (distro == Distro.ALPINE) mirror else "https://$mirror" }
        val speeds = probeAll(distro.mirrors.map { "${base(it)}/${distro.probe}" })
        val best = distro.mirrors.zip(speeds).filter { it.second != null }.maxByOrNull { it.second!! } ?: return
        Log.i(TAG, "mirror speeds: ${distro.mirrors.zip(speeds)}, using ${best.first}")
        synchronized(this) {
            if (isMirrorManual(context)) return
            runCatching {
                val (path, content) = sources(distro, best.first)
                write(store(context).path, path, content)
                prefs(context).edit().putString(KEY_MIRROR, best.first).apply()
            }.onFailure { Log.w(TAG, "cannot write the package sources", it) }
        }
    }

    /** [urls] 按测出的速度从快到慢排好，测不了的按原来的顺序排在后面。只有一个时不测。 */
    private fun fastest(urls: List<String>): List<String> {
        if (urls.size < 2) return urls
        val speeds = probeAll(urls)
        Log.i(TAG, "download speeds: ${urls.zip(speeds)}")
        return urls.indices.sortedByDescending { speeds[it] ?: -1.0 }.map { urls[it] }
    }

    /** 并发给 [urls] 测速，返回各自的速度（字节每毫秒），失败的是 null。会阻塞几秒。 */
    private fun probeAll(urls: List<String>): List<Double?> {
        val pool = Executors.newFixedThreadPool(urls.size)
        val results = urls.map { url -> pool.submit<Double?> { probe(url) } }
        val speeds = results.map {
            runCatching { it.get(PROBE_TIMEOUT_MS * 2L, TimeUnit.MILLISECONDS) }.getOrNull()
        }
        pool.shutdownNow()
        return speeds
    }

    /**
     * 下载 [address] 的开头部分，返回速度（字节每毫秒，含建立连接）；一个字节也没下到时返回
     * null。按吞吐量而不是下完一小段的用时比较：有的镜像开头一小段很快，之后被限速。
     */
    private fun probe(address: String): Double? = runCatching {
        val url = URL(address)
        val start = System.nanoTime()
        val elapsed = { (System.nanoTime() - start) / 1_000_000 }
        val conn = url.openConnection() as HttpURLConnection
        conn.connectTimeout = PROBE_TIMEOUT_MS
        conn.readTimeout = PROBE_TIMEOUT_MS
        conn.setRequestProperty("Range", "bytes=0-${PROBE_BYTES - 1}")
        try {
            if (conn.responseCode !in 200..299) return@runCatching null
            val buf = ByteArray(16 * 1024)
            var total = 0
            conn.inputStream.use { input ->
                while (total < PROBE_BYTES && elapsed() < PROBE_TIMEOUT_MS) {
                    val n = input.read(buf)
                    if (n < 0) break
                    total += n
                }
            }
            if (total == 0) null else total.toDouble() / elapsed().coerceAtLeast(1)
        } finally {
            conn.disconnect()
        }
    }.getOrNull()
}
