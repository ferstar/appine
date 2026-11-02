package io.github.mthli.appine

import java.io.IOException

/**
 * 原生库 libappine.so 的 JNI 接口，实现在 hosts/android/src/lib.rs。
 *
 * `machine` 是 [start] 返回的句柄。[stop] 之后再用它也安全：读写返回 -1，[waitFor] 返回
 * -9（进程都被 SIGKILL 结束了），新开控制台、起进程抛出异常，别的什么也不做。阻塞的调用
 * （[read]、[write]、[waitFor]）只能在后台线程上进行。
 */
internal object Native {
    init {
        System.loadLibrary("appine")
        init(BuildConfig.DEBUG)
    }

    /** stderr 转到 logcat；[verbose] 时打开内核层的调试日志。由 init 块调用。 */
    @JvmStatic
    private external fun init(verbose: Boolean)

    /** 在空目录 [store] 新建 inode 仓库，导入 rootfs 的 tar.gz。 */
    @JvmStatic
    @Throws(IOException::class)
    external fun importRootfs(store: String, tarball: String)

    /** 在仓库里写一个文件；[overwrite] 为 false 时已经存在就不动。 */
    @JvmStatic
    @Throws(IOException::class)
    external fun writeFile(store: String, path: String, data: ByteArray, mode: Int, overwrite: Boolean)

    /** 读仓库里的一个普通文件，不存在时是 null；内核运行着也可以读。 */
    @JvmStatic
    @Throws(IOException::class)
    external fun readFile(store: String, path: String): ByteArray?

    /** 启动内核；[tier] 0 是解释器，1 是 DBT。 */
    @JvmStatic
    @Throws(IOException::class)
    external fun start(store: String, tier: Int): Long

    /** 结束所有 guest 进程并释放句柄。 */
    @JvmStatic
    external fun stop(machine: Long)

    /**
     * 按 `/etc/inittab` 开机（设计文档 §6，Alpine 上是 OpenRC）：阻塞到 sysinit、wait 的命令都
     * 结束，输出在 guest 的 `/var/log/boot.log`。不在主线程上调用。
     */
    @JvmStatic
    external fun boot(machine: Long)

    /**
     * 关机：运行 inittab 里关机的命令（最多 30 秒），给所有进程 SIGTERM、最多等 1 秒，再结束
     * 剩下的，然后释放句柄。不在主线程上调用。
     */
    @JvmStatic
    external fun shutdown(machine: Long)

    /** 取走 guest 的关机请求（busybox 的 reboot、halt、poweroff）：[POWER_NONE] 等。 */
    @JvmStatic
    external fun takePower(machine: Long): Int

    /** 还在运行的 guest 进程数（不算 init）。 */
    @JvmStatic
    external fun processes(machine: Long): Int

    const val POWER_NONE = 0
    const val POWER_REBOOT = 1
    const val POWER_HALT = 2
    const val POWER_OFF = 3

    /** 丢掉可以重建的缓存（池里保留的翻译等），提交推迟的写事务。要取 BKL，不在主线程上调用。 */
    @JvmStatic
    external fun trimMemory(machine: Long)

    /**
     * 宿主的默认网络变了：guest 的路由表换成 [routes]，每一项是
     * `<目标网段的十六进制>/<前缀长度> <网关的十六进制或者 -> <接口>`；null 时改回按接口的
     * 地址合成。
     */
    @JvmStatic
    external fun setRoutes(machine: Long, routes: Array<String>?)

    /**
     * 取走 guest 第一次在非回环地址上监听的端口（设计文档 §12 #7）：每一项是
     * `<进程名>\t<端口>\t<宿主上的端口>`。1024 以下的端口在宿主上换成了别的端口，别的设备
     * 要连宿主上的那个。要取 BKL，不在主线程上调用。
     */
    @JvmStatic
    external fun takeListens(machine: Long): Array<String>?

    /** 新建一个控制台（进程内 PTY），返回它的编号。 */
    @JvmStatic
    @Throws(IOException::class)
    external fun openConsole(machine: Long, rows: Int, cols: Int): Int

    /** 以控制台为控制终端启动一个会话组长，返回 pid。[console] 是负数时不要控制台（stdio 是 App 的）。 */
    @JvmStatic
    @Throws(IOException::class)
    external fun spawn(machine: Long, console: Int, program: String, argv: Array<String>, envp: Array<String>): Int

    /** 读控制台的输出：返回字节数，0 表示会话结束，-1 表示控制台已经关闭。 */
    @JvmStatic
    external fun read(machine: Long, console: Int, buffer: ByteArray): Int

    /** 向控制台输入：返回写入的字节数，-1 表示会话已经结束。 */
    @JvmStatic
    external fun write(machine: Long, console: Int, buffer: ByteArray, offset: Int, length: Int): Int

    @JvmStatic
    external fun resize(machine: Long, console: Int, rows: Int, cols: Int)

    /** 关闭控制台：会话组长收到 SIGHUP。 */
    @JvmStatic
    external fun closeConsole(machine: Long, console: Int)

    /** 等进程结束：返回退出码，被信号终止时返回负的信号编号。 */
    @JvmStatic
    external fun waitFor(machine: Long, pid: Int): Int
}
