package io.github.mthli.appine

import com.termux.terminal.TerminalSession
import java.io.IOException

/** 一个终端标签页的 PTY：内核里的一个控制台，上面跑一个登录 shell。 */
internal class ConsolePty(private val machine: Long) : TerminalSession.Pty {
    private var console = -1
    private var pid = -1

    override fun start(columns: Int, rows: Int): Int {
        console = Native.openConsole(machine, rows, columns)
        // argv[0] 以 - 开头：ash 作为登录 shell 运行，读 /etc/profile
        pid = Native.spawn(machine, console, "/bin/sh", arrayOf("-sh"), ENVIRONMENT)
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
        val ENVIRONMENT = arrayOf(
            "HOME=/root",
            "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            "TERM=xterm-256color",
            "LANG=C.UTF-8",
            "SHELL=/bin/sh",
            "USER=root",
            "LOGNAME=root",
        )
    }
}
