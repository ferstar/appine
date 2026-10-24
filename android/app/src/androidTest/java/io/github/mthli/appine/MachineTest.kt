package io.github.mthli.appine

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream
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

    /** 在新控制台上运行 `sh -c script`，返回输出和退出码。 */
    private fun sh(machine: Long, script: String): Pair<String, Int> {
        val console = Native.openConsole(machine, 24, 80)
        val pid = Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-c", script), ConsolePty.ENVIRONMENT)
        val out = readAll(machine, console)
        val code = Native.waitFor(machine, pid)
        Native.closeConsole(machine, console)
        return out to code
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
}
