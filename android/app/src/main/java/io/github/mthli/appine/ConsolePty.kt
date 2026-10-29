package io.github.mthli.appine

import com.termux.terminal.TerminalSession
import java.io.IOException

/** 一个终端标签页的 PTY：内核里的一个控制台，上面跑一个登录 shell。 */
internal class ConsolePty(private val machine: Long) : TerminalSession.Pty {
    private var console = -1
    private var pid = -1

    override fun start(columns: Int, rows: Int): Int {
        console = Native.openConsole(machine, rows, columns)
        pid = Native.spawn(machine, console, "/bin/sh", arrayOf("sh", "-c", LOGIN), ENVIRONMENT)
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
         * 以 `/etc/passwd` 里 root 的 shell 作为登录 shell 运行（读 /etc/profile）：Alpine 是 ash，
         * Debian、Ubuntu 是 bash，chsh 改过之后也照样。找不到时用 /bin/sh。passwd 从 fd 3 读：
         * 循环的重定向也作用于里面 exec 的 shell，它的 stdin 得是终端。
         */
        const val LOGIN = "while IFS=: read -r name _ _ _ _ _ shell <&3; do " +
            "if [ \"\$name\" = root ] && [ -x \"\$shell\" ]; then " +
            "export SHELL=\"\$shell\"; exec \"\$shell\" -l 3<&-; fi; " +
            "done 3< /etc/passwd; exec /bin/sh -l"

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
