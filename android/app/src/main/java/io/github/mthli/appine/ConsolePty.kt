package io.github.mthli.appine

import android.content.Context
import com.termux.terminal.TerminalSession
import java.io.IOException

/**
 * 一个终端标签页的 PTY：内核里的一个控制台，上面跑一个登录 shell。[login] 是设置里的登录账号
 * （`auto`、`root` 或账号名），由 assets 里的 `login.sh` 以 root 运行时决定登录谁（android/setup 的 map）；
 * [cwd] 是从哪个目录开始（App 被杀之后恢复的标签页，android/service 的 map），null 是家目录。
 */
internal class ConsolePty(
    private val machine: Long,
    private val script: String,
    val login: String,
    private val cwd: String? = null,
) : TerminalSession.Pty {
    /** 内核里的控制台编号，shell 启动之前是 -1。 */
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
        /** 设置里的登录账号：自动（最先建的普通用户，没有就是 root）。 */
        const val LOGIN_AUTO = "auto"
        const val LOGIN_ROOT = "root"

        @Volatile
        private var cached: String? = null

        /**
         * 登录脚本（assets 的 `login.sh`）：选账号，普通用户第一次成为登录账号时加进管理员组、装好
         * doas 或 sudo，然后 `su -l` 过去；root 以 `/etc/passwd` 里的 shell 登录（chsh 改过也照样）。
         */
        fun script(context: Context): String = cached
            ?: context.assets.open("login.sh").bufferedReader().readText().also { cached = it }

        /** 以 root 运行登录脚本的 argv（`sh -c 脚本 appine-login 账号 目录`）。 */
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
            // nmap 以 root 运行时默认做要 raw socket 的扫描，App 建不了这种 socket（与没有
            // CAP_NET_RAW 的 root 一样），nmap 直接退出；设了它就改用 connect 扫描
            "NMAP_UNPRIVILEGED=1",
        )
    }
}
