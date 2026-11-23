package io.github.mthli.appine

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Canvas
import android.os.Build
import android.os.SystemClock
import android.view.ContextThemeWrapper
import android.view.View
import android.view.ViewGroup
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inspector.WindowInspector
import android.widget.TextView
import androidx.core.graphics.createBitmap
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Test
import org.junit.runner.RunWith
import java.io.ByteArrayOutputStream
import java.io.File
import java.lang.reflect.Proxy
import java.util.concurrent.CountDownLatch
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.ThreadPoolExecutor
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/** Exercises the real input connection and paste encoder without sending input to a shell. */
@RunWith(AndroidJUnit4::class)
class TerminalInputTest {
    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext

    private fun onMain(action: () -> Unit) {
        instrumentation.runOnMainSync(action)
        instrumentation.waitForIdleSync()
    }

    private class Host : VtView.Host {
        val bytes = ByteArrayOutputStream()
        override fun onInput(data: ByteArray) { bytes.write(data) }
        override fun onGrid(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {}
        override fun onZoom(fontPx: Int) {}
        override val ctrl = false
        override val alt = false
        override val shift = false
        override fun modifiersUsed() {}
        override fun onLink(uri: String) {}
        override fun onSearchCount(total: Int, selected: Int) {}
        fun text() = bytes.toString("UTF-8")
    }

    private data class Fixture(
        val view: VtView,
        val connection: InputConnection,
        val host: Host,
        val tab: Long,
        val session: VtSession,
        val statusChanges: AtomicInteger,
    )

    private fun fakeEvents(onStatusChanged: () -> Unit = {}): VtSession.Events = Proxy.newProxyInstance(
        VtSession.Events::class.java.classLoader, arrayOf(VtSession.Events::class.java),
    ) { _, method, _ ->
        if (method.name == "onStatusChanged") onStatusChanged()
        null
    } as VtSession.Events

    private fun field(clazz: Class<*>, name: String) =
        clazz.getDeclaredField(name).apply { isAccessible = true }

    private fun withConnection(test: (InputConnection, Host, Long) -> Unit) =
        withFixture { test(it.connection, it.host, it.tab) }

    private fun withFixture(test: (Fixture) -> Unit) {
        val root = File(context.cacheDir, "terminal-input-test").apply { mkdirs() }
        val tar = File(root, "rootfs.tgz")
        context.assets.open("rootfs.tgz").use { input -> tar.outputStream().use(input::copyTo) }
        Native.importRootfs(File(root, "store").path, tar.path)
        val machine = Native.start(File(root, "store").path, 1, false)
        val console = Native.openConsole(machine, 24, 80)
        val tab = VtNative.create(machine, console, 80, 24, 8, 16)
        try {
            ActivityScenario.launch<LicensesActivity>(Intent(context, LicensesActivity::class.java)).use { scenario ->
                lateinit var view: VtView
                lateinit var connection: InputConnection
                lateinit var session: VtSession
                val host = Host()
                val statusChanges = AtomicInteger()
                scenario.onActivity { activity ->
                    assertFalse("Tests require an unlocked page", Privacy.locked(activity))
                    session = VtSession(1, ConsolePty(machine, "", "root"), machine, fakeEvents { statusChanges.incrementAndGet() })
                    // Supply only the terminal; starting a session would also start shell threads.
                    field(VtSession::class.java, "tab").setLong(session, tab)
                    view = VtView(ContextThemeWrapper(activity, R.style.Theme_Appine_Terminal), host)
                    view.attach(session)
                    connection = view.onCreateInputConnection(EditorInfo())
                }
                try {
                    // Clipboard reads require window focus, which can follow ActivityScenario's resume.
                    val deadline = SystemClock.uptimeMillis() + 5000
                    var focused = false
                    while (!focused && SystemClock.uptimeMillis() < deadline) {
                        scenario.onActivity { focused = it.hasWindowFocus() }
                        if (!focused) SystemClock.sleep(20)
                    }
                    assertTrue("The test page must have window focus", focused)
                    test(Fixture(view, connection, host, tab, session, statusChanges))
                } finally {
                    onMain {
                        button(android.R.id.button2)?.performClick()
                        view.attach(null)
                    }
                }
            }
        } finally {
            VtNative.destroy(tab)
            Native.closeConsole(machine, console)
            Native.stop(machine)
            root.deleteRecursively()
        }
    }

    private fun button(id: Int): View? = WindowInspector.getGlobalWindowViews()
        .firstNotNullOfOrNull { it.findViewById(id) }

    private fun hasPreview(): Boolean {
        fun contains(view: View): Boolean =
            (view is TextView && view.text == context.getString(R.string.paste_unsafe_title)) ||
                (view is ViewGroup && (0 until view.childCount).any { contains(view.getChildAt(it)) })
        return WindowInspector.getGlobalWindowViews().any(::contains)
    }

    @Test
    fun removingTrailingNewlinesPreservesOtherWhitespaceAndDoesNotReplay() {
        withConnection { connection, host, _ ->
            onMain {
                connection.commitText("  printf hello \t\r\n\r\n", 1)
                assertTrue(hasPreview())
                assertEquals("", host.text())
                assertTrue("A safe alternative must be offered", button(android.R.id.button3)?.visibility == View.VISIBLE)
            }
            if (InstrumentationRegistry.getArguments().getString("screenshots") == "true") {
                SystemClock.sleep(500) // Let the dialog's window animation finish before visual inspection.
                val screenshot = instrumentation.uiAutomation.takeScreenshot()!!
                File(context.cacheDir, "paste-preview.png").outputStream().use {
                    screenshot.compress(Bitmap.CompressFormat.PNG, 100, it)
                }
                screenshot.recycle()
            }
            onMain { button(android.R.id.button3)!!.performClick() }
            onMain {
                connection.finishComposingText()
                assertEquals("  printf hello \t", host.text())
                assertFalse(hasPreview())
            }
        }
    }

    @Test
    fun removingTrailingNewlinesIsNotOfferedForUnsafeOrEmptyRemainders() {
        withConnection { connection, host, _ ->
            for (text in listOf("first\nsecond\n", "first\u001b[201~\n", "\r\n\r\n")) {
                onMain {
                    connection.commitText(text, 1)
                    assertTrue(hasPreview())
                    assertFalse("Unsafe or empty text must not be offered", button(android.R.id.button3)?.visibility == View.VISIBLE)
                    assertEquals("", host.text())
                    button(android.R.id.button2)!!.performClick()
                }
            }
        }
    }

    @Test
    fun removingTrailingNewlinesUsesCurrentPasteMode() {
        withConnection { connection, host, tab ->
            onMain {
                connection.commitText("hello\n", 1)
                assertTrue(hasPreview())
                assertTrue(button(android.R.id.button3)?.visibility == View.VISIBLE)
                VtNative.print(tab, "\u001b[?2004h".toByteArray())
                button(android.R.id.button3)!!.performClick()
            }
            assertEquals("\u001b[200~hello\u001b[201~", host.text())
        }
    }

    @Test
    fun replacementIsDeliveredImmediatelyAndNotReplayedOnFinish() {
        assumeTrue(Build.VERSION.SDK_INT >= 34)
        withConnection { connection, host, _ ->
            onMain {
                assertTrue(connection.replaceText(0, 0, "first", 1, null))
                assertEquals("first", host.text())
                assertEquals("", connection.getTextBeforeCursor(100, 0).toString())
                connection.finishComposingText()
                assertEquals("first", host.text())
                connection.replaceText(0, 0, "second", 1, null)
                connection.finishComposingText()
                assertEquals("firstsecond", host.text())
            }
        }
    }

    @Test
    fun replacementRequiresConfirmationBeforeSendingLineBreaks() {
        assumeTrue(Build.VERSION.SDK_INT >= 34)
        withConnection { connection, host, _ ->
            onMain {
                connection.replaceText(0, 0, "first\nsecond", 1, null)
                assertTrue("Replacement must show the paste preview", hasPreview())
                assertEquals("", host.text())
                connection.finishComposingText()
                assertEquals("", host.text())
                button(android.R.id.button1)!!.performClick()
            }
            assertEquals("first\rsecond", host.text())
        }
    }

    @Test
    fun finishingCompositionUsesPasteSafetyAndCancellationDiscardsIt() {
        withConnection { connection, host, _ ->
            onMain {
                connection.setComposingText("first\nsecond", 1)
                connection.finishComposingText()
                assertTrue("Finishing composition must show the paste preview", hasPreview())
                assertEquals("", host.text())
                button(android.R.id.button2)!!.performClick()
            }
            onMain {
                connection.finishComposingText()
                connection.commitText("ok", 1)
                assertEquals("ok", host.text())
            }
        }
    }

    @Test
    fun imePasteMenuUsesClipboardSafety() {
        withConnection { connection, host, _ ->
            val clipboard = context.getSystemService(ClipboardManager::class.java)
            val previous = clipboard.primaryClip
            try {
                onMain {
                    clipboard.setPrimaryClip(ClipData.newPlainText("Test", "first\nsecond"))
                    assertTrue(connection.performContextMenuAction(android.R.id.paste))
                    assertTrue("IME paste must show the paste preview", hasPreview())
                    assertEquals("", host.text())
                    button(android.R.id.button2)!!.performClick()
                }
                onMain {
                    connection.finishComposingText()
                    assertEquals("", host.text())
                }
            } finally {
                if (previous == null) clipboard.clearPrimaryClip() else clipboard.setPrimaryClip(previous)
            }
        }
    }

    @Test
    fun ordinaryCompositionAndEnterStillTypeOnce() {
        withConnection { connection, host, _ ->
            onMain {
                connection.setComposingText("ni", 1)
                assertEquals("", host.text())
                connection.commitText("你", 1)
                connection.finishComposingText()
                connection.setComposingText("好", 1)
                connection.finishComposingText()
                connection.finishComposingText()
                connection.commitText("\n", 1)
                assertEquals("你好\r", host.text())
                assertFalse(hasPreview())
            }
        }
    }

    @Test
    fun bracketedPasteAndItsTerminatorUseTheSameChecksForComposition() {
        withConnection { connection, host, tab ->
            onMain {
                VtNative.print(tab, "\u001b[?2004h".toByteArray())
                connection.setComposingText("first\nsecond", 1)
                connection.finishComposingText()
                assertEquals("\u001b[200~first\nsecond\u001b[201~", host.text())
                assertFalse(hasPreview())
                host.bytes.reset()
                connection.setComposingText("\u001b[201~", 1)
                connection.finishComposingText()
                assertTrue("The bracketed paste terminator must require confirmation", hasPreview())
                assertEquals("", host.text())
            }
        }
    }

    @Test
    fun composingDeleteSurroundingTextUpdatesPreedit() {
        withFixture { f ->
            val preeditField = field(VtView::class.java, "preedit")
            onMain {
                f.connection.setComposingText("abc", 1)
                assertEquals("abc", preeditField.get(f.view))
                f.connection.deleteSurroundingText(1, 0)
                assertEquals("ab", preeditField.get(f.view))
                f.connection.deleteSurroundingTextInCodePoints(1, 0)
                assertEquals("a", preeditField.get(f.view))
                f.connection.deleteSurroundingText(1, 0)
                assertEquals("", preeditField.get(f.view))
                assertEquals("", f.host.text())
            }
        }
    }

    @Test
    fun attachClearsPreeditAndStopsBlink() {
        withFixture { f ->
            val preeditField = field(VtView::class.java, "preedit")
            val blinkField = field(VtView::class.java, "blinkRunning")
            onMain {
                f.connection.setComposingText("hello", 1)
                blinkField.setBoolean(f.view, true)
                assertEquals("hello", preeditField.get(f.view))
                f.view.attach(null)
                assertEquals("", preeditField.get(f.view))
                assertFalse("Detaching must stop cursor blink", blinkField.getBoolean(f.view))
            }
        }
    }

    @Test
    fun mode1004FocusEventsDoNotJumpScrolledBackViewport() {
        withFixture { f ->
            val atBottomField = field(VtView::class.java, "atBottom")
            onMain {
                f.view.layout(0, 0, 640, 384)
                VtNative.print(f.tab, ("\u001b[?1004h" + (1..60).joinToString("\r\n") { "line $it" }).toByteArray())
                val bitmap = createBitmap(f.view.width, f.view.height)
                try {
                    f.view.draw(Canvas(bitmap))
                    VtNative.scroll(f.tab, -5)
                    f.view.draw(Canvas(bitmap))
                    assertFalse("Viewport must be scrolled back before focus events", atBottomField.getBoolean(f.view))

                    f.view.onWindowFocusChanged(false)
                    f.view.onWindowFocusChanged(true)
                    assertEquals("\u001b[O\u001b[I", f.host.text())

                    f.view.draw(Canvas(bitmap))
                    assertFalse("Focus events must not jump viewport to bottom", atBottomField.getBoolean(f.view))
                } finally {
                    bitmap.recycle()
                }
            }
        }
    }

    @Test
    fun sessionResetClearsProgressAndStatus() {
        withFixture { f ->
            onMain {
                f.session.onProgress(VtSession.PROGRESS_SET, 40)
                f.session.onStatus(VtSession.STATUS_WORKING, VtSession.KIND_NONE, 40, "job", "Build", "Running")
            }
            onMain {
                assertEquals(VtSession.Progress(VtSession.PROGRESS_SET, 40), f.session.progress)
                assertEquals("job", f.session.status?.id)
                val before = f.statusChanges.get()
                f.session.reset()
                assertNull(f.session.progress)
                assertNull(f.session.status)
                assertTrue("Reset must notify status change when clearing progress/status", f.statusChanges.get() > before)
            }
        }
    }

    @Test
    fun sessionHangUpDropsQueuedWrites() {
        val blockerStarted = CountDownLatch(1)
        val releaseBlocker = CountDownLatch(1)
        val executor = ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS, LinkedBlockingQueue())
        val pty = ConsolePty(0L, "", "root")
        field(ConsolePty::class.java, "console").setInt(pty, 0)
        val session = VtSession(1, pty, 0L, fakeEvents())
        field(VtSession::class.java, "writer").set(session, executor)
        try {
            executor.execute {
                blockerStarted.countDown()
                try {
                    releaseBlocker.await(5, TimeUnit.SECONDS)
                } catch (_: InterruptedException) {
                    // shutdownNow interrupts the running task while dropping queued writes.
                }
            }
            assertTrue(blockerStarted.await(5, TimeUnit.SECONDS))
            session.write("queued".toByteArray())
            assertEquals(1, executor.queue.size)
            session.finishIfRunning()
            assertEquals("Queued writes must be cancelled on hangup", 0, executor.queue.size)
            assertTrue(executor.isShutdown)
            releaseBlocker.countDown()
            assertTrue(executor.awaitTermination(5, TimeUnit.SECONDS))
            assertEquals(1L, executor.completedTaskCount)
        } finally {
            releaseBlocker.countDown()
            executor.shutdownNow()
        }
    }
}
