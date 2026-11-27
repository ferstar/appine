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

    private class FakeStoreAccess : Rootfs.StoreAccess {
        val files = linkedMapOf<String, ByteArray>()
        val obstructed = mutableSetOf<String>()
        val writes = mutableListOf<String>()
        val mutatingWrites = mutableListOf<String>()

        override fun readFile(path: String): ByteArray? {
            if (path in obstructed) throw IOException("obstructed: $path")
            return files[path]?.copyOf()
        }

        override fun writeFile(
            path: String,
            data: ByteArray,
            mode: Int,
            overwrite: Boolean,
            uid: Int,
            gid: Int,
        ) {
            writes.add(path)
            if (path in obstructed) throw IOException("obstructed: $path")
            if (!overwrite && path in files) return
            files[path] = data.copyOf()
            mutatingWrites.add(path)
        }
    }

    @Test
    fun prepareStartSkipsUnchangedAssetsAndPrechecksOpenRcMarker() {
        val store = FakeStoreAccess()
        val assetsRead = mutableListOf<String>()
        val assets = mutableMapOf(
            "doas.apk" to "doas-v1".toByteArray(),
            "appine-open-url" to "open-url-v1".toByteArray(),
            "xdg-open.sh" to "#!/bin/sh\nexec /usr/local/libexec/appine-open-url \"$1\"\n".toByteArray(),
            "appine-gpu.sh" to "#!/bin/sh\nexec \"$@\"\n".toByteArray(),
            "profile.sh" to "# current profile\n".toByteArray(),
            "ashrc.sh" to "# ashrc\n".toByteArray(),
            "bashrc.sh" to "# bashrc\n".toByteArray(),
            "zshrc.sh" to "# zshrc\n".toByteArray(),
        )
        fun readAsset(name: String): ByteArray {
            assetsRead.add(name)
            return assets.getValue(name)
        }

        // Seed an already-configured Alpine store.
        store.files["/etc/network/interfaces"] = "auto lo\niface lo inet loopback\n".toByteArray()
        store.files["/etc/containers/containers.rootless.conf.d/appine.conf"] = "[containers]\nnetns = \"host\"\n".toByteArray()
        store.files["/var/cache/appine/doas.apk"] = assets.getValue("doas.apk")
        store.files["/var/lib/appine/bash-startup-done"] = ByteArray(0)
        store.files["/usr/local/bin/appine-gpu"] = assets.getValue("appine-gpu.sh")
        store.files["/usr/local/libexec/appine-open-url"] = assets.getValue("appine-open-url")
        store.files["/usr/local/bin/xdg-open"] = assets.getValue("xdg-open.sh")
        store.files["/etc/profile.d/appine.sh"] = assets.getValue("profile.sh")
        store.files["/var/lib/appine/zsh-rc-hook-done"] = ByteArray(0)
        store.files["/var/lib/appine/openrc-setup-done"] = ByteArray(0)

        val prep = Rootfs.prepareStart(
            access = store,
            distro = Rootfs.Distro.ALPINE,
            gpu = true,
            browserConfiguration = "12345\ntoken-1\n",
            importing = false,
            readAsset = ::readAsset,
        )

        assertTrue(prep.browserReady)
        assertTrue(prep.openRcDone)
        // Only /etc/appine/browser actually wrote data; doas.apk, appine-open-url, xdg-open and
        // appine-gpu skipped redundant writes, and xdg-open.sh / appine-gpu.sh were not even read.
        assertEquals(listOf(BrowserBridge.CONFIG_PATH), store.mutatingWrites)
        assertFalse(assetsRead.contains("xdg-open.sh"))
        assertFalse(assetsRead.contains("appine-gpu.sh"))
        assertEquals("12345\ntoken-1\n", store.files[BrowserBridge.CONFIG_PATH]?.decodeToString())

        // After an app upgrade changes doas.apk and appine-open-url, both are refreshed.
        store.mutatingWrites.clear()
        assets["doas.apk"] = "doas-v2".toByteArray()
        assets["appine-open-url"] = "open-url-v2".toByteArray()
        val upgraded = Rootfs.prepareStart(
            access = store,
            distro = Rootfs.Distro.ALPINE,
            gpu = true,
            browserConfiguration = "23456\ntoken-2\n",
            importing = false,
            readAsset = ::readAsset,
        )

        assertTrue(upgraded.browserReady)
        assertTrue(upgraded.openRcDone)
        assertEquals(
            listOf("/var/cache/appine/doas.apk", "/usr/local/libexec/appine-open-url", BrowserBridge.CONFIG_PATH),
            store.mutatingWrites,
        )
        assertEquals("doas-v2", store.files["/var/cache/appine/doas.apk"]?.decodeToString())
        assertEquals("open-url-v2", store.files["/usr/local/libexec/appine-open-url"]?.decodeToString())
    }

    @Test
    fun prepareStartLeavesObstructedOpenRcMarkerToEnsureOpenRc() {
        val store = FakeStoreAccess()
        store.files["/var/lib/appine/bash-startup-done"] = ByteArray(0)
        store.files["/var/lib/appine/zsh-rc-hook-done"] = ByteArray(0)
        store.obstructed.add("/var/lib/appine/openrc-setup-done")
        val failed = mutableListOf<String>()

        val prep = Rootfs.prepareStart(
            access = store,
            distro = Rootfs.Distro.ALPINE,
            gpu = false,
            browserConfiguration = "12345\ntoken\n",
            importing = false,
            onFailure = { step, _ -> failed.add(step) },
            readAsset = { "asset".toByteArray() },
        )

        assertTrue(prep.browserReady)
        assertFalse(prep.openRcDone)
        assertEquals(emptyList<String>(), failed)
    }

    @Test
    fun prepareStartFailsFastOnImportAndDegradesOnExistingStore() {
        val store = FakeStoreAccess()
        store.files["/var/lib/appine/bash-startup-done"] = ByteArray(0)
        store.files["/var/lib/appine/zsh-rc-hook-done"] = ByteArray(0)
        store.obstructed.add("/var/cache/appine/doas.apk")
        store.obstructed.add(BrowserBridge.CONFIG_PATH)

        assertThrows(IOException::class.java) {
            Rootfs.prepareStart(
                access = store,
                distro = Rootfs.Distro.ALPINE,
                browserConfiguration = "12345\ntoken\n",
                importing = true,
                readAsset = { "asset".toByteArray() },
            )
        }
        assertFalse("/usr/local/libexec/appine-open-url" in store.files)

        val failed = mutableListOf<String>()
        val prep = Rootfs.prepareStart(
            access = store,
            distro = Rootfs.Distro.ALPINE,
            browserConfiguration = "12345\ntoken\n",
            importing = false,
            onFailure = { step, _ -> failed.add(step) },
            readAsset = { "asset".toByteArray() },
        )

        assertFalse(prep.browserReady)
        assertEquals(listOf("/var/cache/appine/doas.apk", BrowserBridge.CONFIG_PATH), failed)
        assertTrue("/usr/local/libexec/appine-open-url" in store.files)
        assertTrue("/usr/local/bin/xdg-open" in store.files)
    }
}
