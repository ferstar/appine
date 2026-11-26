package io.github.mthli.appine

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.Closeable
import java.io.IOException

class StartConfigTest {
    private class FakeBridge : Closeable {
        var closed = false
            private set

        override fun close() {
            closed = true
        }
    }

    private class Harness(private val openError: IOException? = null) {
        val bridge = FakeBridge()
        val ran = mutableListOf<String>()
        val failed = mutableListOf<String>()

        fun run(
            importing: Boolean,
            configure: (FakeBridge?, (String, IOException) -> Unit) -> Boolean,
        ): FakeBridge? = prepareStartBridge(
            importing = importing,
            onFailure = { step, _ -> failed.add(step) },
            openBridge = { openError?.let { throw it } ?: bridge },
            configure = configure,
        )

        fun step(
            path: String,
            name: String,
            onFailure: (String, IOException) -> Unit,
            error: IOException? = null,
        ): Boolean = Rootfs.runSetupStep(path, onFailure) {
            ran.add(name)
            error?.let { throw it }
        }
    }

    @Test
    fun freshImportAbortsAndClosesBridgeOnFirstIoError() {
        val h = Harness()

        assertThrows(IOException::class.java) {
            h.run(importing = true) { _, onFailure ->
                h.step("/etc/network/interfaces", "network", onFailure, IOException("disk full"))
                h.step("/etc/containers/containers.rootless.conf.d/appine.conf", "containers", onFailure)
                true
            }
        }

        assertTrue(h.bridge.closed)
        assertEquals(listOf("network"), h.ran)
        assertEquals(emptyList<String>(), h.failed)
    }

    @Test
    fun existingStoreDegradesIndependentlyAndKeepsWorkingBridge() {
        val h = Harness()

        val result = h.run(importing = false) { opened, onFailure ->
            assertSame(h.bridge, opened)
            h.step("/etc/containers/containers.rootless.conf.d/appine.conf", "containers", onFailure, IOException("ENOTDIR"))
            h.step("/etc/network/interfaces", "network", onFailure)
            h.step("/var/cache/appine/doas.apk", "doas", onFailure, IOException("EEXIST"))
            h.step(BrowserBridge.CONFIG_PATH, "browser", onFailure)
        }

        assertSame(h.bridge, result)
        assertFalse(h.bridge.closed)
        assertEquals(listOf("containers", "network", "doas", "browser"), h.ran)
        assertEquals(
            listOf("/etc/containers/containers.rootless.conf.d/appine.conf", "/var/cache/appine/doas.apk"),
            h.failed,
        )
    }

    @Test
    fun existingStoreClosesBridgeWhenBrowserConfigFails() {
        val h = Harness()

        val result = h.run(importing = false) { _, onFailure ->
            val configured = h.step(BrowserBridge.CONFIG_PATH, "browser", onFailure, IOException("EEXIST"))
            h.step("/etc/profile.d/appine.sh", "profile", onFailure)
            configured
        }

        assertNull(result)
        assertTrue(h.bridge.closed)
        assertEquals(listOf("browser", "profile"), h.ran)
        assertEquals(listOf(BrowserBridge.CONFIG_PATH), h.failed)
    }

    @Test
    fun bridgeOpenFailureDegradesOnExistingStoreAndAbortsFreshImport() {
        val h = Harness(openError = IOException("bind failed"))

        val result = h.run(importing = false) { bridge, onFailure ->
            assertNull(bridge)
            h.step("/etc/network/interfaces", "network", onFailure)
            false
        }

        assertNull(result)
        assertEquals(listOf("network"), h.ran)
        assertEquals(listOf(BrowserBridge.CONFIG_PATH), h.failed)

        assertThrows(IOException::class.java) {
            h.run(importing = true) { _, _ ->
                h.ran.add("unexpected")
                true
            }
        }
        assertEquals(listOf("network"), h.ran)
    }

    @Test
    fun nonIoExceptionStillPropagatesAndClosesBridge() {
        val h = Harness()

        assertThrows(IllegalStateException::class.java) {
            h.run(importing = false) { _, _ -> throw IllegalStateException("bug") }
        }

        assertTrue(h.bridge.closed)
    }
}
