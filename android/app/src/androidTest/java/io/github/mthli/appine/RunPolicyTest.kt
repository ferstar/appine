package io.github.mthli.appine

import android.content.Context
import android.content.ContextWrapper
import androidx.core.content.edit
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.lang.reflect.Proxy

@RunWith(AndroidJUnit4::class)
class RunPolicyTest {
    // Keep these tests independent of the user's settings and running kernel.
    private val context = object : ContextWrapper(InstrumentationRegistry.getInstrumentation().targetContext) {
        override fun getSharedPreferences(name: String, mode: Int) =
            super.getSharedPreferences("run-policy-test-$name", mode)
    }
    private val prefs get() = context.getSharedPreferences("settings", Context.MODE_PRIVATE)

    @Before
    @After
    fun clear() { prefs.edit(commit = true) { clear() } }

    @Test
    fun oldSwitchRetainsItsBehaviour() {
        assertEquals(RunPolicy.SLEEP, Settings.runPolicy(context))
        assertEquals(RunPolicy.ALWAYS, Settings.nextRunPolicy(context))
        prefs.edit { putBoolean("keep_awake", true) }
        assertEquals(RunPolicy.ALWAYS, Settings.runPolicy(context))
        assertEquals(RunPolicy.SLEEP, Settings.nextRunPolicy(context))
        Settings.setRunPolicy(context, RunPolicy.BUSY)
        assertFalse(prefs.contains("keep_awake"))
        assertEquals(RunPolicy.BUSY, Settings.runPolicy(context))
    }

    @Test
    fun notificationRemembersTheLastNonAlwaysChoice() {
        Settings.setRunPolicy(context, RunPolicy.BUSY)
        repeat(2) {
            Settings.setRunPolicy(context, Settings.nextRunPolicy(context))
            assertEquals(RunPolicy.ALWAYS, Settings.runPolicy(context))
            assertEquals(RunPolicy.BUSY, Settings.nextRunPolicy(context))
            Settings.setRunPolicy(context, Settings.nextRunPolicy(context))
            assertEquals(RunPolicy.BUSY, Settings.runPolicy(context))
        }
        Settings.setRunPolicy(context, RunPolicy.SLEEP)
        Settings.setRunPolicy(context, RunPolicy.ALWAYS)
        assertEquals(RunPolicy.SLEEP, Settings.nextRunPolicy(context))
    }

    @Test
    fun idleGraceHandlesActivityFailuresAndStaleResults() {
        val activity = GuestActivity()
        activity.reset(0)
        activity.sample(10, 0)
        activity.sample(10, GuestActivity.IDLE_MS - 1)
        assertTrue(activity.isBusy(GuestActivity.IDLE_MS - 1))
        assertFalse(activity.isBusy(GuestActivity.IDLE_MS))
        activity.sample(11, 40_000)
        assertTrue(activity.isBusy(40_000))
        assertFalse(activity.isBusy(70_000))
        activity.sample(-1, 70_000)
        assertTrue(activity.isBusy(70_000))
        activity.reset(100_000)
        activity.sample(12, 80_000)
        assertTrue(activity.isBusy(129_999))
        assertFalse(activity.isBusy(130_000))
        activity.sample(0, 130_000)
        assertTrue(activity.isBusy(130_000))
        activity.sample(1, 131_000, 180_000)
        assertTrue(activity.isBusy(180_000))
        assertFalse(activity.isBusy(210_000))
    }

    @Test
    fun killRoutingIgnoresReusedConsoleNumbers() {
        val pendingKills = ArrayDeque<String>()
        val drained = mutableListOf<Pair<List<String>, Long>>()
        val tracker = ConsoleTracker(
            takeKills = {
                val taken = pendingKills.toTypedArray()
                pendingKills.clear()
                taken
            },
            onDrainedKills = { kills, epoch -> drained.add(kills.toList() to epoch) },
        )
        val events = Proxy.newProxyInstance(
            VtSession.Events::class.java.classLoader, arrayOf(VtSession.Events::class.java),
        ) { _, _, _ -> null } as VtSession.Events
        val consoleField = ConsolePty::class.java.getDeclaredField("console").apply { isAccessible = true }
        val openedField = ConsolePty::class.java.getDeclaredField("openedEpoch").apply { isAccessible = true }
        val closedField = ConsolePty::class.java.getDeclaredField("closedEpoch").apply { isAccessible = true }

        fun openSession(id: Int, console: Int): Pair<VtSession, ConsolePty> {
            val pty = ConsolePty(0L, "", "root", tracker = tracker)
            val (c, opened) = tracker.open { console }
            consoleField.setInt(pty, c)
            openedField.setLong(pty, opened)
            return VtSession(id, pty, 0L, events) to pty
        }

        val (first, firstPty) = openSession(1, 0)
        // A poll taken before first closes captures a kill on console 0.
        pendingKills.add("0\t10\tsh\toom\t\t100\t50")
        val (_, polledEpoch) = tracker.take()
        // A second kill happens right as first's shell exits and is drained on close.
        pendingKills.add("0\t10\tsh\toom\t\t100\t50")
        tracker.close(close = {}, onClosed = { closedField.setLong(firstPty, it) })
        assertEquals(1, drained.size)
        val drainedEpoch = drained.single().second

        // A new tab immediately reuses console 0.
        val (second, secondPty) = openSession(2, 0)
        val both = listOf(first, second)

        // Both the earlier poll and the close-drained kill route to first, never second.
        assertSame(first, TerminalService.sessionForKill(both, 0, polledEpoch))
        assertSame(first, TerminalService.sessionForKill(both, 0, drainedEpoch))
        // If first was already closed/removed, its kill is not misdelivered to second.
        assertNull(TerminalService.sessionForKill(listOf(second), 0, polledEpoch))
        assertNull(TerminalService.sessionForKill(listOf(second), 0, drainedEpoch))

        // A kill taken after second opened routes to second, never ended first.
        pendingKills.add("0\t20\tawk\toom\t\t100\t50")
        val (_, secondEpoch) = tracker.take()
        assertSame(second, TerminalService.sessionForKill(both, 0, secondEpoch))
        // If second is later closed and removed, its kill is not misdelivered back to first.
        tracker.close(close = {}, onClosed = { closedField.setLong(secondPty, it) })
        assertNull(TerminalService.sessionForKill(listOf(first), 0, secondEpoch))
    }
}
