package io.github.mthli.appine

import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith
import java.io.File
import java.lang.reflect.Proxy
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

/** Real shell title restoration in isolated stores. */
@RunWith(AndroidJUnit4::class)
class ShellTitleTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext

    private fun events(onTitle: (VtSession) -> Unit = {}): VtSession.Events = Proxy.newProxyInstance(
        VtSession.Events::class.java.classLoader, arrayOf(VtSession.Events::class.java),
    ) { _, method, args ->
        if (method.name == "onTitleChanged") onTitle(args!![0] as VtSession)
        null
    } as VtSession.Events

    private fun withMachine(test: (Long) -> Unit) {
        val store = File(context.cacheDir, "tab-title-${System.nanoTime()}")
        Rootfs.prepare(context, Rootfs.Distro.ALPINE, store = store)
        val machine = Native.start(store.path, 1)
        try {
            test(machine)
        } finally {
            Native.stop(machine)
            store.deleteRecursively()
        }
    }

    @Test fun bashRestoresDirectoryAfterProgramTitles() = checkShell(ash = false)

    @Test fun ashRestoresDirectoryAfterProgramTitles() = checkShell(ash = true)

    private fun checkShell(ash: Boolean) = withMachine { machine ->
        val titles = LinkedBlockingQueue<String>()
        lateinit var session: VtSession
        instrumentation.runOnMainSync {
            val script = if (ash) "cd /root; exec /bin/ash -l" else ConsolePty.script(context)
            session = VtSession(1, ConsolePty(machine, script, "root"), machine,
                events { titles.offer(it.title.orEmpty()) })
            session.start(80, 24, 8, 16)
        }
        fun awaitTitle(expected: String) {
            val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(30)
            val seen = mutableListOf<String>()
            while (System.nanoTime() < deadline) {
                val title = titles.poll(100, TimeUnit.MILLISECONDS) ?: continue
                seen.add(title)
                if (title == expected) return
            }
            throw AssertionError("Expected title $expected; received $seen")
        }
        fun send(text: String) {
            val bytes = text.toByteArray()
            assertEquals(bytes.size, Native.write(machine, session.console, bytes, 0, bytes.size))
        }
        try {
            awaitTitle("/root")
            send("cd /tmp\n")
            awaitTitle("/tmp")
            send("sh -c 'printf \"\\033]2;Building\\007\"; read -r answer'\n")
            awaitTitle("Building")
            send("\n")
            awaitTitle("/tmp")
            send("mkdir -p '/tmp/title spaces 中文'; cd '/tmp/title spaces 中文'\n")
            awaitTitle("/tmp/title spaces 中文")
            // A directory's BEL/ESC must not terminate OSC or inject another title.
            send("path=\$(printf '/tmp/title\\007\\033]2;Injected'); mkdir -p \"\$path\"; cd \"\$path\"\n")
            awaitTitle("/tmp/title??]2;Injected")
        } finally {
            instrumentation.runOnMainSync { session.release() }
        }
    }
}
