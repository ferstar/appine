package io.github.mthli.appine

import android.content.Context
import com.termux.terminal.TerminalSession
import java.io.IOException

/**
 * The PTY of one terminal tab: a console in the kernel with a login shell running on it. [login] is
 * the login account from Settings (`auto`, `root` or an account name), from which the assets'
 * `login.sh`, running as root, decides whom to log in (the android/setup map); [cwd] is the
 * directory to start in (tabs restored after the app was killed, the android/service map), null for
 * the home directory.
 */
internal class ConsolePty(
    private val machine: Long,
    private val script: String,
    val login: String,
    private val cwd: String? = null,
) : TerminalSession.Pty {
    /** The console number in the kernel, -1 before the shell starts. */
    @Volatile
    var console = -1
        private set
    private var pid = -1

    override fun start(columns: Int, rows: Int): Int {
        console = Native.openConsole(machine, rows, columns)
        pid = Native.spawn(machine, console, "/bin/sh", argv(script, login, cwd), ENVIRONMENT)
        return pid
    }

    override fun read(buffer: ByteArray): Int {
        val n = Native.read(machine, console, buffer)
        return if (n > 0) n else -1
    }

    override fun write(buffer: ByteArray, offset: Int, count: Int) {
        if (Native.write(machine, console, buffer, offset, count) < 0) {
            throw IOException("the terminal is closed")
        }
    }

    override fun resize(columns: Int, rows: Int) = Native.resize(machine, console, rows, columns)

    override fun waitFor(): Int = Native.waitFor(machine, pid)

    override fun close() {
        if (console >= 0) Native.closeConsole(machine, console)
    }

    companion object {
        /**
         * Login account in Settings: automatic (the first regular user created, or root when there
         * is none).
         */
        const val LOGIN_AUTO = "auto"
        const val LOGIN_ROOT = "root"

        @Volatile
        private var cached: String? = null

        /**
         * The login script (the assets' `login.sh`): chooses the account; when a regular user first
         * becomes the login account, adds it to the admin group and installs doas or sudo, then
         * `su -l` to it; root logs in with the shell in `/etc/passwd` (also after chsh changed it).
         */
        fun script(context: Context): String = cached
            ?: context.assets.open("login.sh").bufferedReader().readText().also { cached = it }

        /**
         * The argv for running the login script as root
         * (`sh -c script appine-login account directory`).
         */
        fun argv(script: String, login: String, cwd: String? = null) =
            arrayOf("sh", "-c", script, "appine-login", login, cwd.orEmpty())

        val ENVIRONMENT = arrayOf(
            "HOME=/root",
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "TERM=xterm-256color",
            "LANG=C.UTF-8",
            "SHELL=/bin/sh",
            "USER=root",
            "LOGNAME=root",
            // Run as root, nmap does scans that need raw sockets by default, which the app cannot
            // create (like root without CAP_NET_RAW), so nmap just exits; with this set it uses
            // connect scans instead
            "NMAP_UNPRIVILEGED=1",
        )
    }
}
