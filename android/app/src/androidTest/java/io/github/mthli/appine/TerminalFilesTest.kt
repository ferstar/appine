package io.github.mthli.appine

import android.content.Context
import android.content.Intent
import android.database.ContentObserver
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.provider.DocumentsContract
import androidx.core.content.edit
import androidx.lifecycle.ViewModelProvider
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import java.io.ByteArrayInputStream
import java.io.File
import java.io.FileInputStream
import java.util.concurrent.CancellationException
import java.util.concurrent.ExecutionException
import java.util.concurrent.FutureTask
import kotlin.concurrent.read
import java.io.IOException
import java.io.InputStream
import java.util.UUID
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** Real store copies, including failures, with no dependency on the app's running rootfs. */
@RunWith(AndroidJUnit4::class)
class TerminalFilesTest {
    private val context = InstrumentationRegistry.getInstrumentation().targetContext
    private val root = File(context.cacheDir, "file-import-test-${UUID.randomUUID()}")
    private val store = File(root, "store").path
    private val account = Rootfs.Account("root", 0, 0, "/root")

    @Before
    fun prepare() {
        root.mkdirs()
        val tar = File(root, "rootfs.tgz")
        context.assets.open("rootfs.tgz").use { input -> tar.outputStream().use(input::copyTo) }
        Native.importRootfs(store, tar.path)
    }

    @After
    fun cleanUp() { root.deleteRecursively() }

    @Test
    fun copiesBinaryFilesAndNumbersCollisionsWithoutOverwriting() {
        val bytes = ByteArray(256 * 1024 + 37) { it.toByte() }
        val first = TerminalFiles.copyIntoHome(store, account, "资料 100%.bin") { bytes.inputStream() }
        val second = TerminalFiles.copyIntoHome(store, account, "资料 100%.bin") { byteArrayOf(9).inputStream() }
        assertEquals("/root/资料 100%.bin", first)
        assertEquals("/root/资料 100% (1).bin", second)
        assertArrayEquals(bytes, Native.readFile(store, first))
        assertArrayEquals(byteArrayOf(9), Native.readFile(store, second))
    }

    @Test
    fun copyIntoHomeNotifiesDocumentsProvider() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        val previous = prefs.getBoolean("file_access", false)
        Settings.setPrivacyMode(context, false)
        Settings.setFileAccess(context, true)
        val generation = Native.browseGenerations(store, arrayOf("/root")).single()
        val id = if (generation == 0L) "/root" else "${generation}/root"
        val uri = DocumentsContract.buildChildDocumentsUri(Documents.authority(context), id)
        val notified = CountDownLatch(1)
        val observer = object : ContentObserver(Handler(Looper.getMainLooper())) {
            override fun onChange(selfChange: Boolean) { notified.countDown() }
        }
        context.contentResolver.registerContentObserver(uri, false, observer)
        try {
            val imported = TerminalFiles.copyIntoHome(store, account, "notify.txt", context) { "hello".toByteArray().inputStream() }
            assertEquals("/root/notify.txt", imported)
            assertTrue("Expected Files app child listing notification for /root", notified.await(5, TimeUnit.SECONDS))
        } finally {
            context.contentResolver.unregisterContentObserver(observer)
            Settings.setPrivacyMode(context, privacy)
            Settings.setFileAccess(context, previous)
        }
    }

    @Test
    fun publishesAfterSourceClosesAndKeepsAConcurrentFile() {
        val path = "/root/race.txt"
        val original = byteArrayOf(7)
        val imported = byteArrayOf(1, 2, 3)
        val input = object : ByteArrayInputStream(imported) {
            override fun close() {
                assertNull(Native.readFile(store, path))
                Native.writeFile(store, path, original, 420, true)
                super.close()
            }
        }
        val result = TerminalFiles.copyIntoHome(store, account, "race.txt") { input }
        assertEquals("/root/race (1).txt", result)
        assertArrayEquals(original, Native.readFile(store, path))
        assertArrayEquals(imported, Native.readFile(store, result))
    }

    @Test
    fun failedStreamsRemoveThePartialCopyAndKeepExistingFiles() {
        Native.writeFile(store, "/root/keep.txt", "original".toByteArray(), 420, true)
        val input = object : InputStream() {
            var reads = 0
            override fun read(): Int = throw IOException("Source disconnected")
            override fun read(bytes: ByteArray, offset: Int, length: Int): Int {
                if (reads++ > 0) throw IOException("Source disconnected")
                bytes[offset] = 42
                return 1
            }
        }
        assertThrows(IOException::class.java) {
            TerminalFiles.copyIntoHome(store, account, "keep.txt") { input }
        }
        assertArrayEquals("original".toByteArray(), Native.readFile(store, "/root/keep.txt"))
        assertNull(Native.readFile(store, "/root/keep (1).txt"))
    }

    @Test
    fun failedSourceCloseAlsoRemovesTheCopy() {
        val failure = IOException("Source close failed")
        val input = object : ByteArrayInputStream("data".toByteArray()) {
            override fun close() { throw failure }
        }
        val thrown = assertThrows(IOException::class.java) {
            TerminalFiles.copyIntoHome(store, account, "close-failed.txt") { input }
        }
        assertSame(failure, thrown)
        assertNull(Native.readFile(store, "/root/close-failed.txt"))
    }

    @Test
    fun sanitizesNamesAndSupportsEmptyFilesAndEscapedHomePaths() {
        Native.browseCreate(store, "/root", "/root", "100% home", true, 1000, 1000)
        val user = Rootfs.Account("user", 1000, 1000, "/root/100% home")
        val path = TerminalFiles.copyIntoHome(store, user, "../bad\nname\u001b.txt") { byteArrayOf().inputStream() }
        assertEquals("/root/100% home/.._bad_name_.txt", path)
        assertArrayEquals(byteArrayOf(), Native.readFile(store, path))
        assertFalse(path.contains('\n'))
    }

    @Test
    fun refusesMissingOrWholeSystemHomes() {
        for (home in listOf("/", "/missing-home")) {
            assertThrows(IOException::class.java) {
                TerminalFiles.copyIntoHome(store, account.copy(home = home), "file.txt") { byteArrayOf(1).inputStream() }
            }
        }
        assertNull(Native.readFile(store, "/file.txt"))
    }

    @Test
    fun guestCanReadImportedFilesAndClipboardPathsAreLiteralShellArguments() {
        assertEquals("/root/photo.png /root/中文.txt", TerminalFiles.clipboardPaths(listOf("/root/photo.png", "/root/中文.txt")))
        assertEquals("/root/my\\ photo.png", TerminalFiles.clipboardPaths(listOf("/root/my photo.png")))
        val machine = Native.start(store, 1, false)
        try {
            val name = "quote' and \$dollar.txt"
            val path = TerminalFiles.copyIntoHome(store, account, name) { "imported".toByteArray().inputStream() }
            val arguments = listOf(
                path, "/root/\$(touch /root/injected)", "/root/back`tick`", "/root/中文",
                "/root/*?[ab]{one,two}!(x)~#^&|;<>\\\"", "/root/tab\tname", "/root/trailing\\",
            )
            val command = "cat ${TerminalFiles.clipboardPaths(listOf(path))} > /root/content; " +
                "printf '%s\\n' ${TerminalFiles.clipboardPaths(arguments)} > /root/arguments"
            val pid = Native.spawn(machine, -1, "/bin/sh", arrayOf("sh", "-c", command), arrayOf("PATH=/bin:/usr/bin"))
            assertEquals(0, Native.waitFor(machine, pid))
            assertArrayEquals("imported".toByteArray(), Native.readFile(store, "/root/content"))
            assertArrayEquals((arguments.joinToString("\n") + "\n").toByteArray(), Native.readFile(store, "/root/arguments"))
            assertNull(Native.readFile(store, "/root/injected"))
        } finally {
            Native.stop(machine)
        }
    }

    @Test
    fun cancellationReleasesImportWhileProviderStillWaits() {
        val batch = ImportBatch()
        fun publish(input: FileInputStream, name: String) = Rootfs.access.read {
            TerminalFiles.copyIntoHome(store, account, name, check = batch::check) { input }
        }
        batch.stage(root, { output, _ -> output.write(42) }) { publish(it, "first") }
        val started = CountDownLatch(1)
        val release = CountDownLatch(1)
        val exited = CountDownLatch(1)
        val blocked = FutureTask {
            batch.stage(root, { _, _ ->
                started.countDown()
                try {
                    while (true) {
                        try { release.await(); break } catch (_: InterruptedException) { }
                    }
                } finally {
                    exited.countDown()
                }
            }) { publish(it, "second") }
        }
        Thread(blocked).start()
        try {
            assertTrue(started.await(5, TimeUnit.SECONDS))
            val writer = Rootfs.access.writeLock()
            assertTrue(writer.tryLock(5, TimeUnit.SECONDS))
            writer.unlock()
            batch.cancel()
            val error = assertThrows(ExecutionException::class.java) { blocked.get(5, TimeUnit.SECONDS) }
            assertTrue(error.cause is CancellationException)
            assertEquals(1L, exited.count)
            assertArrayEquals(byteArrayOf(42), Native.readFile(store, "/root/first"))
            assertNull(Native.readFile(store, "/root/second"))
        } finally {
            release.countDown()
            assertTrue(exited.await(5, TimeUnit.SECONDS))
        }
    }

    @Test
    fun pickerResultWaitsForUnlockAndSurvivesRecreation() {
        val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)
        val privacy = Settings.privacyMode(context)
        val fileAccess = prefs.getBoolean("file_access", false)
        Settings.setPrivacyMode(context, true)
        try {
            ActivityScenario.launch<MainActivity>(Intent(context, MainActivity::class.java)).use { scenario ->
                lateinit var files: TerminalFiles
                scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    files = ViewModelProvider(activity)[TerminalFiles::class.java]
                    assertTrue(files.begin("root"))
                    files.selected(listOf(Uri.parse("content://unavailable/import-test")))
                }
                scenario.recreate()
                scenario.onActivity { activity ->
                    assertTrue(Privacy.locked(activity))
                    assertSame(files, ViewModelProvider(activity)[TerminalFiles::class.java])
                    assertTrue(files.occupied)
                    assertFalse(files.copying)
                    assertNull(files.result)
                }
            }
        } finally {
            prefs.edit(commit = true) {
                putBoolean("privacy_mode", privacy)
                putBoolean("file_access", fileAccess)
            }
        }
    }
}
