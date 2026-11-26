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

    @Test
    fun freshImportAbortsAndClosesBridgeOnFirstIoError() {
        val bridge = FakeBridge()
        val ran = mutableListOf<String>()
        val failed = mutableListOf<String>()

        assertThrows(IOException::class.java) {
            prepareStartBridge(
                importing = true,
                onFailure = { step, _ -> failed.add(step) },
                openBridge = { bridge },
                configure = { _, onFailure ->
                    Rootfs.runSetupStep("/etc/network/interfaces", onFailure) {
                        ran.add("network")
                        throw IOException("disk full")
                    }
                    Rootfs.runSetupStep("/etc/containers/containers.rootless.conf.d/appine.conf", onFailure) {
                        ran.add("containers")
                    }
                    true
                },
            )
        }

        assertTrue(bridge.closed)
        assertEquals(listOf("network"), ran)
        assertEquals(emptyList<String>(), failed)
    }

    @Test
    fun existingStoreDegradesIndependentlyAndKeepsWorkingBridge() {
        val bridge = FakeBridge()
        val ran = mutableListOf<String>()
        val failed = mutableListOf<String>()

        val result = prepareStartBridge(
            importing = false,
            onFailure = { step, _ -> failed.add(step) },
            openBridge = { bridge },
            configure = { opened, onFailure ->
                assertSame(bridge, opened)
                Rootfs.runSetupStep("/etc/containers/containers.rootless.conf.d/appine.conf", onFailure) {
                    ran.add("containers")
                    throw IOException("ENOTDIR")
                }
                Rootfs.runSetupStep("/etc/network/interfaces", onFailure) {
                    ran.add("network")
                }
                Rootfs.runSetupStep("/var/cache/appine/doas.apk", onFailure) {
                    ran.add("doas")
                    throw IOException("EEXIST")
                }
                Rootfs.runSetupStep(BrowserBridge.CONFIG_PATH, onFailure) {
                    ran.add("browser")
                }
            },
        )

        assertSame(bridge, result)
        assertFalse(bridge.closed)
        assertEquals(listOf("containers", "network", "doas", "browser"), ran)
        assertEquals(
            listOf("/etc/containers/containers.rootless.conf.d/appine.conf", "/var/cache/appine/doas.apk"),
            failed,
        )
    }

    @Test
    fun existingStoreClosesBridgeWhenBrowserConfigFails() {
        val bridge = FakeBridge()
        val ran = mutableListOf<String>()
        val failed = mutableListOf<String>()

        val result = prepareStartBridge(
            importing = false,
            onFailure = { step, _ -> failed.add(step) },
            openBridge = { bridge },
            configure = { _, onFailure ->
                val configured = Rootfs.runSetupStep(BrowserBridge.CONFIG_PATH, onFailure) {
                    ran.add("browser")
                    throw IOException("EEXIST")
                }
                Rootfs.runSetupStep("/etc/profile.d/appine.sh", onFailure) {
                    ran.add("profile")
                }
                configured
            },
        )

        assertNull(result)
        assertTrue(bridge.closed)
        assertEquals(listOf("browser", "profile"), ran)
        assertEquals(listOf(BrowserBridge.CONFIG_PATH), failed)
    }

    @Test
    fun bridgeOpenFailureDegradesOnExistingStoreAndAbortsFreshImport() {
        val ran = mutableListOf<String>()
        val failed = mutableListOf<String>()

        val result = prepareStartBridge<FakeBridge>(
            importing = false,
            onFailure = { step, _ -> failed.add(step) },
            openBridge = { throw IOException("bind failed") },
            configure = { bridge, onFailure ->
                assertNull(bridge)
                Rootfs.runSetupStep("/etc/network/interfaces", onFailure) {
                    ran.add("network")
                }
                false
            },
        )

        assertNull(result)
        assertEquals(listOf("network"), ran)
        assertEquals(listOf(BrowserBridge.CONFIG_PATH), failed)

        assertThrows(IOException::class.java) {
            prepareStartBridge<FakeBridge>(
                importing = true,
                onFailure = { _, _ -> },
                openBridge = { throw IOException("bind failed") },
                configure = { _, _ ->
                    ran.add("unexpected")
                    true
                },
            )
        }
        assertEquals(listOf("network"), ran)
    }

    @Test
    fun nonIoExceptionStillPropagatesAndClosesBridge() {
        val bridge = FakeBridge()

        assertThrows(IllegalStateException::class.java) {
            prepareStartBridge(
                importing = false,
                onFailure = { _, _ -> },
                openBridge = { bridge },
                configure = { _, _ -> throw IllegalStateException("bug") },
            )
        }

        assertTrue(bridge.closed)
    }
}
