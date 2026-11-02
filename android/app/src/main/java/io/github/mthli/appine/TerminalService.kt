package io.github.mthli.appine

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.RouteInfo
import android.os.Binder
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import com.termux.terminal.TerminalSession
import com.termux.terminal.TerminalSessionClient
import java.util.concurrent.Executors
import kotlin.concurrent.thread

/**
 * 持有 guest 内核和所有终端会话的前台服务（设计文档 §11.2）。会话的生命周期挂在服务上，
 * 不挂在 Activity 上：Activity 关掉之后 shell 照常运行。通知栏显示会话数，并提供
 * "全部退出"。
 *
 * 内核启动之后按 `/etc/inittab` 开机（Alpine 上是 OpenRC 的服务），退出时先按它关机
 * （[exitAll]）。关掉最后一个标签页之后，还有 guest 进程（服务）在跑就留在后台，没有了才
 * 退出；guest 的 `poweroff`、`halt` 是全部退出，`reboot` 重启内核。
 */
class TerminalService : Service() {

    /** Activity 关心的事件。回调都在主线程上。 */
    interface Listener {
        /** 内核已经启动（或启动失败，[error] 不为 null）。 */
        fun onMachineReady(error: String?)

        /**
         * 还没有 rootfs（第一次运行、重置之后，或者上次准备失败，[error] 不为 null）：要用户
         * 选发行版。
         */
        fun onNeedDistro(error: String?)

        /** 准备 rootfs 的进度。 */
        fun onProgress(text: String)

        /** 会话列表、标题或状态变了。 */
        fun onSessionsChanged()

        /** 会话的屏幕内容变了。 */
        fun onTextChanged(session: TerminalSession)

        /** 全部退出：用户在通知里选了"全部退出"，或者 guest 关机了。 */
        fun onExitAll()

        /** guest 要求重启：会话都结束了，内核关机之后重新启动（之后是 [onMachineReady]）。 */
        fun onRestarting()
    }

    inner class LocalBinder : Binder() {
        val service: TerminalService get() = this@TerminalService
    }

    private val binder = LocalBinder()
    private val main = Handler(Looper.getMainLooper())

    /**
     * 释放缓存和停止内核按顺序在这个线程上做：两者都要取 BKL，不占主线程；停止之后也不会
     * 再有释放缓存的调用用到已经释放的句柄。
     */
    private val worker = Executors.newSingleThreadExecutor { Thread(it, "appine-worker") }

    /** 界面隐藏一段时间之后还没回来才释放缓存（[listener]）。 */
    private val trimWhenHidden = Runnable { trimMemory() }

    /**
     * 每隔 [POLL_MS] 在 [worker] 上问一次内核（要取 BKL）：
     * - guest 第一次在非回环地址上监听一个端口时发通知（设计文档 §12 #7）：别的设备连得上它，
     *   1024 以下的端口在宿主上换了一个，要告诉用户连哪个；
     * - guest 的关机请求（busybox 的 reboot、halt、poweroff）；
     * - 没有会话、界面也不在时还有没有 guest 进程：没有了就退出（设计文档 §11.2）。
     */
    private val poll = object : Runnable {
        override fun run() {
            val m = machine
            if (m == 0L) return
            runCatching {
                worker.execute {
                    val lines = Native.takeListens(m)
                    val power = Native.takePower(m)
                    val processes = Native.processes(m)
                    main.post {
                        if (machine != m) return@post
                        lines?.forEach(::notifyListen)
                        when (power) {
                            Native.POWER_REBOOT -> restart()
                            Native.POWER_HALT, Native.POWER_OFF -> exitAll()
                            else -> if (sessions.isEmpty() && listener == null && processes == 0) exitAll()
                        }
                    }
                }
            }
            main.postDelayed(this, POLL_MS)
        }
    }

    /** 正在关机退出（[exitAll]）。 */
    private var exiting = false

    /** 内核句柄，启动之前是 0。 */
    var machine = 0L
        private set

    /** 启动失败的原因。 */
    var startError: String? = null
        private set

    /** 在等用户选发行版（[install]）；[installError] 是上次准备失败的原因。 */
    var needsDistro = false
        private set
    var installError: String? = null
        private set

    /** 第一次运行时给镜像测速的线程：重置之前要等它写完源。 */
    @Volatile
    private var mirrorThread: Thread? = null

    /** 注册了 [network]。 */
    private var watchingNetwork = false

    val sessions = mutableListOf<TerminalSession>()

    /**
     * 默认网络的路由交给 guest（设计文档 §10）：Android 10 起 App 读不了宿主的路由表，guest
     * 的 `ip route`、`/proc/net/route` 靠它。回调在系统的线程上，交给 [worker] 按顺序做。
     */
    private val network = object : ConnectivityManager.NetworkCallback() {
        override fun onLinkPropertiesChanged(network: Network, lp: LinkProperties) {
            setRoutes(routes(lp))
        }

        override fun onLost(network: Network) {
            setRoutes(emptyArray())
        }
    }

    /**
     * 当前的 Activity；它停下（切到别的 App、熄屏）时是 null。停下 [TRIM_DELAY_MS] 之后还没
     * 回来就释放内核里可以重建的缓存：Android 14 起有前台服务的进程收不到 `onTrimMemory`
     * 的 `UI_HIDDEN`，只能自己看界面在不在；很快切回来时也不必重新翻译。
     */
    var listener: Listener? = null
        set(value) {
            field = value
            main.removeCallbacks(trimWhenHidden)
            if (value == null) main.postDelayed(trimWhenHidden, TRIM_DELAY_MS)
        }

    /** 所有会话共用的回调：会话比 Activity 活得久，事件转给当前的 Activity（如果有）。 */
    private val client = object : TerminalSessionClient {
        override fun onTextChanged(changedSession: TerminalSession) {
            listener?.onTextChanged(changedSession)
        }

        override fun onTitleChanged(changedSession: TerminalSession) {
            listener?.onSessionsChanged()
        }

        override fun onSessionFinished(finishedSession: TerminalSession) {
            listener?.onSessionsChanged()
        }

        override fun onCopyTextToClipboard(session: TerminalSession, text: String?) {
            val clipboard = getSystemService(ClipboardManager::class.java)
            clipboard.setPrimaryClip(ClipData.newPlainText(getString(R.string.app_name), text))
        }

        override fun onPasteTextFromClipboard(session: TerminalSession?) {
            val clipboard = getSystemService(ClipboardManager::class.java)
            val text = clipboard.primaryClip?.getItemAt(0)?.coerceToText(this@TerminalService)
            if (!text.isNullOrEmpty()) session?.emulator?.paste(text.toString())
        }

        override fun onBell(session: TerminalSession) {}

        override fun onColorsChanged(session: TerminalSession) {
            listener?.onTextChanged(session)
        }

        override fun onTerminalCursorStateChange(state: Boolean) {}

        override fun setTerminalShellPid(session: TerminalSession, pid: Int) {}

        override fun getTerminalCursorStyle(): Int? = null

        override fun logError(tag: String?, message: String?) {
            Log.e(tag, message.orEmpty())
        }

        override fun logWarn(tag: String?, message: String?) {
            Log.w(tag, message.orEmpty())
        }

        override fun logInfo(tag: String?, message: String?) {
            Log.i(tag, message.orEmpty())
        }

        override fun logDebug(tag: String?, message: String?) {
            Log.d(tag, message.orEmpty())
        }

        override fun logVerbose(tag: String?, message: String?) {
            Log.v(tag, message.orEmpty())
        }

        override fun logStackTraceWithMessage(tag: String?, message: String?, e: Exception?) {
            Log.e(tag, message.orEmpty(), e)
        }

        override fun logStackTrace(tag: String?, e: Exception?) {
            Log.e(tag, "", e)
        }
    }

    override fun onBind(intent: Intent): IBinder = binder

    override fun onCreate() {
        super.onCreate()
        createChannel()
        val notification = notification()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
        if (Rootfs.isReady(this)) start(null) else needsDistro = true
    }

    /** 用户选了发行版：下载（如果要）、导入 rootfs，然后启动内核。 */
    internal fun install(distro: Rootfs.Distro) {
        if (!needsDistro) return
        needsDistro = false
        installError = null
        start(distro)
    }

    /** 启动内核；[distro] 不为 null 时先准备它的 rootfs。 */
    private fun start(distro: Rootfs.Distro?) {
        thread(name = "appine-start") {
            val result = runCatching {
                if (distro != null) {
                    Rootfs.prepare(this, distro, progress = { done, total ->
                        val text = getString(R.string.downloading, distro.label, done shr 20, total shr 20)
                        main.post { listener?.onProgress(text) }
                    })
                    mirrorThread = thread(name = "appine-mirrors") { Rootfs.chooseMirror(this, distro) }
                }
                Rootfs.ensureNetworkConfig(this)
                Native.start(Rootfs.store(this).path, 1)
            }
            main.post {
                result.onSuccess {
                    machine = it
                    // 开机要等 sysinit、wait 的命令（OpenRC 起服务），不占 worker：标签页照常打开
                    thread(name = "appine-boot") { Native.boot(it) }
                    getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(network)
                    watchingNetwork = true
                    main.post(poll)
                    listener?.onMachineReady(null)
                }.onFailure {
                    Log.e(TAG, "cannot start", it)
                    val error = it.message ?: it.toString()
                    if (distro != null && !Rootfs.isReady(this)) {
                        // 下载、导入失败：让用户重试或者换一个
                        needsDistro = true
                        installError = error
                        listener?.onNeedDistro(error)
                    } else {
                        startError = error
                        listener?.onMachineReady(error)
                    }
                }
            }
        }
    }

    /**
     * 重置 rootfs（设计文档 §11.4）：结束所有会话和 guest 进程，删掉仓库，再让用户选发行版。
     * 停内核、删文件在 [worker] 上做。
     */
    fun reset() {
        if (machine == 0L) return
        val m = detach()
        worker.execute {
            Native.stop(m)
            mirrorThread?.join()
            Rootfs.reset(this)
            main.post {
                needsDistro = true
                listener?.onNeedDistro(null)
            }
        }
    }

    /**
     * 全部退出（通知里的按钮、guest 的 `poweroff`、关掉最后一个标签页之后也没有 guest 进程了）：
     * 先按 inittab 关机（OpenRC 停服务，PostgreSQL 这些不必下次做崩溃恢复），完了才停止服务，
     * 这期间前台服务和通知还在，App 进程不会被当作缓存进程杀掉。
     */
    fun exitAll() {
        if (exiting) return
        exiting = true
        // Activity 要到 onStop 才放开，关机可能比它快：先当它已经走了，关机完了还有 listener
        // 就是用户又打开了 App
        listener?.onExitAll()
        listener = null
        val m = detach()
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification())
        worker.execute {
            if (m != 0L) Native.shutdown(m)
            main.post {
                exiting = false
                // 关机期间用户又打开了 App：重新启动，不然界面一直停在"正在启动"
                if (listener != null) start(null) else stopSelf()
            }
        }
    }

    /** guest 要求重启（busybox 的 `reboot`）：关机，再启动一个新的内核（之后 Activity 开新会话）。 */
    private fun restart() {
        if (machine == 0L) return
        val m = detach()
        listener?.onRestarting()
        worker.execute {
            Native.shutdown(m)
            main.post { if (!exiting) start(null) }
        }
    }

    /**
     * 不再用当前的内核：不听网络回调、不再轮询，结束所有会话，返回原来的句柄。先清掉句柄：
     * 会话结束时 Activity 看到内核还在，会以为是最后一个标签页关掉了。
     */
    private fun detach(): Long {
        val m = machine
        if (watchingNetwork) {
            getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(network)
            watchingNetwork = false
        }
        machine = 0
        main.removeCallbacks(poll)
        for (s in sessions) s.finishIfRunning()
        sessions.clear()
        sessionsChanged()
        return m
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_EXIT_ALL) exitAll()
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        main.removeCallbacks(trimWhenHidden)
        main.removeCallbacks(poll)
        if (watchingNetwork) getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(network)
        for (s in sessions) s.finishIfRunning()
        sessions.clear()
        val m = machine
        machine = 0
        // 没经过 exitAll 就被停止了：照样关机，尽力而为。要等 guest 线程退出，不在主线程上做
        if (m != 0L) worker.execute { Native.shutdown(m) }
        worker.shutdown()
        super.onDestroy()
    }

    /**
     * 宿主内存紧张时立即丢掉内核里可以重建的缓存（池里保留的 Tier 1 翻译最多七八十 MB）。
     * Android 14 起不再发 `RUNNING_*` 这些级别；界面隐藏（`UI_HIDDEN`）由 [listener] 处理。
     */
    @Suppress("DEPRECATION") // RUNNING_LOW、RUNNING_CRITICAL 只在 Android 14 之前发
    override fun onTrimMemory(level: Int) {
        super.onTrimMemory(level)
        if (level >= TRIM_MEMORY_RUNNING_LOW && level != TRIM_MEMORY_UI_HIDDEN) trimMemory()
    }

    private fun trimMemory() {
        val m = machine
        if (m != 0L) worker.execute { Native.trimMemory(m) }
    }

    private fun setRoutes(routes: Array<String>) {
        val m = machine
        // 回调可能和 onDestroy 交错：worker 已经关掉时就不用再交了
        if (m != 0L) runCatching { worker.execute { Native.setRoutes(m, routes) } }
    }

    /** 新开一个终端会话（登录 shell）。shell 在第一次知道窗口大小时启动。 */
    fun createSession(): TerminalSession {
        val session = TerminalSession(ConsolePty(machine), TRANSCRIPT_ROWS, client)
        sessions.add(session)
        sessionsChanged()
        return session
    }

    /** 关闭会话：shell 收到 SIGHUP。 */
    fun removeSession(session: TerminalSession) {
        session.finishIfRunning()
        sessions.remove(session)
        sessionsChanged()
    }

    private fun sessionsChanged() {
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification())
        listener?.onSessionsChanged()
    }

    private fun createChannel() {
        val channel = NotificationChannel(
            CHANNEL_ID, getString(R.string.channel_sessions), NotificationManager.IMPORTANCE_LOW,
        )
        val services = NotificationChannel(
            NETWORK_CHANNEL_ID, getString(R.string.channel_network), NotificationManager.IMPORTANCE_DEFAULT,
        )
        getSystemService(NotificationManager::class.java).createNotificationChannels(listOf(channel, services))
    }

    /** [Native.takeListens] 的一项：`<进程名>\t<端口>\t<宿主上的端口>`。每个端口一条通知。 */
    private fun notifyListen(line: String) {
        val fields = line.split('\t')
        if (fields.size != 3) return
        val comm = fields[0]
        val port = fields[1].toIntOrNull() ?: return
        val hostPort = fields[2].toIntOrNull() ?: return
        val text = if (hostPort != port) {
            getString(R.string.listening_mapped, hostPort)
        } else {
            getString(R.string.listening_direct)
        }
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, NETWORK_CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.listening_title, comm, port))
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        getSystemService(NotificationManager::class.java).notify(LISTEN_NOTIFICATION_BASE + port, notification)
    }

    private fun notification(): Notification {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val exit = PendingIntent.getService(
            this, 1, Intent(this, TerminalService::class.java).setAction(ACTION_EXIT_ALL),
            PendingIntent.FLAG_IMMUTABLE,
        )
        val n = sessions.size
        val text = when {
            exiting -> getString(R.string.stopping)
            n == 0 && machine != 0L -> getString(R.string.background_running)
            else -> resources.getQuantityString(R.plurals.sessions_running, n, n)
        }
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, getString(R.string.exit_all), exit).build())
            .build()
    }

    companion object {
        private const val TAG = "appine"

        /** [Native.setRoutes] 的格式。Android 13 之前看不出路由的类型，都当作单播。 */
        fun routes(lp: LinkProperties): Array<String> = lp.routes.mapNotNull { r ->
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && r.type != RouteInfo.RTN_UNICAST) {
                return@mapNotNull null
            }
            val iface = r.`interface` ?: lp.interfaceName ?: return@mapNotNull null
            val dst = r.destination
            val gateway = r.gateway?.takeIf { r.hasGateway() }?.address?.let(::hex) ?: "-"
            "${hex(dst.rawAddress)}/${dst.prefixLength} $gateway $iface"
        }.toTypedArray()

        private fun hex(bytes: ByteArray) = bytes.joinToString("") { "%02x".format(it) }
        private const val CHANNEL_ID = "sessions"
        private const val NETWORK_CHANNEL_ID = "network"
        private const val NOTIFICATION_ID = 1
        /** 监听的通知按端口编号，与 [NOTIFICATION_ID] 错开。 */
        private const val LISTEN_NOTIFICATION_BASE = 1000
        private const val POLL_MS = 2_000L
        private const val ACTION_EXIT_ALL = "io.github.mthli.appine.EXIT_ALL"
        private const val TRANSCRIPT_ROWS = 2000
        private const val TRIM_DELAY_MS = 60_000L
    }
}
