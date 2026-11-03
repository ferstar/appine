package io.github.mthli.appine

import android.annotation.SuppressLint
import android.content.Context
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import android.util.Log
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.net.HttpURLConnection
import java.net.URL
import java.security.MessageDigest
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.locks.ReentrantReadWriteLock
import kotlin.concurrent.write

/**
 * Importing and configuring the rootfs (the android/setup map): the first run imports the Alpine in
 * the APK, and Settings can switch to Debian (downloaded only when chosen; the version and sha256
 * are fixed here, and as with `scripts/fetch-rootfs` only a tarball whose digest matches is
 * accepted). After the import the configuration files are written, mirror speeds are probed in the
 * background, and the fastest one is written into the package manager's sources; it can also be
 * chosen by hand in Settings.
 */
internal object Rootfs {
    private const val TAG = "appine"

    /**
     * A tarball to download: `urls` are several sources of the same file; the digest is fixed, so
     * any of them is equally trustworthy.
     */
    data class Download(val urls: List<String>, val sha256: String, val size: Long)

    /**
     * The distros to choose from. `mirrors` is the built-in mirror list (the first one, the
     * official one, is used when probing fails), and `probe` is the file whose beginning is
     * downloaded when probing (relative to the mirror).
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
         * `debian:trixie` built by debuerreotype (the same build the official Docker image uses),
         * pinned to one commit. GitHub is the only source, and a direct connection from China is
         * very slow; no mirror site carries this build, and we do not host it ourselves for now.
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

    /** The mirror's host name, for display in Settings. */
    fun host(mirror: String): String = mirror.substringAfter("://").substringBefore('/')

    /** The most bytes and time a probe downloads. */
    private const val PROBE_BYTES = 512 * 1024
    private const val PROBE_TIMEOUT_MS = 5000

    /** Download timeout: for connecting and between two reads. */
    private const val DOWNLOAD_TIMEOUT_MS = 30_000

    /** The most times a download on one source continues after breaking off. */
    private const val RESUMES = 5

    /**
     * Records (SharedPreferences): the installed distro, the mirror written into the sources,
     * whether the user chose the mirror in Settings.
     */
    private const val KEY_DISTRO = "distro"
    private const val KEY_MIRROR = "mirror"
    private const val KEY_MIRROR_MANUAL = "mirror_manual"

    fun store(context: Context): File = File(context.filesDir, "rootfs")

    /** The store has been imported and configured. */
    fun isReady(context: Context): Boolean = isReady(store(context))

    private fun isReady(store: File) = File(store, "meta.db").exists()

    private fun prefs(context: Context) = context.getSharedPreferences("rootfs", Context.MODE_PRIVATE)

    /**
     * The distro installed in the app's store, null while there is no store. Recorded at import;
     * stores imported before round 190 have no record and count as Alpine (the app had not been
     * released yet, so only development installs have such stores).
     */
    fun installed(context: Context): Distro? {
        if (!isReady(context)) return null
        val name = prefs(context).getString(KEY_DISTRO, null) ?: return Distro.ALPINE
        return Distro.entries.firstOrNull { it.name == name } ?: Distro.ALPINE
    }

    /** Thrown when a download is aborted ([download]'s `cancelled` returns true). */
    class Cancelled : IOException("cancelled")

    private fun tarball(context: Context, distro: Distro) = File(context.cacheDir, "${distro.name.lowercase()}.tar.gz")

    /**
     * Downloads the tarball of [distro] into the cache directory, unless a verified one is already
     * in the cache. While downloading, [progress] receives the bytes downloaded so far and in
     * total; when [cancelled] returns true the download aborts, throwing [Cancelled]. Blocks; call
     * it on a background thread.
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
     * Imports [distro] into store [store] (by default the one the app uses). It is first imported
     * into a temporary directory and renamed when done, so a kill midway means starting over next
     * time. The tarball to download stays in the cache directory once verified ([download]) and is
     * deleted only after a successful import, so retrying a failed import needs no new download.
     * Blocks; call it on a background thread.
     */
    @SuppressLint("ApplySharedPref") // The record must be on disk before the rename into place
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
            // Written to disk before the rename: once the store is there, the record is its own
            prefs(context).edit()
                .putString(KEY_DISTRO, distro.name)
                .putString(KEY_MIRROR, distro.mirrors[0])
                .putBoolean(KEY_MIRROR_MANUAL, false)
                .commit()
        }
        // Sync before the rename: otherwise after a power loss the store could be in place while
        // its files are empty; after the rename, sync the directory too, so what is later written
        // into the store does not fall back into rootfs.tmp on a power loss and get deleted as an
        // unfinished import
        Native.syncRootfs(tmp.path)
        if (!tmp.renameTo(store)) throw IOException("cannot move the rootfs into place")
        syncDir(store.parentFile!!)
    }

    /** Makes renames and deletions in directory [dir] durable. */
    private fun syncDir(dir: File) {
        try {
            val fd = Os.open(dir.path, OsConstants.O_RDONLY, 0)
            try {
                Os.fsync(fd)
            } finally {
                Os.close(fd)
            }
        } catch (e: ErrnoException) {
            Log.w(TAG, "cannot sync ${dir.path}", e)
        }
    }

    /** Deletes the app's store (distro switch, rootfs reset). The kernel must be stopped first. */
    fun reset(context: Context) = access.write {
        File(context.filesDir, "rootfs.tmp").deleteRecursively()
        if (!store(context).deleteRecursively()) Log.w(TAG, "cannot delete all of the rootfs")
    }

    /**
     * Downloads [download] to [dest]: with several sources their speed is probed first, and they
     * are tried from the fastest, moving to the next one when one fails.
     * The sha256 is computed while writing, and on a mismatch (or a wrong length) the file is
     * deleted and an exception thrown; it is written to `.part` first and renamed only once
     * verified.
     * When [cancelled] returns true no other source is tried, and [Cancelled] is thrown.
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
     * Downloads from [url]. When a timeout or a dropped connection interrupts it, it continues from
     * where it broke off with `Range` (counted only when this attempt got data, at most [RESUMES]
     * times); when the server does not support resuming, it starts over.
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
                                // Progress is reported about two hundred times at most
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
     * An `/etc/network/interfaces` with only the loopback interface (written before every kernel
     * start, only when missing; one the user changed is left alone), the same as setup-alpine
     * writes. The network is the host's (the kernel/net map); services that need the network (nginx
     * and so on) have OpenRC bring up networking, which fails without this file, and then the
     * services do not start either. `rc_provide` in rc.conf is not used: it is added to every
     * service, and providing `clock` as well makes a cycle. Without OpenRC and ifupdown nobody
     * reads this file.
     */
    /**
     * Run in the guest before every boot (`sh -c`): when OpenRC is installed and none of
     * hwclock/osclock/swclock is in any runlevel, adds osclock to boot. busybox's syslog does
     * `need clock`, and OpenRC provides clock with hwclock by default, which cannot read an RTC or
     * set the host's time, so the boot log always has a failure; osclock means "the operating
     * system manages the time" and does nothing (the kernel/process map). Whichever clock service
     * the user put in place is left alone.
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
     * On Alpine, puts the doas package bundled with the app into the store
     * (`/var/cache/appine/doas.apk`): the login script installs it offline when a regular user
     * first becomes the login account (the android/setup map). Written before every kernel start,
     * so it is replaced by the new one after an app upgrade.
     */
    @Throws(IOException::class)
    fun ensureLoginTools(context: Context) {
        if (installed(context) != Distro.ALPINE) return
        val data = context.assets.open("doas.apk").use { it.readBytes() }
        Native.writeFile(store(context).path, "/var/cache/appine/doas.apk", data, 420 /* 0644 */, true)
    }

    /**
     * The services that did not start at this boot: OpenRC writes
     * ` * ERROR: <service> failed to start` into `/var/log/boot.log` (the kernel empties this file
     * at every boot, the kernel/process map). Empty without OpenRC or when it cannot be read.
     */
    fun failedServices(context: Context): List<String> {
        val log = runCatching { Native.readFile(store(context).path, "/var/log/boot.log") }.getOrNull()
            ?: return emptyList()
        return Regex("""ERROR: (\S+) failed to start""").findAll(String(log)).map { it.groupValues[1] }.distinct().toList()
    }

    /** An account in `/etc/passwd`. */
    data class Account(val name: String, val uid: Int, val gid: Int, val home: String)

    /** The accounts in `/etc/passwd`, empty when it cannot be read. */
    private fun accounts(context: Context): List<Pair<Account, String>> {
        val passwd = runCatching { Native.readFile(store(context).path, "/etc/passwd") }.getOrNull()
            ?: return emptyList()
        return String(passwd).lines().mapNotNull { line ->
            val f = line.split(':')
            if (f.size < 7) return@mapNotNull null
            val uid = f[2].toIntOrNull() ?: return@mapNotNull null
            val gid = f[3].toIntOrNull() ?: return@mapNotNull null
            Account(f[0], uid, gid, f[5]) to f[6]
        }
    }

    /**
     * The regular users that can be the login account: UID 1000–65533 with a login shell (not
     * nologin or false), sorted by UID; the first one is the one logged in "automatically". The
     * rules are the same as in the assets' `login.sh`. Empty when it cannot be read.
     */
    fun regularAccounts(context: Context): List<Account> = accounts(context).mapNotNull { (account, shell) ->
        if (shell.isEmpty() || shell.endsWith("/nologin") || shell.endsWith("/false")) return@mapNotNull null
        if (account.uid !in 1000..65533) return@mapNotNull null
        account
    }.sortedBy { it.uid }

    /**
     * Which account the login setting [login] ([ConsolePty.LOGIN_AUTO], [ConsolePty.LOGIN_ROOT] or
     * an account name) actually is: the rules are the same as in `login.sh`; automatic is the first
     * regular user created, or root when there is none, and a named account that no longer exists
     * is root too. When `/etc/passwd` cannot be read, it is root with home directory `/root`.
     */
    fun loginAccount(context: Context, login: String): Account {
        val all = accounts(context).map { it.first }
        val root = all.firstOrNull { it.name == ConsolePty.LOGIN_ROOT } ?: Account(ConsolePty.LOGIN_ROOT, 0, 0, "/root")
        return when (login) {
            ConsolePty.LOGIN_AUTO -> regularAccounts(context).firstOrNull() ?: root
            ConsolePty.LOGIN_ROOT -> root
            else -> all.firstOrNull { it.name == login } ?: root
        }
    }

    /**
     * The store's read-write lock. The kernel must be the only user of the store when it opens it;
     * only then does it do the boot cleanup (emptying `/dev/shm` and `/run`, deleting objects that
     * were not committed last time); the DocumentsProvider may open the store separately at any
     * time on binder threads, holding the read lock each time, and starting the kernel or deleting
     * the store holds the write lock.
     */
    val access = ReentrantReadWriteLock()

    /**
     * `/etc/profile.d/appine.sh`: shows the motd, sets the terminal and language environment
     * variables and a coloured prompt. Which directory the shell starts in is decided by the
     * assets' `login.sh` (tabs restored after the app was killed start in their original directory,
     * the android/service map).
     */
    private val PROFILE = """
        cat /etc/motd 2>/dev/null
        export TERM=xterm-256color
        export LANG=C.UTF-8
        export NMAP_UNPRIVILEGED=1
        PS1='\[\e[1;32m\]\u@\h\[\e[0m\]:\[\e[1;34m\]\w\[\e[0m\]\$ '

    """.trimIndent()

    /**
     * The copy written before round 194: it ended with one more line, `cd "$HOME"`, which took
     * restored tabs back to the home directory.
     */
    private val OLD_PROFILE = PROFILE.trimEnd('\n') + "\ncd \"${'$'}HOME\" 2>/dev/null\n"

    /**
     * Replaces the profile the app wrote into a previously imported store with the current one, if
     * it was not changed.
     */
    @Throws(IOException::class)
    fun updateProfile(context: Context) {
        val path = "/etc/profile.d/appine.sh"
        val old = runCatching { Native.readFile(store(context).path, path) }.getOrNull() ?: return
        if (String(old) == OLD_PROFILE) write(store(context).path, path, PROFILE)
    }

    /** The package manager's sources: which file they are written in and what the content is. */
    private fun sources(distro: Distro, mirror: String): Pair<String, String> = when (distro) {
        Distro.ALPINE -> "/etc/apk/repositories" to
            "$mirror/${BuildConfig.ALPINE_BRANCH}/main\n$mirror/${BuildConfig.ALPINE_BRANCH}/community\n"
        // The rootfs has no ca-certificates, so apt uses http (the packages are signed)
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
            // Debian's has no hosts, and resolv.conf points at 1.1.1.1. DNS must go through the
            // kernel layer's stub (the kernel/net map) for the system's Private DNS and VPNs to
            // take effect
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

    /**
     * The mirror written into the package manager's sources (an entry of [Distro.mirrors]); null
     * while there is no store.
     */
    fun mirror(context: Context): String? = if (isReady(context)) prefs(context).getString(KEY_MIRROR, null) else null

    /** The user chose the mirror in Settings (rather than a probe). */
    fun isMirrorManual(context: Context): Boolean = prefs(context).getBoolean(KEY_MIRROR_MANUAL, false)

    /**
     * Replaces the addresses of built-in mirrors (any of [Distro.mirrors], http or https) in the
     * sources file content [text] with [mirror], keeping the scheme; other addresses (repositories
     * the user added, mirrors outside the list) are left alone. Lines the user added (Alpine's
     * testing, Debian's backports) and changed components stay, and commented-out lines are
     * replaced too. Returns null when there is no built-in mirror anywhere.
     */
    fun retarget(distro: Distro, text: String, mirror: String): String? {
        val bare = { m: String -> m.substringAfter("://") }
        val known = distro.mirrors.joinToString("|") { Regex.escape(bare(it)) }
        val address = Regex("""(https?://)(?:$known)(?=[/\s]|$)""")
        if (!address.containsMatchIn(text)) return null
        return address.replace(text) { it.groupValues[1] + bare(mirror) }
    }

    /**
     * Switches the package manager's sources of [distro] to [mirror] ([retarget]); when the sources
     * file is gone it is rewritten by [sources]. When the sources contain no built-in mirror at all
     * (all replaced by mirrors outside the list), nothing changes and false is returned. It can be
     * written while the kernel runs too: the store's commit counter changes and the kernel layer's
     * caches are invalidated with it (the kernel/store map).
     */
    @Throws(IOException::class)
    private fun writeMirror(store: String, distro: Distro, mirror: String): Boolean {
        val (path, content) = sources(distro, mirror)
        val old = Native.readFile(store, path) ?: return true.also { write(store, path, content) }
        val new = retarget(distro, String(old), mirror) ?: return false
        if (new != String(old)) write(store, path, new)
        return true
    }

    /**
     * [mirror] was chosen in Settings: write it into the package manager's sources of [distro]
     * ([writeMirror]), after which no probe replaces it. Returns false when the sources have no
     * built-in mirror and nothing changed.
     */
    @Throws(IOException::class)
    fun setMirror(context: Context, distro: Distro, mirror: String): Boolean = synchronized(this) {
        if (!writeMirror(store(context).path, distro, mirror)) return false
        prefs(context).edit().putString(KEY_MIRROR, mirror).putBoolean(KEY_MIRROR_MANUAL, true).apply()
        return true
    }

    /**
     * Probes the mirrors of [distro] concurrently and writes the fastest into the package manager's
     * sources ([writeMirror]); if all fail, nothing changes. When the user chose a mirror in
     * Settings (also during the probe), it is left alone. Returns false when the sources have no
     * built-in mirror and nothing changed. Blocks for a few seconds; call it on a background
     * thread. Debian's mirrors are written as host names only, and probed with https.
     */
    fun chooseMirror(context: Context, distro: Distro): Boolean {
        prefs(context).edit().putBoolean(KEY_MIRROR_MANUAL, false).apply()
        val base = { mirror: String -> if (distro == Distro.ALPINE) mirror else "https://$mirror" }
        val speeds = probeAll(distro.mirrors.map { "${base(it)}/${distro.probe}" })
        val best = distro.mirrors.zip(speeds).filter { it.second != null }.maxByOrNull { it.second!! } ?: return true
        Log.i(TAG, "mirror speeds: ${distro.mirrors.zip(speeds)}, using ${best.first}")
        synchronized(this) {
            if (isMirrorManual(context)) return true
            val written = runCatching { writeMirror(store(context).path, distro, best.first) }
                .onFailure { Log.w(TAG, "cannot write the package sources", it) }
                .getOrDefault(true)
            if (written) {
                prefs(context).edit().putString(KEY_MIRROR, best.first).apply()
            } else {
                Log.i(TAG, "no built-in mirror in the package sources, leaving them alone")
            }
            return written
        }
    }

    /**
     * [urls] sorted by measured speed from fast to slow; those that could not be measured follow in
     * their original order. Not probed when there is only one.
     */
    private fun fastest(urls: List<String>): List<String> {
        if (urls.size < 2) return urls
        val speeds = probeAll(urls)
        Log.i(TAG, "download speeds: ${urls.zip(speeds)}")
        return urls.indices.sortedByDescending { speeds[it] ?: -1.0 }.map { urls[it] }
    }

    /**
     * Probes [urls] concurrently and returns each one's speed (bytes per ms), null for failures.
     * Blocks for a few seconds.
     */
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
     * Downloads the beginning of [address] and returns the speed (bytes per ms, including
     * connection setup); null when not a single byte arrived. Compared by throughput rather than by
     * the time to finish a small piece: some mirrors are fast for a small piece at the start and
     * are throttled after that.
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
