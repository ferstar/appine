package io.github.mthli.appine

import android.net.ConnectivityManager
import android.system.Os
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream
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
        Rootfs.prepare(context)
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
