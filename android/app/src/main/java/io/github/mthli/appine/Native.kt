package io.github.mthli.appine

import java.io.IOException

/**
 * 原生库 libappine.so 的 JNI 接口，实现在 hosts/android/src/lib.rs。
 *
 * `machine` 是 [start] 返回的句柄；[stop] 之后不能再用。阻塞的调用（[read]、[write]、
 * [waitFor]）只能在后台线程上进行。
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

    /** 启动内核；[tier] 0 是解释器，1 是 DBT。 */
    @JvmStatic
    @Throws(IOException::class)
    external fun start(store: String, tier: Int): Long

    /** 结束所有 guest 进程并释放句柄。 */
    @JvmStatic
    external fun stop(machine: Long)

    /** 新建一个控制台（进程内 PTY），返回它的编号。 */
    @JvmStatic
    @Throws(IOException::class)
    external fun openConsole(machine: Long, rows: Int, cols: Int): Int

    /** 以控制台为控制终端启动一个会话组长，返回 pid。 */
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
