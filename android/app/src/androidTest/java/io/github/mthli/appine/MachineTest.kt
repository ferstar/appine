package io.github.mthli.appine

import android.net.ConnectivityManager
import android.provider.DocumentsContract
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

    /**
     * 像 [login] 一样运行登录脚本（自动选账号），但一步一步交互：每一步等输出里（上一步等到的地方
     * 之后）出现 `wait`，最多两分钟，再输入 `send`。返回全部输出。
     */
    private fun loginSteps(machine: Long, vararg steps: Pair<String, String>): String {
        val console = Native.openConsole(machine, 24, 200)
        val argv = ConsolePty.argv(ConsolePty.script(context), ConsolePty.LOGIN_AUTO, null)
        val pid = Native.spawn(machine, console, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
        val bytes = ByteArrayOutputStream()
        val text = { synchronized(bytes) { bytes.toString("UTF-8") } }
        val reader = thread {
            val buf = ByteArray(4096)
            while (true) {
                val n = Native.read(machine, console, buf)
                if (n <= 0) break
                synchronized(bytes) { bytes.write(buf, 0, n) }
            }
        }
        var from = 0
        for ((wait, send) in steps) {
            val deadline = System.currentTimeMillis() + 120_000
            var at = text().indexOf(wait, from)
            while (at < 0 && System.currentTimeMillis() < deadline) {
                Thread.sleep(50)
                at = text().indexOf(wait, from)
            }
            assertTrue("no \"$wait\" in:\n${text()}", at >= 0)
            from = at + wait.length
            if (send.isEmpty()) continue
            val b = send.toByteArray()
            assertEquals(b.size, Native.write(machine, console, b, 0, b.size))
        }
        Native.waitFor(machine, pid)
        reader.join(5000)
        Native.closeConsole(machine, console)
        return text().replace("\r\n", "\n")
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
     * 按 inittab 开机、关机（kernel/process 的 map、设计文档 §11.2）：开机之后 once 的命令在跑，guest 的
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

    /**
     * 设置里手动选的镜像写进源：内核运行着也写得进去，guest 马上看得到；记下是手动选的。只换内置
     * 镜像的地址，用户加的行（testing、自己的仓库、注释掉的）留着；一处内置的镜像也没有时不动。
     */
    @Test
    fun manualMirrorIsWritten() {
        val machine = start(1)
        val distro = Rootfs.Distro.ALPINE
        val before = sh(machine, "cat /etc/apk/repositories").first
        val prefs = context.getSharedPreferences("rootfs", android.content.Context.MODE_PRIVATE)
        val saved = prefs.all
        val (official, tuna, ustc) = distro.mirrors
        val branch = BuildConfig.ALPINE_BRANCH
        try {
            sh(machine, "echo '@testing http://${official.removePrefix("https://")}/edge/testing' >> /etc/apk/repositories")
            sh(machine, "echo '#$official/$branch/community' >> /etc/apk/repositories")
            sh(machine, "echo 'https://packages.example.com/alpine/$branch/main' >> /etc/apk/repositories")
            assertTrue(Rootfs.setMirror(context, distro, ustc))
            assertTrue(Rootfs.isMirrorManual(context))
            assertEquals(ustc, Rootfs.mirror(context))
            val bare = ustc.removePrefix("https://")
            // 原来的源可能已经被 App 测速换过镜像
            assertEquals(
                distro.mirrors.fold(before) { text, m -> text.replace(m, ustc) } +
                    "@testing http://$bare/edge/testing\n#$ustc/$branch/community\n" +
                    "https://packages.example.com/alpine/$branch/main\n",
                sh(machine, "cat /etc/apk/repositories").first,
            )
            // 再换一次：上次选的也是内置的镜像
            assertTrue(Rootfs.setMirror(context, distro, tuna))
            assertTrue(sh(machine, "cat /etc/apk/repositories").first.startsWith("$tuna/$branch/main\n"))
            // 全换成了列表以外的镜像：不动，记录也不变
            val custom = "https://mirror.example.org/alpine/$branch/main\n"
            sh(machine, "printf '$custom' > /etc/apk/repositories")
            assertFalse(Rootfs.setMirror(context, distro, ustc))
            assertEquals(custom, sh(machine, "cat /etc/apk/repositories").first)
            assertEquals(tuna, Rootfs.mirror(context))
            // 源文件不在了：照导入时那样重写
            sh(machine, "rm /etc/apk/repositories")
            assertTrue(Rootfs.setMirror(context, distro, ustc))
            assertEquals("$ustc/$branch/main\n$ustc/$branch/community\n", sh(machine, "cat /etc/apk/repositories").first)
            // Debian 的 deb822：只换内置镜像的主机，协议、组件、别人的仓库照旧
            val debian = """
                Types: deb
                URIs: https://deb.debian.org/debian
                Suites: trixie trixie-updates trixie-backports
                Components: main contrib non-free

                Types: deb
                URIs: http://deb.debian.org/debian-security
                Suites: trixie-security
                Components: main

                Types: deb
                URIs: https://download.docker.com/linux/debian
                Suites: trixie
                Components: stable

            """.trimIndent()
            assertEquals(
                debian.replace("deb.debian.org", "mirrors.aliyun.com"),
                Rootfs.retarget(Rootfs.Distro.DEBIAN, debian, "mirrors.aliyun.com"),
            )
            assertEquals(null, Rootfs.retarget(Rootfs.Distro.DEBIAN, "URIs: http://ftp.cn.debian.org/debian\n", "mirrors.aliyun.com"))
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
     * apt-get update（它取不到索引时只警告、退出码仍是 0，所以看输出）；再建一个普通用户，自动以它登录：
     * 装 sudo 时按 Ctrl-C 跳过、shell 里按 Ctrl-C 不结束会话，下一次登录装上 sudo。要联网下载约 50 MB，默认跳过：
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
                // 普通用户第一次成为登录账号：加进 sudo 组，联网装 sudo。apt-get update 下载时按
                // Ctrl-C：这次跳过，登录照常。util-linux 的 su 不另起会话，shell 有控制终端和作业
                // 控制：Ctrl-C 只打断前台的 sleep，不会结束整个会话（原来标签页就关了）
                assertEquals(0, sh(machine, "useradd -m -s /bin/bash alice").second)
                val skipped = loginSteps(
                    machine,
                    "installing sudo" to "",
                    "% [" to "\u0003",
                    "$ " to "tty; echo sl''eeping; sleep 30\n",
                    "sleeping" to "\u0003",
                    "$ " to "echo still-here; exit\n",
                )
                assertTrue(skipped, skipped.contains("appine: skipped installing sudo"))
                // 输入的回显里没有这两段（bash 在命令的输出前面先写 \e[?2004l\r，不是换行）
                assertTrue(skipped, skipped.contains("/dev/pts/"))
                assertTrue(skipped, skipped.contains("still-here\n"))
                assertFalse(skipped, skipped.contains("Session terminated"))
                // 下一次登录装上 sudo（这次显示 apt 的进度），没设密码时不问
                val admin = login(machine, "id -un; id -nG; sudo -n id -un; exit\n")
                assertTrue(admin, admin.contains("Setting up sudo"))
                assertTrue(admin, admin.contains("alice is in the sudo group and can become root with sudo"))
                assertTrue(admin, becameAdmin(admin, "sudo"))
            } finally {
                Native.stop(machine)
                store.deleteRecursively()
            }
        }
    }

    /**
     * guest 的路由表（kernel/net 的 map）：App 读不了宿主的 `/proc/net/route`，交来默认网络的
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
     * 宿主不让绑的 1024 以下的端口（kernel/net 的 map）：App 没有 CAP_NET_BIND_SERVICE，79 也不在
     * Android 放开的那几个里；guest 的 nc 绑 79 时宿主上换成 10079，guest 里看到的还是 79。App
     * 经由 [Native.takeListens] 知道外部端口（§12 #7 的通知用它），从 App 进程连 10079 收得到
     * guest 发的数据，内核日志记着换到了哪里。
     */
    @Test
    fun lowPortsTheHostRefusesMoveToHighPorts() {
        assertTrue("the app itself could bind port 79", runCatching { ServerSocket(79).close() }.isFailure)
        val out = ncListen(79, 10079)
        assertTrue(out, out.contains("appine: port 79/tcp is 10079 on the host"))
    }

    /**
     * 宿主让绑的低端口：2026 年 5 月起的 Google Play 系统更新让 App 绑得了 22、80、443 这几个
     * （Android 13 以上、内核 5.15 以上；API 37 的模拟器上有），guest 的 nc 绑 22 就是宿主的 22，
     * 不换，也不记内核日志。没有这个更新的系统上跳过。
     */
    @Test
    fun lowPortsTheHostAllowsAreBoundAsTheyAre() {
        assumeTrue("apps cannot bind port 22 here", runCatching { ServerSocket(22).close() }.isSuccess)
        val out = ncListen(22, 22)
        assertTrue(out, out.contains("no log"))
    }

    /**
     * guest 的 nc 在 [port] 上监听：报告的监听事件是宿主上的 [hostPort]，从 App 进程连它收得到
     * guest 发的数据。返回控制台的输出（之后 `dmesg` 里关于这个端口的行，没有时是 "no log"）。
     */
    private fun ncListen(port: Int, hostPort: Int): String {
        val machine = start(1)
        val console = Native.openConsole(machine, 24, 80)
        val script = "echo hello | nc -l -p $port; dmesg | grep 'port $port/' || echo no log"
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
        assertEquals("nc\t$port\t$hostPort", listens?.singleOrNull())
        // nc 读完 stdin 之后不关连接：读到了就关掉，它才退出
        Socket("127.0.0.1", hostPort).use { s ->
            val got = ByteArray(6)
            DataInputStream(s.getInputStream()).readFully(got)
            assertEquals("hello\n", String(got))
        }
        reader.join(10_000)
        assertEquals(out, 0, Native.waitFor(machine, pid))
        assertEquals(0, Native.takeListens(machine)?.size)
        return out
    }

    /**
     * 内核层和引擎的 bug 只结束撞上它的 guest 进程（engine 的 map、设计文档 §12 第 5 条），用 debug 版 App 的
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

    /**
     * 系统"文件"App 看到的（[Documents]）：根是登录账号的家目录，打开"整个系统"之后多一个 `/`；
     * 列目录跟随符号链接，不列断了的链接和 FIFO；文件能读；家目录外面的要打开"整个系统"才看得到，
     * 也不能写；树的授权不能用 `..` 走出去；按名字搜索。文件是运行着的内核刚写的：另外打开仓库时
     * 内核先提交，看得到。
     */
    @Test
    fun documentsShowTheHomeDirectory() {
        val prefs = context.getSharedPreferences("settings", android.content.Context.MODE_PRIVATE)
        prefs.edit().putString("login", ConsolePty.LOGIN_ROOT).putBoolean("system_documents", false).commit()
        val machine = start(1)
        val setup = "rm -rf /root/dt && mkdir -p /root/dt/sub && printf hi > /root/dt/a.txt && " +
            "ln -s /etc/alpine-release /root/dt/release && ln -s /nowhere /root/dt/broken && " +
            "mkfifo /root/dt/fifo && printf x > '/root/dt/100% 中文.txt' && " +
            "printf 'export A=1\\n' > /root/dt/.bashrc && printf 'a\\0b' > /root/dt/blob"
        assertEquals(0, sh(machine, setup).second)
        val resolver = context.contentResolver
        val authority = Documents.authority(context)
        fun rows(uri: android.net.Uri, vararg columns: String, args: android.os.Bundle? = null): List<List<String?>>? =
            resolver.query(uri, null, args, null)?.use { c ->
                val out = mutableListOf<List<String?>>()
                while (c.moveToNext()) out.add(columns.map { c.getString(c.getColumnIndexOrThrow(it)) })
                out
            }
        val id = DocumentsContract.Document.COLUMN_DOCUMENT_ID
        val name = DocumentsContract.Document.COLUMN_DISPLAY_NAME
        val mime = DocumentsContract.Document.COLUMN_MIME_TYPE
        val size = DocumentsContract.Document.COLUMN_SIZE
        fun roots() = rows(DocumentsContract.buildRootsUri(authority), DocumentsContract.Root.COLUMN_ROOT_ID, id)
        fun document(doc: String) = DocumentsContract.buildDocumentUri(authority, doc)
        fun read(doc: String) = resolver.openInputStream(document(doc))!!.use { String(it.readBytes()) }
        try {
            assertEquals(listOf(listOf("home", "/root")), roots())
            assertEquals(
                // 按扩展名猜不出的按内容：文本是 text/plain（点文件、链接到的版本号），有 NUL 的不是
                listOf(
                    listOf("/root/dt/.bashrc", ".bashrc", "text/plain", "11"),
                    listOf("/root/dt/100%25 中文.txt", "100% 中文.txt", "text/plain", "1"),
                    listOf("/root/dt/a.txt", "a.txt", "text/plain", "2"),
                    listOf("/root/dt/blob", "blob", "application/octet-stream", "3"),
                    listOf("/root/dt/release", "release", "text/plain", null),
                    listOf("/root/dt/sub", "sub", DocumentsContract.Document.MIME_TYPE_DIR, null),
                ),
                rows(DocumentsContract.buildChildDocumentsUri(authority, "/root/dt"), id, name, mime, size)
                    ?.map { if (it[0] == "/root/dt/release") it.take(3) + listOf(null) else it },
            )
            assertEquals("hi", read("/root/dt/a.txt"))
            assertEquals("x", read("/root/dt/100%25 中文.txt"))
            assertTrue(read("/root/dt/release").startsWith("3."))
            // 家目录外面的、不规范的 ID 都当作不存在（查询返回 null）
            assertEquals(listOf(listOf("text/plain")), rows(document("/root/dt/release"), mime))
            assertEquals(null, rows(document("/etc/motd"), id))
            assertEquals(null, rows(document("/root/dt/../../etc/motd"), id))
            assertEquals(null, rows(document("/root//dt"), id))
            // 树的授权：里面的可以，`..` 走出去的不行
            val tree = DocumentsContract.buildTreeDocumentUri(authority, "/root/dt")
            assertEquals(
                listOf(listOf("/root/dt/sub")),
                rows(DocumentsContract.buildDocumentUriUsingTree(tree, "/root/dt/sub"), id),
            )
            assertThrows(SecurityException::class.java) {
                rows(DocumentsContract.buildDocumentUriUsingTree(tree, "/root/dt/../../etc/motd"), id)
            }
            // "文件"App 搜索时名字在参数里
            val search = DocumentsContract.buildSearchDocumentsUri(authority, Documents.ROOT_HOME, "A.TXT")
            val args = android.os.Bundle().apply { putString(DocumentsContract.QUERY_ARG_DISPLAY_NAME, "A.TXT") }
            val found = rows(search, id, args = args)
            assertEquals(listOf(listOf("/root/dt/a.txt")), found)
            // 打开"整个系统"
            prefs.edit().putBoolean("system_documents", true).commit()
            assertEquals(listOf(listOf("home", "/root"), listOf("system", "/")), roots())
            assertEquals(listOf(listOf("motd")), rows(document("/etc/motd"), name))
            // 家目录外面的只读
            assertThrows(Exception::class.java) { resolver.openOutputStream(document("/etc/motd"))!!.close() }
            assertTrue(rows(DocumentsContract.buildChildDocumentsUri(authority, "/"), name)!!.contains(listOf("etc")))
        } finally {
            prefs.edit().remove("login").remove("system_documents").commit()
            sh(machine, "rm -rf /root/dt")
        }
    }

    /**
     * 在"文件"App 里改家目录（[Documents]）：新建（名字有了时换一个、按类型补扩展名）、写、追加、
     * 截断、改名（不覆盖）、移动、删除整个目录；新建的属于登录账号。运行中的内核马上看得到。家目录
     * 外面的、经由符号链接走出去的都不行，家目录本身不能删。
     */
    @Test
    fun documentsWriteInTheHomeDirectory() {
        val prefs = context.getSharedPreferences("settings", android.content.Context.MODE_PRIVATE)
        val machine = start(1)
        val setup = "deluser --remove-home bob 2>/dev/null; adduser -D -u 1500 bob && mkdir /home/bob/dw && " +
            "ln -s /etc /home/bob/dw/etc-link && chown -h bob:bob /home/bob/dw /home/bob/dw/etc-link"
        assertEquals(0, sh(machine, setup).second)
        prefs.edit().putString("login", "bob").putBoolean("system_documents", false).commit()
        val resolver = context.contentResolver
        val authority = Documents.authority(context)
        fun uri(doc: String) = DocumentsContract.buildDocumentUri(authority, doc)
        fun id(uri: android.net.Uri?) = uri?.let { DocumentsContract.getDocumentId(it) }
        fun create(dir: String, mime: String, name: String) =
            id(DocumentsContract.createDocument(resolver, uri(dir), mime, name))
        fun write(doc: String, mode: String, text: String) =
            resolver.openOutputStream(uri(doc), mode)!!.use { it.write(text.toByteArray()) }
        fun flags(doc: String) = resolver.query(uri(doc), null, null, null, null)!!.use {
            it.moveToFirst()
            it.getInt(it.getColumnIndexOrThrow(DocumentsContract.Document.COLUMN_FLAGS))
        }
        val dw = "/home/bob/dw"
        try {
            assertEquals("$dw/notes.txt", create(dw, "text/plain", "notes"))
            assertEquals("$dw/notes (1).txt", create(dw, "text/plain", "notes.txt"))
            assertEquals("$dw/sub", create(dw, DocumentsContract.Document.MIME_TYPE_DIR, "sub"))
            assertEquals("$dw/a_b", create(dw, "application/octet-stream", "a/b"))
            write("$dw/notes.txt", "wt", "hello from Files")
            write("$dw/notes.txt", "wa", " again")
            assertEquals("hello from Files again" to 0, sh(machine, "cat $dw/notes.txt"))
            write("$dw/notes (1).txt", "w", "x")
            write("$dw/notes (1).txt", "wt", "y")
            assertEquals("y" to 0, sh(machine, "cat '$dw/notes (1).txt'"))
            val owners = "stat -c '%U %G %a %n' $dw/notes.txt $dw/sub"
            assertEquals("bob bob 644 $dw/notes.txt\nbob bob 755 $dw/sub\n" to 0, sh(machine, owners))
            // 文档的标志：家目录里面的能写、删、改名、移动，家目录本身只能在里面新建
            val home = DocumentsContract.Document.FLAG_DIR_SUPPORTS_CREATE
            assertEquals(home, flags("/home/bob"))
            assertTrue(flags("$dw/notes.txt") and DocumentsContract.Document.FLAG_SUPPORTS_WRITE != 0)
            assertTrue(flags("$dw/sub") and DocumentsContract.Document.FLAG_SUPPORTS_DELETE != 0)
            // 改名不覆盖；移动；删除整个目录
            assertThrows(Exception::class.java) {
                DocumentsContract.renameDocument(resolver, uri("$dw/notes (1).txt"), "notes.txt")
            }
            val renamed = DocumentsContract.renameDocument(resolver, uri("$dw/notes (1).txt"), "renamed.txt")
            assertEquals("$dw/renamed.txt", id(renamed))
            val moved = DocumentsContract.moveDocument(resolver, renamed!!, uri(dw), uri("$dw/sub"))
            assertEquals("$dw/sub/renamed.txt", id(moved))
            assertEquals("y" to 0, sh(machine, "cat $dw/sub/renamed.txt"))
            assertTrue(DocumentsContract.deleteDocument(resolver, uri("$dw/sub")))
            assertEquals(1, sh(machine, "test -e $dw/sub").second)
            // 家目录外面、经由符号链接走出去的、家目录本身
            val motd = sh(machine, "cat /etc/motd").first
            assertThrows(Exception::class.java) { create("/etc", "text/plain", "x") }
            assertThrows(Exception::class.java) { create("$dw/etc-link", "text/plain", "x") }
            assertThrows(Exception::class.java) { write("$dw/etc-link/motd", "wt", "gone") }
            assertThrows(Exception::class.java) { DocumentsContract.deleteDocument(resolver, uri("/home/bob")) }
            assertEquals(motd to 0, sh(machine, "cat /etc/motd"))
            assertEquals(1, sh(machine, "test -e /etc/x").second)
            // 删除符号链接只删链接
            assertTrue(DocumentsContract.deleteDocument(resolver, uri("$dw/etc-link")))
            assertEquals(0, sh(machine, "test ! -e $dw/etc-link && test -d /etc").second)
        } finally {
            prefs.edit().remove("login").remove("system_documents").commit()
            sh(machine, "deluser --remove-home bob")
        }
    }

    /**
     * "文件"App 改的东西交给运行中的内核，guest 里监视着的程序收到 inotify 事件：写的文件在"文件"App
     * 这一侧关闭时才报（[Documents.openDocument] 带着关闭的回调重新打开 object）。反过来，guest 改过
     * 目录项的目录由 [Native.takeChangedDirs] 取走，[Documents.dirsChanged] 通知"文件"App 刷新。
     */
    @Test
    fun documentsChangesReachTheKernelAndBack() {
        val prefs = context.getSharedPreferences("settings", android.content.Context.MODE_PRIVATE)
        val machine = start(1)
        val dw = "/home/bob/dw"
        val setup = "deluser --remove-home bob 2>/dev/null; adduser -D -u 1500 bob && mkdir $dw && chown bob:bob $dw"
        assertEquals(0, sh(machine, setup).second)
        prefs.edit().putString("login", "bob").putBoolean("system_documents", false).commit()
        val resolver = context.contentResolver
        val authority = Documents.authority(context)
        fun uri(doc: String) = DocumentsContract.buildDocumentUri(authority, doc)
        try {
            // busybox 的 inotifyd 印成"字母\t目录\t名字"：n 新建，c 修改，r 打开，w 写完关闭，m、y 移出移入，
            // d 删除。它在自己的控制台上跑，读到它退出（timeout）为止
            val console = Native.openConsole(machine, 24, 200)
            val argv = arrayOf("sh", "-c", "timeout 6 inotifyd - $dw")
            val pid = Native.spawn(machine, console, "/bin/sh", argv, ConsolePty.ENVIRONMENT)
            var out = ""
            val reader = thread { out = readAll(machine, console) }
            Thread.sleep(1500)
            val created = DocumentsContract.createDocument(resolver, uri(dw), "text/plain", "w.txt")!!
            resolver.openOutputStream(created, "wt")!!.use { it.write("from Files".toByteArray()) }
            val renamed = DocumentsContract.renameDocument(resolver, created, "v.txt")!!
            assertTrue(DocumentsContract.deleteDocument(resolver, renamed))
            Native.waitFor(machine, pid)
            reader.join(5000)
            Native.closeConsole(machine, console)
            val log = out.lines().joinToString("\n") { it.split('\t').let { f -> "${f[0]}\t${f.getOrElse(2) { "" }}" } }
            val want = listOf("n\tw.txt", "w\tw.txt", "m\tw.txt", "y\tv.txt", "d\tv.txt")
            assertTrue(out, want.all { log.lines().contains(it) })
            val lines = log.lines()
            assertTrue(log, lines.indexOf("w\tw.txt") > lines.indexOf("n\tw.txt"))
            // guest 改过目录项的目录：第一次取时才开始记，通知之后"文件"App 的观察者收到
            Native.takeChangedDirs(machine)
            assertEquals(0, sh(machine, "touch $dw/from-guest").second)
            val dirs = Native.takeChangedDirs(machine)!!
            assertTrue(dirs.joinToString(), dw in dirs)
            val latch = java.util.concurrent.CountDownLatch(1)
            val observer = object : android.database.ContentObserver(null) {
                override fun onChange(selfChange: Boolean) = latch.countDown()
            }
            resolver.registerContentObserver(DocumentsContract.buildChildDocumentsUri(authority, dw), false, observer)
            try {
                Documents.dirsChanged(context, dirs)
                assertTrue(latch.await(5, java.util.concurrent.TimeUnit.SECONDS))
            } finally {
                resolver.unregisterContentObserver(observer)
            }
        } finally {
            prefs.edit().remove("login").remove("system_documents").commit()
            sh(machine, "deluser --remove-home bob")
        }
    }

    private fun depth(n: Int): Int = if (n < 0) 0 else depth(n + 1) + 1

    /**
     * 内核层结束的进程留下报告给 App（[Native.takeKills]）：控制台、pid、进程名、原因（bug 的还有内核日志的
     * 最后几条），取走一次就没了。
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
        assertEquals("" to 0, sh(machine, "echo before the bug > /dev/kmsg"))
        assertEquals(-SIGSEGV, run(machine, "/tmp/kernel-panic", "kernel-panic").second)
        val bug = Native.takeKills(machine)!!.single().split('\t')
        assertEquals(listOf("kernel-panic", "bug"), bug.subList(2, 4))
        assertTrue(bug[0], bug[0].toInt() >= 0)
        assertTrue(bug[4], bug[4].contains("pid ${bug[1]} (kernel-panic)"))
        // 报告里有 panic 的消息和位置（通知上"复制报告"复制的就是它）
        assertTrue(bug[4], bug[4].contains(": panic injected by the test hook at core/kernel/src/bug.rs:"))
        // 还有内核日志里它前面的几条（guest 写的那一条）和它自己，带着时间
        val log = bug[5].split('\u001e')
        assertTrue(bug[5], log.all { it.startsWith("[") } && log.any { it.endsWith("] before the bug") })
        assertTrue(bug[5], log.last().endsWith(bug[4]))
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
