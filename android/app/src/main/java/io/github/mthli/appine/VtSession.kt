package io.github.mthli.appine

import android.os.Handler
import android.os.Looper
import java.io.IOException
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.RejectedExecutionException
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * A tab on the libghostty-vt terminal (the android/terminal map), owned by [TerminalService]. The
 * terminal lives in the native library and takes the console's output on a pump thread whether or
 * not a [VtView] shows it ([view]); the shell starts when a view first knows the grid ([start]),
 * and until then restored text and messages wait. Input goes out on the tab's own writer thread, so
 * a program that stops reading blocks only its tab.
 */
internal class VtSession(
    private val pty: ConsolePty,
    private val machine: Long,
    private val events: Events,
) : Session {

    override val console: Int get() = pty.console
    override val login: String get() = pty.login

    /** What the service hears about (main thread). */
    interface Events {
        fun onTitleChanged(session: VtSession)

        fun onFinished(session: VtSession)
    }

    /** The terminal ([VtNative]), 0 until [start] created it. */
    @Volatile
    var tab = 0L
        private set

    /** The view showing this tab, if any; read on the pump thread. */
    @Volatile
    var view: VtView? = null

    private val main = Handler(Looper.getMainLooper())
    private var started = false
    private var released = false
    private var exit: Int? = null
    private val hungUp = AtomicBoolean()
    private var writer: ExecutorService? = null

    /** Restored text and messages to show before the shell's output. */
    private var pending: ByteArray? = null

    // The grid the terminal and the console have
    private var cols = 0
    private var rows = 0
    private var cellWidth = 0
    private var cellHeight = 0

    /** The last transcript of the primary screen, kept for saves while a full-screen program runs. */
    private var lastTranscript: String? = null

    override val title: String? get() = if (tab == 0L) null else VtNative.title(tab)

    override val isRunning: Boolean get() = exit == null

    override val transcriptText: String?
        get() {
            if (tab == 0L) return null
            VtNative.transcript(tab)?.let {
                lastTranscript = it
                return it
            }
            // The alternate screen (vim, less): what the tab showed before it, not the program's
            // screen without scrollback
            return lastTranscript.orEmpty()
        }

    override fun setRestored(text: ByteArray) {
        pending = text
    }

    override fun showMessage(text: String): Boolean {
        if (tab == 0L) {
            if (started) return false
            pending = (pending ?: ByteArray(0)) + "\u001b[33m$text\u001b[0m\r\n".toByteArray()
            return true
        }
        val shown = VtNative.message(tab, text)
        if (shown) view?.invalidate()
        return shown
    }

    /**
     * Opens the console with a grid of [cols] × [rows], creates the terminal, shows what was
     * pending and starts the shell with its pump, writer and waiter threads. Only the first call
     * does anything.
     */
    fun start(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
        if (started || released || hungUp.get()) return
        started = true
        val error = try {
            pty.start(cols, rows)
            null
        } catch (e: IOException) {
            e.message ?: e.toString()
        }
        val t = try {
            VtNative.create(machine, pty.console, cols, rows, cellWidth, cellHeight)
        } catch (e: IOException) {
            0L
        }
        if (t == 0L) {
            // The kernel is gone: the service is about to release the tab
            hangUp()
            exited(127)
            return
        }
        tab = t
        this.cols = cols
        this.rows = rows
        this.cellWidth = cellWidth
        this.cellHeight = cellHeight
        pending?.let { VtNative.print(t, it) }
        pending = null
        if (error != null) {
            // As in a Termux tab: show why and treat the tab as ended. The console opened before
            // spawning failed is closed here
            VtNative.print(t, "\r\n[$error]\r\n".toByteArray())
            hangUp()
            exit = 127
            view?.onStarted()
            events.onFinished(this)
            return
        }
        writer = Executors.newSingleThreadExecutor { Thread(it, "vt-writer") }
        val pump = thread(name = "vt-pump") { VtNative.pump(t, this) }
        thread(name = "vt-waiter") {
            val code = pty.waitFor()
            // Output from before the exit may not have been taken yet: wait a little for the pump
            // (a background job may keep the console open, so not without limit)
            pump.join(500)
            main.post { exited(code) }
        }
        view?.onStarted()
    }

    /** The grid changed: the terminal reflows and the console's foreground group gets SIGWINCH. */
    fun resize(cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
        if (tab == 0L) return
        if (cols == this.cols && rows == this.rows && cellWidth == this.cellWidth && cellHeight == this.cellHeight) return
        this.cols = cols
        this.rows = rows
        this.cellWidth = cellWidth
        this.cellHeight = cellHeight
        VtNative.resize(tab, cols, rows, cellWidth, cellHeight)
    }

    /** Input for the program, written on the tab's writer thread; dropped once the shell exited. */
    fun write(data: ByteArray) {
        val w = writer ?: return
        if (!isRunning) return
        val console = pty.console
        try {
            w.execute { Native.write(machine, console, data, 0, data.size) }
        } catch (_: RejectedExecutionException) {
            // Released meanwhile
        }
    }

    override fun finishIfRunning() {
        if (isRunning) hangUp()
    }

    override fun release() {
        if (released) return
        released = true
        finishIfRunning()
        writer?.shutdown()
        // A running pump keeps the terminal until it returns
        if (tab != 0L) VtNative.destroy(tab)
    }

    /** Closes the console once, so a later close cannot hang up a tab that reused its number. */
    private fun hangUp() {
        if (hungUp.compareAndSet(false, true)) pty.close()
    }

    private fun exited(code: Int) {
        if (exit != null) return
        exit = code
        hangUp()
        writer?.shutdown()
        if (released) return
        if (tab != 0L) {
            val reason = when {
                code > 0 -> " (code $code)"
                code < 0 -> " (signal ${-code})"
                else -> ""
            }
            VtNative.print(tab, "\r\n[Process completed$reason - press Enter]".toByteArray())
            view?.invalidate()
        }
        events.onFinished(this)
    }

    /** From vt.rs, on the pump thread: the screen changed since the last frame. */
    @Suppress("unused")
    fun onOutput() {
        view?.postInvalidateOnAnimation()
    }

    /** From vt.rs, on the pump thread: a program set the title. */
    @Suppress("unused")
    fun onTitle() {
        main.post { if (!released) events.onTitleChanged(this) }
    }
}
