package io.github.mthli.appine

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
 * rootfs 的首次启动流程（设计文档 §11.4）：把用户选的发行版导入 inode 仓库，写入配置文件；
 * 之后在后台给镜像测速，选最快的一个写进包管理器的源。Alpine 的 minirootfs 在 APK 的
 * assets 里；Debian、Ubuntu 的选了才下载，版本和 sha256 固定写在这里，与
 * `scripts/fetch-rootfs` 一样只认摘要对得上的 tarball。
 */
internal object Rootfs {
    private const val TAG = "appine"

    /** 要下载的 tarball：`urls` 是同一个文件的几个来源，摘要固定，从哪个下载都一样可信。 */
    data class Download(val urls: List<String>, val sha256: String, val size: Long)

    /**
     * 可以选的发行版。`mirrors` 是内置的镜像列表（测速失败时用第一个），`probe` 是测速时
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

        /** debuerreotype 构建的 `debian:trixie`（Docker 官方镜像用的同一份），固定在一个提交上。 */
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

        /** Ubuntu 官方的 ubuntu-base，清华、中科大的镜像上也有同一份。 */
        UBUNTU(
            "Ubuntu 24.04",
            Download(
                listOf(
                    "cdimage.ubuntu.com",
                    "mirrors.tuna.tsinghua.edu.cn/ubuntu-cdimage",
                    "mirrors.ustc.edu.cn/ubuntu-cdimage",
                ).map { "https://$it/ubuntu-base/releases/24.04/release/ubuntu-base-24.04.5-base-arm64.tar.gz" },
                "a91d5a93010193712d346d761372b7c9db6dfcf093893161c64ca107f05914f2",
                29_936_675,
            ),
            listOf(
                "ports.ubuntu.com",
                "mirrors.tuna.tsinghua.edu.cn",
                "mirrors.ustc.edu.cn",
                "mirrors.aliyun.com",
            ),
            "ubuntu-ports/dists/noble/InRelease",
        ),
    }

    /** 测速时最多下载的字节数和时间。 */
    private const val PROBE_BYTES = 512 * 1024
    private const val PROBE_TIMEOUT_MS = 5000

    /** 下载的超时：连接和两次读之间。 */
    private const val DOWNLOAD_TIMEOUT_MS = 30_000

    /** 一个来源上下载中断之后最多接着下几次。 */
    private const val RESUMES = 5

    fun store(context: Context): File = File(context.filesDir, "rootfs")

    /** 仓库已经导入并配置好了。 */
    fun isReady(context: Context): Boolean = isReady(store(context))

    private fun isReady(store: File) = File(store, "meta.db").exists()

    /**
     * 把 [distro] 导入仓库 [store]（默认是 App 用的那个）。先导入到临时目录，完成后再改名，
     * 中途被杀掉时下次重新来过。要下载的 tarball 校验过之后留在缓存目录里，导入成功才删掉，
     * 导入失败重来时不必再下载。下载时 [progress] 收到已经下载和总共的字节数。会阻塞，在
     * 后台线程上调用。
     */
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
        val download = distro.download
        val tarball = File(context.cacheDir, "${distro.name.lowercase()}.tar.gz")
        if (download == null) {
            context.assets.open("rootfs.tgz").use { input ->
                tarball.outputStream().use { input.copyTo(it) }
            }
        } else if (!(tarball.length() == download.size && sha256(tarball) == download.sha256)) {
            fetch(download, tarball, progress)
        }
        Native.importRootfs(tmp.path, tarball.path)
        tarball.delete()
        configure(tmp.path, distro)
        if (!tmp.renameTo(store)) throw IOException("cannot move the rootfs into place")
    }

    /** 删掉 App 的仓库（重置 rootfs）。内核要先停下。 */
    fun reset(context: Context) {
        File(context.filesDir, "rootfs.tmp").deleteRecursively()
        if (!store(context).deleteRecursively()) Log.w(TAG, "cannot delete all of the rootfs")
    }

    /**
     * 下载 [download] 到 [dest]：有几个来源时先测速，从快的开始试，一个失败了换下一个。
     * 边写边算 sha256，对不上（或者长度不对）时删掉，抛出异常；先写到 `.part`，校验过了才改名。
     */
    @Throws(IOException::class)
    fun fetch(download: Download, dest: File, progress: (Long, Long) -> Unit) {
        var error: IOException? = null
        for (url in fastest(download.urls)) {
            try {
                Log.i(TAG, "downloading $url")
                return fetch(url, download, dest, progress)
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
    private fun fetch(url: String, download: Download, dest: File, progress: (Long, Long) -> Unit) {
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
                    if (http == null || done == before || ++resumes > RESUMES) throw e
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
     * OpenRC 的配置（每次启动内核之前，没有时才写，用户改过的不动）：网络是宿主的，总是通的
     * （设计文档 §10），由 appine 提供 `net`、`loopback`。不然要网络的服务（nginx 等）去拉起
     * networking，没有 `/etc/network/interfaces` 时它失败，服务也起不来。没装 OpenRC 时这个
     * 文件没人读。
     */
    @Throws(IOException::class)
    fun ensureOpenRcConfig(context: Context) = write(
        store(context).path, "/etc/rc.conf.d/appine.conf",
        "# appine: the network is the host's and always up\nrc_provide=\"loopback net\"\n",
        overwrite = false,
    )

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
        Distro.UBUNTU -> "/etc/apt/sources.list.d/ubuntu.sources" to
            """
            Types: deb
            URIs: http://$mirror/ubuntu-ports/
            Suites: noble noble-updates noble-backports
            Components: main universe restricted multiverse
            Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

            Types: deb
            URIs: http://$mirror/ubuntu-ports/
            Suites: noble-security
            Components: main universe restricted multiverse
            Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

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
            // Debian 的没有 hosts，resolv.conf 指向 1.1.1.1；Ubuntu 的两个都是空文件。DNS 要走
            // 内核层的存根（设计文档 §10），系统的私人 DNS、VPN 才生效
            write(store, "/etc/hosts", "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n")
            write(store, "/etc/resolv.conf", "nameserver 127.0.0.53\n")
        }
        write(
            store, "/etc/profile.d/appine.sh",
            """
            cat /etc/motd 2>/dev/null
            export TERM=xterm-256color
            export LANG=C.UTF-8
            export NMAP_UNPRIVILEGED=1
            PS1='\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '
            cd "${'$'}HOME" 2>/dev/null

            """.trimIndent(),
        )
        val install = when (distro) {
            Distro.ALPINE -> "apk add git python3"
            else -> "apt update && apt install git python3"
        }
        write(
            store, "/etc/motd",
            """
            Welcome to appine: ${distro.label}, running inside an Android app.

            Install packages with ${install.substringBefore(' ')}, for example:  $install

            """.trimIndent(),
        )
    }

    /**
     * 并发给 [distro] 的镜像测速，把最快的写进包管理器的源；全部失败时保持原样。会阻塞
     * 几秒，在后台线程上调用。Debian、Ubuntu 的镜像只写主机名，测速用 https。
     */
    fun chooseMirror(context: Context, distro: Distro) {
        val base = { mirror: String -> if (distro == Distro.ALPINE) mirror else "https://$mirror" }
        val speeds = probeAll(distro.mirrors.map { "${base(it)}/${distro.probe}" })
        val best = distro.mirrors.zip(speeds).filter { it.second != null }.maxByOrNull { it.second!! } ?: return
        Log.i(TAG, "mirror speeds: ${distro.mirrors.zip(speeds)}, using ${best.first}")
        runCatching {
            val (path, content) = sources(distro, best.first)
            write(store(context).path, path, content)
        }.onFailure { Log.w(TAG, "cannot write the package sources", it) }
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
