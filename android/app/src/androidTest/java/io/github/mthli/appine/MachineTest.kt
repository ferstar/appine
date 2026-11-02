package io.github.mthli.appine

import android.net.ConnectivityManager
import android.system.Os
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.File
import java.io.IOException
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.security.MessageDigest
import java.nio.ByteBuffer
import java.nio.ByteOrder
import kotlin.concurrent.thread

/**
 * 在真机或模拟器上检查原生库：导入 rootfs、启动内核、在控制台上运行 shell（设计文档
 * §13.3 的 Android 专项测试）。4K 和 16K 页的设备上都要运行：
 * `./gradlew connectedDebugAndroidTest`。
 */
@RunWith(AndroidJUnit4::class)
class MachineTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext
    private val machines = mutableListOf<Long>()

    @Before
    fun prepare() {
        Rootfs.prepare(context, Rootfs.Distro.ALPINE)
        // 与 App 启动内核之前一样，把以前导入时写的 profile 换成现在的
        Rootfs.updateProfile(context)
    }

    @After
    fun stop() {
        machines.forEach(Native::stop)
    }

    private fun start(tier: Int) = Native.start(Rootfs.store(context).path, tier).also { machines.add(it) }

    /** 读控制台直到会话结束，换行统一成 \n。 */
    private fun readAll(machine: Long, console: Int): String {
        val out = ByteArrayOutputStream()
        val buf = ByteArray(4096)
        while (true) {
            val n = Native.read(machine, console, buf)
            if (n <= 0) break
            out.write(buf, 0, n)
        }
        return out.toString("UTF-8").replace("\r\n", "\n")
    }

    /** 在新控制台上运行 `program`，返回输出和 [Native.waitFor] 的结果。 */
    private fun run(machine: Long, program: String, vararg argv: String): Pair<String, Int> {
        val console = Native.openConsole(machine, 24, 80)
        val pid = Native.spawn(machine, console, program, arrayOf(*argv), ConsolePty.ENVIRONMENT)
        val out = readAll(machine, console)
        val code = Native.waitFor(machine, pid)
        Native.closeConsole(machine, console)
        return out to code
    }

    /** 在新控制台上运行 `sh -c script`，返回输出和退出码。 */
    private fun sh(machine: Long, script: String) = run(machine, "/bin/sh", "sh", "-c", script)

    /**
     * 在新控制台上像终端标签页一样运行登录脚本（[ConsolePty.script]，登录账号是 [account]），输入
     * [input]，返回输出。
     */
    private fun login(
        machine: Long,
        input: String,
        account: String = ConsolePty.LOGIN_AUTO,
        cwd: String? = null,
    ): String {
        val console = Native.openConsole(machine, 24, 200)
        val argv = ConsolePty.argv(ConsolePty.script(context), account, cwd)
        val pid = Native.spawn(machine, console, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
        var out = ""
        val reader = thread { out = readAll(machine, console) }
        val b = input.toByteArray()
        assertEquals(b.size, Native.write(machine, console, b, 0, b.size))
        Native.waitFor(machine, pid)
        reader.join(5000)
        Native.closeConsole(machine, console)
        return out
    }

    @Test
    fun busyboxRunsUnderBothTiers() {
        for (tier in 0..1) {
            val (out, code) = sh(
                start(tier),
                "echo hello; getconf PAGESIZE; cat /etc/alpine-release; " +
                    "echo x | sha256sum | cut -c1-16; (sleep 0.1 & wait); ls /proc/self/fd | wc -l; tty",
            )
            assertEquals(out, 0, code)
            val lines = out.lines()
            assertEquals(out, "hello", lines[0])
            // guest 的页大小与宿主（可能是 16K）无关
            assertEquals(out, "4096", lines[1])
            assertTrue(out, lines[2].startsWith("3."))
            assertEquals(out, "73cb3858a687a849", lines[3])
            assertTrue(out, lines[5].startsWith("/dev/pts/"))
        }
    }

    @Test
    fun interactiveShellTakesInputAndControlC() {
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val pid = Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-i"), ConsolePty.ENVIRONMENT)
        var output = ""
        val reader = thread { output = readAll(machine, console) }
        fun type(s: String) {
            val b = s.toByteArray()
            assertEquals(b.size, Native.write(machine, console, b, 0, b.size))
        }
        type("sleep 30\n")
        Thread.sleep(500)
        // ^C 经过行规程变成发给前台进程组的 SIGINT
        type("\u0003")
        type("echo status=\$?\n")
        type("exit 7\n")
        assertEquals(7, Native.waitFor(machine, pid))
        reader.join(5000)
        assertTrue(output, output.contains("status=130"))
    }

    @Test
    fun consolesRunConcurrently() {
        val machine = start(1)
        val results = Array(3) { "" }
        val workers = (0 until 3).map { i ->
            thread { results[i] = sh(machine, "for n in 1 2 3; do echo $i-\$n; sleep 0.05; done").first }
        }
        workers.forEach { it.join(10_000) }
        for (i in 0 until 3) assertEquals(results[i], "$i-1\n$i-2\n$i-3\n", results[i])
    }

    /** 释放缓存（onTrimMemory）时正在运行的进程不受影响，之后照常起新进程。 */
    @Test
    fun trimMemoryKeepsTheMachineWorking() {
        val machine = start(1)
        assertEquals("ok\n" to 0, sh(machine, "echo ok"))
        var result = "" to -1
        val running = thread {
            result = sh(machine, "for n in 1 2 3 4 5; do sha256sum /bin/busybox >/dev/null; sleep 0.05; done; echo done")
        }
        repeat(5) {
            Native.trimMemory(machine)
            Thread.sleep(50)
        }
        running.join(30_000)
        assertEquals("done\n" to 0, result)
        Native.trimMemory(machine)
        assertEquals("ok\n" to 0, sh(machine, "echo ok"))
    }

    /**
     * 内核停下之后再用它的句柄也安全：会话结束之后，TerminalSession 在主线程上才清理控制台，
     * 这时服务可能已经停了内核（重置 rootfs、服务销毁）。
     */
    @Test
    fun callsAfterStopAreHarmless() {
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val pid = Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-c", "sleep 30"), ConsolePty.ENVIRONMENT)
        Native.stop(machine)
        Native.closeConsole(machine, console)
        Native.resize(machine, console, 30, 100)
        Native.trimMemory(machine)
        Native.setRoutes(machine, null)
        assertEquals(-1, Native.read(machine, console, ByteArray(16)))
        assertEquals(-1, Native.write(machine, console, ByteArray(1), 0, 1))
        assertEquals(-9, Native.waitFor(machine, pid))
        assertThrows(IOException::class.java) { Native.openConsole(machine, 24, 80) }
        Native.stop(machine)
    }

    /**
     * 按 inittab 开机、关机（设计文档 §6、§11.2）：开机之后 once 的命令在跑，guest 的
     * `poweroff` 是关机的请求，[Native.shutdown] 运行关机的命令、结束所有进程、释放句柄。
     * 启动之前写好只有回环接口的 /etc/network/interfaces。
     */
    @Test
    fun bootsAndShutsDownByInittab() {
        val store = Rootfs.store(context).path
        val probe = start(1)
        val original = sh(probe, "cat /etc/inittab").first.toByteArray()
        Native.stop(probe)
        val inittab = "::sysinit:/bin/sh -c 'echo sysinit > /tmp/inittab-order'\n" +
            // OpenRC 的服务没起来时这样写（eerror），重复的只算一次
            "::sysinit:/bin/sh -c 'for s in fake fake other; do echo \" * ERROR: \$s failed to start\"; done'\n" +
            "::once:/bin/sleep 30\n" +
            "::shutdown:/bin/sh -c 'echo shutdown >> /tmp/inittab-order'\n"
        Native.writeFile(store, "/etc/inittab", inittab.toByteArray(), 420, true)
        try {
            Rootfs.ensureNetworkConfig(context)
            val machine = start(1)
            Native.boot(machine)
            assertTrue(Native.processes(machine) >= 1)
            assertEquals("sysinit\n" to 0, sh(machine, "cat /tmp/inittab-order"))
            assertEquals(listOf("fake", "other"), Rootfs.failedServices(context))
            assertEquals(Native.POWER_NONE, Native.takePower(machine))
            assertEquals(0, sh(machine, "poweroff").second)
            assertEquals(Native.POWER_OFF, Native.takePower(machine))
            assertEquals(Native.POWER_NONE, Native.takePower(machine))
            val begin = System.nanoTime()
            Native.shutdown(machine)
            assertTrue(System.nanoTime() - begin < 10_000_000_000L)
            assertEquals(0, Native.processes(machine))
            val after = start(1)
            assertEquals("sysinit\nshutdown\n" to 0, sh(after, "cat /tmp/inittab-order"))
            assertTrue(sh(after, "cat /etc/network/interfaces").first.contains("iface lo inet loopback"))
        } finally {
            machines.forEach(Native::stop)
            Native.writeFile(store, "/etc/inittab", original, 420, true)
        }
    }

    /**
     * 开机之前把 osclock 加进 boot（[Rootfs.PREFER_OSCLOCK]）：没装 OpenRC 时什么也不做；用一个
     * 假的 rc-update 看它什么时候被调用，用户放了别的时钟服务时不调用。同时检查不带控制台的
     * [Native.spawn]。
     */
    @Test
    fun prefersOsclockOverHwclock() {
        val machine = start(1)
        // minirootfs 里没有 OpenRC 的目录：有的话是装过 OpenRC，不动它
        assumeTrue(sh(machine, "test -e /etc/init.d -o -e /etc/runlevels").second != 0)
        val argv = arrayOf("sh", "-c", Rootfs.PREFER_OSCLOCK)
        fun prefer() = Native.waitFor(machine, Native.spawn(machine, -1, "/bin/sh", argv, ConsolePty.ENVIRONMENT))
        assertEquals(0, prefer())
        val fake = "mkdir -p /etc/init.d /etc/runlevels/boot && touch /etc/init.d/osclock && " +
            "printf '#!/bin/sh\\necho \"\$*\" >> /tmp/rc-update-args\\n' > /sbin/rc-update && " +
            "chmod +x /sbin/rc-update && rm -f /tmp/rc-update-args"
        try {
            assertEquals(0, sh(machine, fake).second)
            assertEquals(0, prefer())
            assertEquals("-q add osclock boot\n" to 0, sh(machine, "cat /tmp/rc-update-args"))
            assertEquals(0, sh(machine, "rm /tmp/rc-update-args; ln -s /etc/init.d/hwclock /etc/runlevels/boot/hwclock").second)
            assertEquals(0, prefer())
            assertEquals(1, sh(machine, "test -e /tmp/rc-update-args").second)
        } finally {
            sh(machine, "rm -rf /sbin/rc-update /etc/init.d /etc/runlevels /tmp/rc-update-args")
        }
    }

    /** 登录 shell 是 /etc/passwd 里 root 的 shell，chsh 改过之后照样；它读 /etc/profile。 */
    @Test
    fun loginShellComesFromPasswd() {
        val machine = start(1)
        val shell = sh(machine, "awk -F: '\$1 == \"root\" { print \$7 }' /etc/passwd").first.trim()
        val out = login(machine, "echo shell=\$SHELL\nexit\n")
        assertTrue(out, out.contains("shell=$shell\n"))
        assertTrue(out, out.contains("Welcome to appine"))
        assertEquals("" to 0, sh(machine, "sed -i '1s|:[^:]*\$|:/bin/ash|' /etc/passwd"))
        try {
            val changed = login(machine, "echo shell=\$SHELL\nexit\n")
            assertTrue(changed, changed.contains("shell=/bin/ash\n"))
        } finally {
            sh(machine, "sed -i '1s|:[^:]*\$|:$shell|' /etc/passwd")
        }
    }

    /**
     * 恢复的标签页从原来的目录开始（设计文档 §11.2）：root 进得去就进；目录不在了回家目录；普通
     * 用户以自己的身份进，进不去（/root）回自己的家目录。
     */
    @Test
    fun loginStartsInTheSavedDirectory() {
        Rootfs.ensureLoginTools(context)
        val machine = start(1)
        try {
            assertTrue(login(machine, "pwd; exit\n", ConsolePty.LOGIN_ROOT, "/etc").contains("\n/etc\n"))
            assertTrue(login(machine, "pwd; exit\n", ConsolePty.LOGIN_ROOT, "/no/such/dir").contains("\n/root\n"))
            assertTrue(login(machine, "pwd; exit\n", ConsolePty.LOGIN_ROOT).contains("\n/root\n"))
            assertEquals(0, sh(machine, "adduser -D -s /bin/sh alice").second)
            assertTrue(login(machine, "pwd; exit\n", "alice", "/usr/share").contains("\n/usr/share\n"))
            val denied = login(machine, "pwd; exit\n", "alice", "/root")
            assertTrue(denied, denied.contains("\n/home/alice\n"))
        } finally {
            sh(
                machine,
                "deluser --remove-home alice; apk del -q --no-network --repositories-file /dev/null doas; " +
                    "rm -rf /var/lib/appine /etc/doas.d",
            )
        }
    }

    /** 控制台前台进程组的当前目录跟着 shell 的 cd 变；shell 退出之后取不到。 */
    @Test
    fun consoleCwdFollowsTheForegroundShell() {
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val argv = ConsolePty.argv(ConsolePty.script(context), ConsolePty.LOGIN_ROOT)
        val pid = Native.spawn(machine, console, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
        val reader = thread { readAll(machine, console) }
        fun waitFor(want: String?): String? {
            val deadline = System.currentTimeMillis() + 10_000
            var cwd = Native.consoleCwd(machine, console)
            while (cwd != want && System.currentTimeMillis() < deadline) {
                Thread.sleep(50)
                cwd = Native.consoleCwd(machine, console)
            }
            return cwd
        }
        assertEquals("/root", waitFor("/root"))
        val cd = "cd /usr/lib\n".toByteArray()
        Native.write(machine, console, cd, 0, cd.size)
        assertEquals("/usr/lib", waitFor("/usr/lib"))
        val exit = "exit\n".toByteArray()
        Native.write(machine, console, exit, 0, exit.size)
        Native.waitFor(machine, pid)
        assertEquals(null, waitFor(null))
        reader.join(5000)
        Native.closeConsole(machine, console)
    }

    /**
     * 存下的标签页（设计文档 §11.2）：标记还在（被杀）时读回原样，读过一次标记就没了；正常退出
     * （clear）之后什么也读不到。
     */
    @Test
    fun tabsComeBackOnlyAfterAKill() {
        Tabs.clear(context)
        val tabs = listOf(
            Tabs.Tab("auto", "/etc", "line 1\nline 2"),
            Tabs.Tab("root", null, ""),
        )
        Tabs.markRunning(context)
        Tabs.save(context, tabs, 1)
        val saved = Tabs.takeKilled(context)
        assertEquals(tabs, saved?.tabs)
        assertEquals(1, saved?.current)
        assertTrue(saved!!.time > 0)
        assertEquals(null, Tabs.takeKilled(context))
        Tabs.markRunning(context)
        Tabs.save(context, tabs.take(1), 0)
        Tabs.clear(context)
        assertEquals(null, Tabs.takeKilled(context))
    }

    /**
     * 登录脚本的输出里：`id -un` 是 alice，`id -nG` 里有 [group]，`doas`/`sudo -n id -un` 是 root
     * （按行比，终端的输出里夹着控制序列和回车）。
     */
    private fun becameAdmin(out: String, group: String): Boolean {
        val lines = out.lines().map { it.trim() }
        val i = lines.indexOf("alice")
        return i >= 0 && lines.getOrNull(i + 1)?.split(' ')?.containsAll(listOf("alice", group)) == true &&
            lines.getOrNull(i + 2) == "root"
    }

    /**
     * 登录账号（设计文档 §11.1）：只有 root 时自动登录 root；建了普通用户之后自动以它登录，第一次
     * 登录时加进 wheel、离线装上 App 带的 doas，没设密码时 doas 不问密码；设了密码之后要问；
     * 明确选 root 时是 root，选的账号不在了也是 root。最后删掉账号、doas 和留下的记录。
     */
    @Test
    fun loginAccountsAndDoas() {
        Rootfs.ensureLoginTools(context)
        val machine = start(1)
        val show = "id -un; id -nG; doas -n id -un; exit\n"
        try {
            assertTrue(login(machine, "id -un; exit\n").contains("\nroot\n"))
            assertEquals(0, sh(machine, "adduser -D -s /bin/sh alice").second)
            assertEquals(listOf("alice"), Rootfs.regularAccounts(context).map { it.name })
            val first = login(machine, show)
            assertTrue(first, first.contains("alice is in the wheel group and can become root with doas"))
            assertTrue(first, becameAdmin(first, "wheel"))
            assertEquals(0, sh(machine, "echo alice:secret | chpasswd").second)
            val withPassword = login(machine, show)
            assertFalse(withPassword, withPassword.contains("can become root"))
            assertTrue(withPassword, withPassword.contains("doas: Authentication required"))
            assertTrue(login(machine, "id -un; exit\n", ConsolePty.LOGIN_ROOT).contains("\nroot\n"))
            val missing = login(machine, "id -un; exit\n", "bob")
            assertTrue(missing, missing.contains("there is no user bob") && missing.contains("\nroot\n"))
        } finally {
            sh(
                machine,
                "deluser --remove-home alice; apk del -q --no-network --repositories-file /dev/null doas; " +
                    "rm -rf /var/lib/appine /etc/doas.d",
            )
        }
    }

    /** 下载的 tarball 按长度和 sha256 校验，对不上时删掉（用 file: 地址代替网络）。 */
    @Test
    fun downloadsAreVerified() {
        val src = File(context.cacheDir, "download-src.tgz")
        context.assets.open("rootfs.tgz").use { input -> src.outputStream().use { input.copyTo(it) } }
        val sha = MessageDigest.getInstance("SHA-256").digest(src.readBytes()).joinToString("") { "%02x".format(it) }
        val dest = File(context.cacheDir, "download-dest.tgz")
        var last = 0L to 0L
        Rootfs.fetch(Rootfs.Download(listOf(src.toURI().toString()), sha, src.length()), dest) { done, total -> last = done to total }
        assertEquals(src.length() to src.length(), last)
        assertTrue(dest.readBytes().contentEquals(src.readBytes()))
        dest.delete()
        for (wrong in listOf(Rootfs.Download(listOf(src.toURI().toString()), "0".repeat(64), src.length()),
            Rootfs.Download(listOf(src.toURI().toString()), sha, src.length() + 1))) {
            assertThrows(IOException::class.java) { Rootfs.fetch(wrong, dest) { _, _ -> } }
            assertFalse(dest.exists() || File(dest.path + ".part").exists())
        }
        src.delete()
    }

    /**
     * 下载中途断开时带 Range 接着下；服务器不认 Range（回 200）时从头再来。本地的 HTTP 服务
     * 第一次只发前三分之一就断开连接。
     */
    @Test
    fun downloadsResumeWhereTheyBrokeOff() {
        val data = ByteArray(300_000) { (it * 7 + it / 251).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(data).joinToString("") { "%02x".format(it) }
        val dest = File(context.cacheDir, "download-resumed.tgz")
        for (ranges in listOf(true, false)) {
            val server = ServerSocket(0, 4, InetAddress.getByName("127.0.0.1"))
            val asked = mutableListOf<String>()
            val serving = thread {
                repeat(2) { n ->
                    server.accept().use { c ->
                        val reader = c.getInputStream().bufferedReader()
                        val headers = generateSequence { reader.readLine() }.takeWhile { it.isNotEmpty() }.toList()
                        val range = headers.firstOrNull { it.startsWith("Range:", ignoreCase = true) }
                        asked.add(range ?: "-")
                        val from = range?.substringAfter("bytes=")?.substringBefore('-')?.toInt()?.takeIf { ranges } ?: 0
                        val head = if (from > 0) {
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes $from-${data.size - 1}/${data.size}\r\n"
                        } else {
                            "HTTP/1.1 200 OK\r\n"
                        }
                        val out = c.getOutputStream()
                        out.write((head + "Content-Length: ${data.size - from}\r\nConnection: close\r\n\r\n").toByteArray())
                        out.write(data, from, if (n == 0) data.size / 3 else data.size - from)
                        out.flush()
                    }
                }
            }
            Rootfs.fetch(Rootfs.Download(listOf("http://127.0.0.1:${server.localPort}/rootfs.tgz"), sha, data.size.toLong()), dest) { _, _ -> }
            serving.join(5000)
            server.close()
            assertEquals("ranges=$ranges", listOf("-", "Range: bytes=100000-"), asked)
            assertTrue(dest.readBytes().contentEquals(data))
            dest.delete()
        }
    }

    /**
     * 下载中止时（设置里取消换发行版）抛出 [Rootfs.Cancelled]，下了一半的不留。本地的 HTTP 服务
     * 发三分之一之后停住，等客户端取消、断开。
     */
    @Test
    fun downloadsCanBeCancelled() {
        val data = ByteArray(300_000) { (it * 7).toByte() }
        val sha = MessageDigest.getInstance("SHA-256").digest(data).joinToString("") { "%02x".format(it) }
        val dest = File(context.cacheDir, "download-cancelled.tgz")
        val server = ServerSocket(0, 4, InetAddress.getByName("127.0.0.1"))
        val sent = java.util.concurrent.CountDownLatch(1)
        val serving = thread {
            server.accept().use { c ->
                val reader = c.getInputStream().bufferedReader()
                generateSequence { reader.readLine() }.takeWhile { it.isNotEmpty() }.count()
                val out = c.getOutputStream()
                out.write("HTTP/1.1 200 OK\r\nContent-Length: ${data.size}\r\nConnection: close\r\n\r\n".toByteArray())
                out.write(data, 0, data.size / 3)
                out.flush()
                sent.countDown()
                // 停住，等客户端放弃（读到 EOF）
                runCatching { c.getInputStream().read() }
            }
        }
        var cancelled = false
        val url = "http://127.0.0.1:${server.localPort}/rootfs.tgz"
        assertThrows(Rootfs.Cancelled::class.java) {
            Rootfs.fetch(Rootfs.Download(listOf(url), sha, data.size.toLong()), dest, cancelled = { cancelled }) { done, _ ->
                if (done >= data.size / 3) {
                    sent.await()
                    cancelled = true
                }
            }
        }
        server.close()
        serving.join(5000)
        assertFalse(dest.exists() || File(dest.path + ".part").exists())
    }

    /** 设置里手动选的镜像写进源：内核运行着也写得进去，guest 马上看得到；记下是手动选的。 */
    @Test
    fun manualMirrorIsWritten() {
        val machine = start(1)
        val distro = Rootfs.Distro.ALPINE
        val before = sh(machine, "cat /etc/apk/repositories").first
        val prefs = context.getSharedPreferences("rootfs", android.content.Context.MODE_PRIVATE)
        val saved = prefs.all
        try {
            Rootfs.setMirror(context, distro, distro.mirrors[2])
            assertTrue(Rootfs.isMirrorManual(context))
            assertEquals(distro.mirrors[2], Rootfs.mirror(context))
            val written = sh(machine, "cat /etc/apk/repositories").first
            assertTrue(written, written.startsWith("${distro.mirrors[2]}/"))
            assertTrue(written, written.contains("/community\n"))
        } finally {
            // 恢复原来的源和记录：别的用例、App 本身都用这个仓库
            Native.writeFile(Rootfs.store(context).path, "/etc/apk/repositories", before.toByteArray(), 420, true)
            prefs.edit().clear().apply {
                for ((k, v) in saved) {
                    when (v) {
                        is String -> putString(k, v)
                        is Boolean -> putBoolean(k, v)
                    }
                }
            }.commit()
        }
    }

    /**
     * 构建时生成的开源许可（设计文档 §13.4）打进了 APK：appine 自己的 AGPL 声明和全文、Termux
     * 的库、Alpine 里 GPL 的包（带 aports 的提交）、Android 库、链接进库的 crate（不含过程宏）、
     * 许可证全文；索引里的每个文件都在。
     */
    @Test
    fun noticesCoverTheApp() {
        val index = context.assets.open("notices/index.tsv").bufferedReader().readLines().map { it.split('\t') }
        assertTrue(index.all { it.size == 3 })
        fun text(title: String) = index.first { it[0].startsWith(title) }
            .let { context.assets.open("notices/${it[2]}").bufferedReader().readText() }
        val titles = index.map { it[0] }
        assertTrue(text("appine").contains("GNU AFFERO GENERAL PUBLIC LICENSE"))
        assertTrue(text("Termux").contains("GPLv3 only"))
        val alpine = text("Alpine Linux")
        assertTrue(alpine, Regex("""busybox \S+\n\s+License: GPL-2.0-only\n\s+aports: main/busybox at [0-9a-f]{40}""").containsMatchIn(alpine))
        assertTrue(text("Android libraries").contains("androidx.core:core:"))
        assertTrue(text("rusqlite ").contains("Permission is hereby granted"))
        assertTrue(titles.any { it.startsWith("SQLite ") })
        assertFalse("proc macros are not in the library", titles.any { it.startsWith("syn ") || it.startsWith("thiserror-impl ") })
        assertTrue(text("GNU General Public License 2.0").contains("Version 2, June 1991"))
        for (row in index) context.assets.open("notices/${row[2]}").close()
    }

    /**
     * 下载 Debian 的 rootfs 导入另外的仓库，在登录 shell（bash）里看系统信息、DNS，运行
     * apt-get update（它取不到索引时只警告、退出码仍是 0，所以看输出）；再建一个普通用户，自动以它登录时装上 sudo。要联网下载约 50 MB，默认跳过：
     * `./gradlew connectedDebugAndroidTest -Pandroid.testInstrumentationRunnerArguments.network=true`。
     */
    @Test
    fun downloadedDistributionsRun() {
        assumeTrue(InstrumentationRegistry.getArguments().getString("network") == "true")
        for (distro in listOf(Rootfs.Distro.DEBIAN)) {
            val store = File(context.cacheDir, "store-${distro.name.lowercase()}")
            store.deleteRecursively()
            Rootfs.prepare(context, distro, store = store)
            val machine = Native.start(store.path, 1)
            try {
                val out = login(
                    machine,
                    "grep PRETTY_NAME /etc/os-release; echo shell=\$SHELL; getent hosts localhost | head -n 1; " +
                        "apt-get update 2>&1 | grep -q 'Failed to fetch' || echo apt-ok\nexit\n",
                )
                assertTrue(out, out.contains(distro.label.substringBefore(' ')))
                assertTrue(out, out.contains("shell=/bin/bash\n"))
                assertTrue(out, out.contains("apt-ok\n"))
                // 普通用户第一次成为登录账号：加进 sudo 组，联网装上 sudo，没设密码时不问
                assertEquals(0, sh(machine, "useradd -m -s /bin/bash alice").second)
                val admin = login(machine, "id -un; id -nG; sudo -n id -un; exit\n")
                assertTrue(admin, admin.contains("alice is in the sudo group and can become root with sudo"))
                assertTrue(admin, becameAdmin(admin, "sudo"))
            } finally {
                Native.stop(machine)
                store.deleteRecursively()
            }
        }
    }

    /**
     * guest 的路由表（设计文档 §10）：App 读不了宿主的 `/proc/net/route`，交来默认网络的
     * `LinkProperties` 之前是按接口的地址合成的（默认路由没有网关），交来之后有网关；
     * `ip route get` 的源地址由宿主选。
     */
    @Test
    fun routesComeFromLinkProperties() {
        val machine = start(1)
        val cm = context.getSystemService(ConnectivityManager::class.java)
        val lp = cm.getLinkProperties(cm.activeNetwork)
        assertNotNull("no default network", lp)
        val default = lp!!.routes.first { it.isDefaultRoute && it.hasGateway() && it.gateway!!.address.size == 4 }
        val (before, _) = sh(machine, "ip route; ip route get 1.1.1.1")
        assertTrue(before, before.contains("default dev ${default.`interface`} scope link"))
        assertTrue(before, before.contains("1.1.1.1 dev ${default.`interface`}  src "))
        Native.setRoutes(machine, TerminalService.routes(lp))
        val (after, code) = sh(machine, "ip route; ip route get 1.1.1.1; route -n")
        assertEquals(after, 0, code)
        val gateway = default.gateway!!.hostAddress
        assertTrue(after, after.contains("default via $gateway dev ${default.`interface`}"))
        assertTrue(after, after.contains("1.1.1.1 via $gateway dev ${default.`interface`}  src "))
        assertTrue(after, after.lines().any { it.startsWith("0.0.0.0 ") && it.contains(gateway!!) })
        Native.setRoutes(machine, null)
        assertTrue(sh(machine, "ip route").first.contains("default dev "))
    }

    /**
     * 1024 以下的端口（设计文档 §10）：App 没有 CAP_NET_BIND_SERVICE，宿主上绑不了 22；guest 的
     * nc 绑 22 时宿主上换成 10022，guest 里看到的还是 22。App 经由 [Native.takeListens] 知道
     * 外部端口（§12 #7 的通知用它），从 App 进程连 10022 收得到 guest 发的数据。
     */
    @Test
    fun lowPortsMoveToHighPortsOnTheHost() {
        val denied = runCatching { ServerSocket(22).close() }.isFailure
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val script = "echo hello | nc -l -p 22; dmesg | grep 'port 22/'"
        val pid = Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-c", script), ConsolePty.ENVIRONMENT)
        var out = ""
        val reader = thread { out = readAll(machine, console) }
        var listens: Array<String>? = null
        val deadline = System.currentTimeMillis() + 30_000
        while (System.currentTimeMillis() < deadline) {
            listens = Native.takeListens(machine)
            if (!listens.isNullOrEmpty()) break
            Thread.sleep(50)
        }
        assertEquals("nc\t22\t10022", listens?.singleOrNull())
        // nc 读完 stdin 之后不关连接：读到了就关掉，它才退出
        Socket("127.0.0.1", 10022).use { s ->
            val got = ByteArray(6)
            DataInputStream(s.getInputStream()).readFully(got)
            assertEquals("hello\n", String(got))
        }
        reader.join(10_000)
        assertEquals(out, 0, Native.waitFor(machine, pid))
        assertTrue(out, out.contains("appine: port 22/tcp is 10022 on the host"))
        assertTrue("the app itself could bind port 22", denied)
        assertEquals(0, Native.takeListens(machine)?.size)
    }

    /**
     * 内核层和引擎的 bug 只结束撞上它的 guest 进程（设计文档 §7.4、§12 第 5 条），用 debug 版 App 的
     * 测试钩子模拟。在 App 进程里，引擎在宿主上出错时 SIGSEGV 先经过 ART 的 libsigchain：ART 不认领，
     * 才交给引擎的处理函数，由它 siglongjmp 回去。宿主调用（JNI）里内核层 panic 时，那次调用失败，
     * 控制台被挂断。之后 ART 和 Machine 都照常工作。
     */
    @Test
    fun bugsKillOnlyTheGuestProcess() {
        Os.setenv("APPINE_TEST_HOOKS", "1", true)
        val store = Rootfs.store(context).path
        for ((name, code) in GUESTS) Native.writeFile(store, "/tmp/$name", elf(code), 0x1ed, true)
        for (tier in 0..1) {
            val machine = start(tier)
            assertEquals("tier $tier", -SIGSEGV, run(machine, "/tmp/engine-fault", "engine-fault").second)
            assertEquals("tier $tier", -SIGSEGV, run(machine, "/tmp/kernel-panic", "kernel-panic").second)
            // 读控制台时内核层 panic：读到 -1，会话组长收到 SIGHUP
            assertEquals("tier $tier", "" to -SIGHUP, run(machine, "/tmp/host-call", "host-call"))
            // ART 照常工作：GC，栈溢出变成 StackOverflowError
            Runtime.getRuntime().gc()
            assertTrue(runCatching { depth(0) }.exceptionOrNull() is StackOverflowError)
            assertEquals("ok\n" to 0, sh(machine, "echo ok"))
        }
    }

    private fun depth(n: Int): Int = if (n < 0) 0 else depth(n + 1) + 1

    /**
     * 内核层结束的进程留下报告给 App（[Native.takeKills]）：控制台、pid、进程名、原因，取走一次就没了。
     * 内核 bug 用测试钩子模拟，OOM 用 cgroup 的 memory.max：awk 拼出 64 MB 的字符串之后接着运行
     * 一会儿（OOM killer 定时量用量，一下子用完就退出的进程发现不了）。
     */
    @Test
    fun killsAreReportedToTheApp() {
        Os.setenv("APPINE_TEST_HOOKS", "1", true)
        val store = Rootfs.store(context).path
        Native.writeFile(store, "/tmp/kernel-panic", elf(GUESTS.getValue("kernel-panic")), 0x1ed, true)
        val machine = start(1)
        assertEquals(0, Native.takeKills(machine)?.size)
        assertEquals(-SIGSEGV, run(machine, "/tmp/kernel-panic", "kernel-panic").second)
        val bug = Native.takeKills(machine)!!.single().split('\t')
        assertEquals(listOf("kernel-panic", "bug"), bug.subList(2, 4))
        assertTrue(bug[0], bug[0].toInt() >= 0)
        assertTrue(bug[4], bug[4].contains("pid ${bug[1]} (kernel-panic)"))
        val script = "cd /sys/fs/cgroup && echo +memory > cgroup.subtree_control && mkdir -p oomt && " +
            "echo 16M > oomt/memory.max && " +
            "sh -c 'echo \$\$ > oomt/cgroup.procs; exec awk \"BEGIN { s = \\\"x\\\"; " +
            "for (i = 0; i < 26; i++) s = s s; for (j = 0; j < 50000000; j++) n++ }\"'; echo \$?"
        val (out, code) = sh(machine, script)
        assertTrue(out, out.endsWith("137\n") && code == 0)
        val oom = Native.takeKills(machine)!!.single().split('\t')
        assertEquals(listOf("awk", "oom", "/oomt", "16777216"), oom.subList(2, 5) + oom[6])
        assertTrue(oom[5], oom[5].toLong() > 16777216)
        assertEquals(0, Native.takeKills(machine)?.size)
    }

    private companion object {
        const val SIGHUP = 1
        const val SIGSEGV = 11

        /** 测试钩子的 syscall 编号（core/kernel/src/bug.rs），x0 是要模拟的 bug。 */
        const val HOOK = 0xa991
        val SVC = 0xd4000001L.toInt()

        /** `movz x<rd>, #imm`。 */
        fun movz(rd: Int, imm: Int) = (0xd2800000L or (imm.toLong() shl 5) or rd.toLong()).toInt()

        /** `exit(x0)`。 */
        val EXIT = intArrayOf(movz(8, 93), SVC)

        val GUESTS = mapOf(
            // 让栈所在的页在页表里指向不能访问的宿主内存，再写它：引擎在宿主上出错
            "engine-fault" to intArrayOf(
                movz(0, 2), 0x910003e1L.toInt() /* mov x1, sp */, movz(8, HOOK), SVC,
                0xf9000020L.toInt() /* str x0, [x1] */, movz(0, 4),
            ) + EXIT,
            "kernel-panic" to intArrayOf(movz(0, 1), movz(8, HOOK), SVC, movz(0, 7)) + EXIT,
            // 为宿主下一次调用 Machine 的方法布下 bug，然后读 stdin
            "host-call" to intArrayOf(
                movz(0, 6), movz(8, HOOK), SVC,
                movz(0, 0), 0xd10043e1L.toInt() /* sub x1, sp, #16 */, movz(2, 1), movz(8, 63), SVC,
                movz(0, 0),
            ) + EXIT,
        )

        /** 只有一个代码段的静态 aarch64 ELF：加载到 0x400000，从第一条指令开始执行。 */
        fun elf(code: IntArray): ByteArray {
            val base = 0x400000L
            val headers = 64 + 56
            val size = (headers + 4 * code.size).toLong()
            val b = ByteBuffer.allocate(size.toInt()).order(ByteOrder.LITTLE_ENDIAN)
            // 64 位、小端、版本 1
            b.put(byteArrayOf(0x7f, 'E'.code.toByte(), 'L'.code.toByte(), 'F'.code.toByte(), 2, 1, 1, 0))
            b.put(ByteArray(8))
            // ET_EXEC、EM_AARCH64、版本、入口、程序头和节头的偏移、标志
            b.putShort(2).putShort(183).putInt(1).putLong(base + headers).putLong(64).putLong(0).putInt(0)
            // 文件头和程序头的大小、一个程序头、没有节头
            b.putShort(64).putShort(56).putShort(1).putShort(64).putShort(0).putShort(0)
            // PT_LOAD：可读可执行，整个文件
            b.putInt(1).putInt(5).putLong(0).putLong(base).putLong(base).putLong(size).putLong(size).putLong(0x1000)
            code.forEach { b.putInt(it) }
            return b.array()
        }
    }
}
