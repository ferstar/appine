package io.github.mthli.appine

import com.termux.terminal.TerminalSession

/**
 * A terminal tab as [TerminalService] holds it (the android/service map): a login shell on a
 * kernel console ([ConsolePty]) and the emulator that shows it, either Termux's ([TermuxSession])
 * or libghostty-vt ([VtSession], when the native library has the `vt` feature). Calls are on the
 * main thread, except [console] and [login], which saving reads on the service's worker.
 */
interface Session {
    /** The kernel console, -1 before the shell starts. */
    val console: Int

    /** The login account setting the tab was opened with ([ConsolePty.login]). */
    val login: String

    /** The title a program set, null when none (the tab bar then shows `sh`). */
    val title: String?

    /** True until the shell has exited (also before it starts). */
    val isRunning: Boolean

    /**
     * Plain text of the screen and its scrollback for saving the tab; null while the shell has not
     * started (a restored tab not shown yet).
     */
    val transcriptText: String?

    /** A restored tab: [text] (a terminal byte stream) is shown before the shell's output. */
    fun setRestored(text: ByteArray)

    /**
     * Shows a yellow line of app notice above the cursor's line (a kill report); kept until the
     * emulator exists; false in a full-screen program, where nothing is shown.
     */
    fun showMessage(text: String): Boolean

    /** Hangs up the console when the shell still runs: it gets SIGHUP. */
    fun finishIfRunning()

    /** The tab is closed or its kernel goes away: hang up and free the emulator. */
    fun release()
}

/** A tab on the vendored Termux emulator, shown by its `TerminalView`. */
internal class TermuxSession(val terminal: TerminalSession, private val pty: ConsolePty) : Session {
    override val console: Int get() = pty.console
    override val login: String get() = pty.login
    override val title: String? get() = terminal.title
    override val isRunning: Boolean get() = terminal.isRunning
    override val transcriptText: String? get() = terminal.transcriptText
    override fun setRestored(text: ByteArray) = terminal.setRestored(text)
    override fun showMessage(text: String): Boolean = terminal.showMessage(text)
    override fun finishIfRunning() = terminal.finishIfRunning()
    override fun release() = terminal.finishIfRunning()
}
