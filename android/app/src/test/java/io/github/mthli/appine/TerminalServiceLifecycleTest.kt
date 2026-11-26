package io.github.mthli.appine

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.Closeable
import java.util.concurrent.Executors
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit

class TerminalServiceLifecycleTest {
    private class Fixture {
        var held = true
        val events = mutableListOf<String>()
        val stopped = LinkedBlockingQueue<Pair<Long, String>>()
        val lifecycle = ServiceLifecycle(::updateWakeLock)

        private fun updateWakeLock() {
            held = lifecycle.wantsWakeLock(RunPolicy.ALWAYS, machine = 0, switching = false) { false }
            events += "wake lock $held"
        }

        fun recordStop(handle: Long) {
            stopped.add(handle to Thread.currentThread().name)
        }

        fun awaitStop(): Pair<Long, String>? = stopped.poll(5, TimeUnit.SECONDS)

        fun completeExit(hasRestore: Boolean, hasListeners: Boolean) {
            lifecycle.completeExit(
                hasRestore, hasListeners,
                clearTabs = { events += "clear tabs" },
                startOrImport = { events += "start or import" },
                stopSelf = { events += "stop self" },
            )
        }
    }

    private fun shutdownWorker() = Executors.newSingleThreadExecutor().apply { shutdown() }

    @Test
    fun destroyedReplaceAndRestartDropTheirContinuations() {
        val worker = shutdownWorker()
        for (distro in listOf(null, Rootfs.Distro.ALPINE)) {
            for (exiting in listOf(false, true)) {
                val f = Fixture()
                f.lifecycle.transitioning = true
                f.lifecycle.exiting = exiting
                f.lifecycle.destroyed = true

                f.lifecycle.completeTransition(distro, start = { error("unexpected start") }, finishExit = {
                    worker.execute { error("unexpected exit") }
                })

                assertFalse(f.lifecycle.transitioning)
                assertFalse(f.held)
                assertEquals(listOf("wake lock false"), f.events)
            }
        }
    }

    @Test
    fun liveReplacementImportsEvenDuringExitAndRestartHonorsExit() {
        fun checkTransition(distro: Rootfs.Distro?, exiting: Boolean, expected: String) {
            val f = Fixture()
            f.lifecycle.transitioning = true
            f.lifecycle.exiting = exiting
            f.lifecycle.completeTransition(
                distro,
                start = {
                    assertFalse(f.lifecycle.transitioning)
                    f.events += "start $it"
                },
                finishExit = {
                    assertFalse(f.lifecycle.transitioning)
                    f.events += "exit $it"
                },
            )
            assertEquals(listOf(expected), f.events)
        }
        checkTransition(null, exiting = false, "start null")
        checkTransition(null, exiting = true, "exit 0")
        checkTransition(Rootfs.Distro.ALPINE, exiting = false, "start ALPINE")
        checkTransition(Rootfs.Distro.ALPINE, exiting = true, "start ALPINE")
    }

    @Test
    fun exitPostedBeforeDestructionLeavesTabsAndReturningListenerAlone() {
        for (hasListeners in listOf(false, true)) {
            val f = Fixture()
            f.lifecycle.exiting = true
            var posted: (() -> Unit)? = null
            f.lifecycle.finishExit(42, { it.run() }, stop = { f.events += "stop $it" }) {
                posted = { f.completeExit(hasRestore = false, hasListeners = hasListeners) }
            }

            f.lifecycle.destroyed = true
            checkNotNull(posted).invoke()

            assertFalse(f.lifecycle.exiting)
            assertFalse(f.held)
            assertEquals(listOf("stop 42", "wake lock false"), f.events)
        }
    }

    @Test
    fun destroyedExitStopsOnlyAnOwnedHandleOffTheWorker() {
        val worker = shutdownWorker()
        for (handle in listOf(0L, 42L)) {
            val f = Fixture()
            f.lifecycle.exiting = true
            f.lifecycle.destroyed = true

            f.lifecycle.finishExit(handle, worker, stop = f::recordStop) {
                error("unexpected exit continuation")
            }

            assertFalse(f.lifecycle.exiting)
            assertFalse(f.held)
            assertEquals(listOf("wake lock false"), f.events)
            if (handle == 0L) assertTrue(f.stopped.isEmpty())
            else assertEquals(handle to "appine-stop", f.awaitStop())
        }
    }

    @Test
    fun liveExitPreservesPendingRestoreAndRestartsForReturningListeners() {
        for (hasRestore in listOf(false, true)) {
            for (hasListeners in listOf(false, true)) {
                val f = Fixture()
                f.lifecycle.exiting = true
                f.completeExit(hasRestore, hasListeners)
                assertFalse(f.lifecycle.exiting)
                val expected = mutableListOf("wake lock false")
                if (!hasRestore) expected += "clear tabs"
                expected += if (hasListeners) "start or import" else "stop self"
                assertEquals(expected, f.events)
            }
        }
    }

    @Test
    fun destroyedStartDisposesOnlyASuccessfulResultEvenWithPendingExit() {
        for (success in listOf(false, true)) {
            val f = Fixture()
            val kernel = StartedKernel(42, Closeable { f.events += "close browser" }, true, false, null)
            val result = if (success) Result.success(kernel) else Result.failure(IllegalStateException())
            f.lifecycle.starting = true
            f.lifecycle.exiting = true
            f.lifecycle.destroyed = true

            f.lifecycle.completeStart(
                result,
                shutdown = f::recordStop,
                started = { error("unexpected kernel publication") },
                failed = { error("unexpected failure report") },
                exit = { error("unexpected exit") },
            )

            assertFalse(f.lifecycle.starting)
            assertFalse(f.held)
            if (success) {
                assertEquals(listOf("wake lock false", "close browser"), f.events)
                assertEquals(42L to "appine-stop", f.awaitStop())
            } else {
                assertEquals(listOf("wake lock false"), f.events)
                assertTrue(f.stopped.isEmpty())
            }
        }
    }

    @Test
    fun wakeLockPolicyCoversEachReasonToStayAwakeAndDestructionOverridesAll() {
        val state = ServiceLifecycle {}
        fun checkPolicies(machine: Long, switching: Boolean, busy: Boolean, vararg livePolicies: RunPolicy) {
            for (destroyed in listOf(false, true)) {
                state.destroyed = destroyed
                for (policy in RunPolicy.entries) {
                    assertEquals(policy.name, !destroyed && policy in livePolicies,
                        state.wantsWakeLock(policy, machine, switching) { busy })
                }
            }
        }
        checkPolicies(0, false, true)
        checkPolicies(0, true, false, RunPolicy.BUSY)
        checkPolicies(42, false, false, RunPolicy.ALWAYS)
        checkPolicies(42, false, true, RunPolicy.ALWAYS, RunPolicy.BUSY)
        for (flag in listOf(state::starting, state::transitioning, state::booting, state::exiting)) {
            flag.set(true)
            checkPolicies(0, false, false, RunPolicy.ALWAYS, RunPolicy.BUSY)
            flag.set(false)
        }
    }
}
