package io.github.mthli.appine

import android.os.SystemClock
import android.util.Log
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File

/** The distribution fixture contains packaged Vulkan tools, a gpu user and the survey binaries. */
@RunWith(AndroidJUnit4::class)
class GpuTest {
    private fun run(machine: Long, script: String, user: String = "gpu"): String {
        val console = Native.openConsole(machine, 24, 120)
        try {
            val pid = Native.spawn(machine, console, "/bin/su", arrayOf("su", "-s", "/bin/sh", user, "-c", script),
                arrayOf("PATH=/usr/local/bin:/usr/bin:/bin:/sbin", "HOME=/tmp"))
            val output = StringBuilder()
            val buf = ByteArray(4096)
            while (true) {
                val n = Native.read(machine, console, buf)
                if (n <= 0) break
                val s = buf.decodeToString(0, n)
                output.append(s)
                Log.i("appine-gpu-test", s)
            }
            assertEquals(output.toString(), 0, Native.waitFor(machine, pid))
            return output.toString()
        } finally {
            Native.closeConsole(machine, console)
        }
    }

    private fun reclaimed() {
        val deadline = SystemClock.uptimeMillis() + 5000
        do {
            val memory = File("/proc/self/maps").useLines { lines -> lines.count { "vkr-shmem" in it } }
            val workers = File("/proc/self/task").listFiles().orEmpty().count {
                runCatching { File(it, "comm").readText().trim() }.getOrNull() == "venus-retire"
            }
            if (memory == 0 && workers == 0) return
            SystemClock.sleep(25)
        } while (SystemClock.uptimeMillis() < deadline)
        throw AssertionError("Renderer mappings or retirement threads survived machine shutdown")
    }

    @Test(timeout = 60000)
    fun unsupportedDriver() {
        assumeTrue(InstrumentationRegistry.getArguments().getString("gpuUnsupported") == "true")
        val app = InstrumentationRegistry.getInstrumentation().targetContext
        val dir = File.createTempFile("venus-", ".store", app.cacheDir)
        check(dir.delete() && dir.mkdir())
        val tar = File.createTempFile("venus-", ".tgz", app.cacheDir)
        var machine = 0L
        try {
            app.assets.open("rootfs.tgz").use { input -> tar.outputStream().use(input::copyTo) }
            Native.importRootfs(dir.path, tar.path)
            machine = Native.start(dir.path, 1)
            run(machine, "test ! -e /dev/dri/renderD128 && test ! -d /sys/dev/char/226:128", "nobody")
        } finally {
            if (machine != 0L) Native.stop(machine)
            dir.deleteRecursively()
            tar.delete()
        }
    }

    @Test(timeout = 300000)
    fun distributionVenus() {
        val registry = InstrumentationRegistry.getInstrumentation()
        val cache = registry.targetContext.cacheDir
        val distro = InstrumentationRegistry.getArguments().getString("gpuDistro")
        assumeTrue(distro == "alpine" || distro == "debian")
        val tar = File(cache, "venus-$distro.tgz")
        assertTrue("Missing distribution fixture $tar", tar.isFile)
        val dir = File.createTempFile("venus-", ".store", cache)
        check(dir.delete() && dir.mkdir())
        var machine = 0L
        try {
            Native.importRootfs(dir.path, tar.path)
            machine = Native.start(dir.path, 1)
            val out = run(machine, "set -e; id; /tmp/lifetime; vulkaninfo --summary; /tmp/compute")
            for (expected in listOf("uid=1000", "DRIVER_ID_MESA_VENUS", "verified=65536")) assertTrue(out, expected in out)

            val concurrent = run(machine, """
                /tmp/compute --kill-in-flight & victim=$!
                /tmp/compute & survivor=$!
                echo terminal-responsive
                wait ${'$'}victim
                test $? -eq 137 || exit 1
                wait ${'$'}survivor
            """.trimIndent())
            for (expected in listOf("terminal-responsive", "unsignaled submission fence", "verified=65536"))
                assertTrue(concurrent, expected in concurrent)
            Native.stop(machine)
            machine = 0
            reclaimed()
            machine = Native.start(dir.path, 1)
            assertTrue("Restarted machine", "verified=65536" in run(machine, "/tmp/compute"))
        } finally {
            if (machine != 0L) Native.stop(machine)
            dir.deleteRecursively()
        }
        reclaimed()
    }
}
