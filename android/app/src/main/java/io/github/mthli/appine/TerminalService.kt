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
 *
 * 第一次运行时导入 APK 里的 Alpine；设置里换发行版、重置 rootfs 时先在后台下载，原来的系统
 * 照常运行，下载好了才停下内核、换掉仓库（[switchTo]）。
 */
class TerminalService : Service() {

    /** 界面（终端、设置）关心的事件。回调都在主线程上，界面只实现自己要的。 */
    interface Listener {
        /** 内核已经启动（或启动失败，[error] 不为 null）。 */
        fun onMachineReady(error: String?) {}

        /** 准备 rootfs 的进度（[progress]）。 */
        fun onProgress(text: String) {}

        /** 会话列表、标题或状态变了。 */
        fun onSessionsChanged() {}

        /** 会话的屏幕内容变了。 */
        fun onTextChanged(session: TerminalSession) {}

        /** 全部退出：用户在通知里选了"全部退出"，或者 guest 关机了。 */
        fun onExitAll() {}

        /**
         * 要重启：guest 的 `reboot`、设置里换了执行层。会话都结束了，内核关机之后重新启动（之后
         * 是 [onMachineReady]）。
         */
        fun onRestarting() {}

        /** 设置里看的状态变了：换发行版的下载进度（[switching]）、镜像。 */
        fun onSettingsChanged() {}

        /** 有要告诉用户的事（[takeNotice]）：换发行版没有成功。 */
        fun onNotice() {}
    }

    /**
     * 设置里换发行版（重置 rootfs 是换成同一个）：[distro] 的 tarball 正在后台下载，原来的
     * 系统照常运行。
     */
    internal class Switch(val distro: Rootfs.Distro) {
        @Volatile var done = 0L
        @Volatile var total = distro.download?.size ?: 0L
        @Volatile var cancelled = false
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

    /** 界面隐藏一段时间之后还没回来才释放缓存（[addListener]）。 */
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
                            else -> if (sessions.isEmpty() && listeners.isEmpty() && processes == 0) exitAll()
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

    /** 正在准备 rootfs（导入、换发行版）时的进度，内核启动之后是 null。 */
    var progress: String? = null
        private set

    /** 设置里换发行版，正在下载（[switchTo]）。 */
    internal var switching: Switch? = null
        private set

    /**
     * 下载线程还没结束的那次换发行版。取消之后线程可能还卡在连接、读上（最多一个超时），这期间
     * 不开始新的下载：两个线程会写同一个 `.part`，先结束的那个还会删掉它。
     */
    private var downloader: Switch? = null

    /** 要告诉用户、还没告诉的事（[Listener.onNotice]）。 */
    private var notice: String? = null

    /** 导入之后、设置里选"自动"之后给镜像测速的线程：换掉仓库之前要等它写完源。 */
    @Volatile
    private var mirrorThread: Thread? = null

    /** 正在给镜像测速。 */
    val measuring: Boolean get() = mirrorThread?.isAlive == true

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
     * 看得见的界面（终端、设置），Activity 在 onStart 加上、onStop 去掉；都停下（切到别的 App、
     * 熄屏）[TRIM_DELAY_MS] 之后还没回来就释放内核里可以重建的缓存：Android 14 起有前台服务的
     * 进程收不到 `onTrimMemory` 的 `UI_HIDDEN`，只能自己看界面在不在；很快切回来时也不必重新
     * 翻译。
     */
    private val listeners = mutableListOf<Listener>()

    fun addListener(l: Listener) {
        if (l !in listeners) listeners.add(l)
        main.removeCallbacks(trimWhenHidden)
    }

    fun removeListener(l: Listener) {
        listeners.remove(l)
        if (listeners.isEmpty()) {
            main.removeCallbacks(trimWhenHidden)
            main.postDelayed(trimWhenHidden, TRIM_DELAY_MS)
        }
    }

    /** 回调过程中界面可能去掉自己，所以先拷一份。 */
    private fun each(f: (Listener) -> Unit) = listeners.toList().forEach(f)

    /** 所有会话共用的回调：会话比 Activity 活得久，事件转给当前的 Activity（如果有）。 */
    private val client = object : TerminalSessionClient {
        override fun onTextChanged(changedSession: TerminalSession) {
            each { it.onTextChanged(changedSession) }
        }

        override fun onTitleChanged(changedSession: TerminalSession) {
            each { it.onSessionsChanged() }
        }

        override fun onSessionFinished(finishedSession: TerminalSession) {
            each { it.onSessionsChanged() }
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
            each { it.onTextChanged(session) }
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
        // 第一次运行（或者上次导入没完成）：导入 APK 里的 Alpine，别的发行版在设置里换
        start(if (Rootfs.isReady(this)) null else Rootfs.Distro.ALPINE)
    }

    /** 执行层（设置里选，设计文档 §4）：1 是 Tier 1，0 是解释器。下次启动内核时生效。 */
    val tier: Int get() = getSharedPreferences(PREFS, MODE_PRIVATE).getInt(KEY_TIER, 1)

    /** 启动内核；[distro] 不为 null 时先准备它的 rootfs（仓库要已经删掉了）。 */
    private fun start(distro: Rootfs.Distro?) {
        startError = null
        if (distro != null) preparing(getString(R.string.preparing, distro.label))
        val tier = tier
        thread(name = "appine-start") {
            val result = runCatching {
                if (distro != null) {
                    Rootfs.prepare(this, distro, progress = { done, total ->
                        val text = getString(R.string.downloading, distro.label, done shr 20, total shr 20)
                        main.post { preparing(text) }
                    })
                    mirrorThread = thread(name = "appine-mirrors") {
                        Rootfs.chooseMirror(this, distro)
                        main.post { settingsChanged() }
                    }
                }
                Rootfs.ensureNetworkConfig(this)
                Native.start(Rootfs.store(this).path, tier)
            }
            main.post {
                result.onSuccess {
                    machine = it
                    progress = null
                    // 开机要等 sysinit、wait 的命令（OpenRC 起服务），不占 worker：标签页照常打开
                    thread(name = "appine-boot") {
                        runCatching {
                            val argv = arrayOf("sh", "-c", Rootfs.PREFER_OSCLOCK)
                            Native.waitFor(it, Native.spawn(it, -1, "/bin/sh", argv, ConsolePty.ENVIRONMENT))
                        }.onFailure { e -> Log.w(TAG, "cannot prefer osclock", e) }
                        Native.boot(it)
                    }
                    getSystemService(ConnectivityManager::class.java).registerDefaultNetworkCallback(network)
                    watchingNetwork = true
                    main.post(poll)
                    updateNotification()
                    each { l -> l.onMachineReady(null) }
                }.onFailure {
                    Log.e(TAG, "cannot start", it)
                    val error = it.message ?: it.toString()
                    if (distro != null && distro != Rootfs.Distro.ALPINE && !Rootfs.isReady(this)) {
                        // 换成别的发行版时导入失败（下载已经校验过，多半是空间不够）：原来的仓库
                        // 已经删了，退回 APK 里的 Alpine，不让用户对着一个用不了的 App
                        tell(getString(R.string.switch_failed, distro.label, error, Rootfs.Distro.ALPINE.label))
                        start(Rootfs.Distro.ALPINE)
                    } else {
                        progress = null
                        startError = error
                        updateNotification()
                        each { l -> l.onMachineReady(error) }
                    }
                }
            }
        }
    }

    private fun preparing(text: String) {
        progress = text
        updateNotification()
        each { it.onProgress(text) }
    }

    /** 记下要告诉用户的事，看得见的界面马上取走（[takeNotice]），没有的话等界面回来。 */
    private fun tell(text: String) {
        notice = text
        each { it.onNotice() }
    }

    /** 取走要告诉用户的事（只给一个界面）。 */
    fun takeNotice(): String? = notice.also { notice = null }

    /** 可以换发行版、重置 rootfs：内核在运行，或者启动失败了（仓库坏了时靠它恢复）。 */
    val canSwitch: Boolean get() = switching == null && downloader == null && (machine != 0L || startError != null)

    /**
     * 换成 [distro]（设置里；重置 rootfs 是换成装着的那个）。先在后台下载并校验 tarball，这期间
     * 原来的系统照常运行，可以取消（[cancelSwitch]）；下载好了才结束所有会话、停下内核、删掉
     * 仓库，导入新的。下载失败时原来的系统不动。
     */
    internal fun switchTo(distro: Rootfs.Distro) {
        if (!canSwitch) return
        val sw = Switch(distro)
        switching = sw
        downloader = sw
        settingsChanged()
        thread(name = "appine-download") {
            val result = runCatching {
                Rootfs.download(this, distro, progress = { done, total ->
                    sw.done = done
                    sw.total = total
                    main.post { if (switching === sw) settingsChanged() }
                }, cancelled = { sw.cancelled })
            }
            main.post {
                if (downloader === sw) downloader = null
                if (switching !== sw) {
                    // 已经取消了：结果不要，只是现在可以再换了
                    settingsChanged()
                    return@post
                }
                switching = null
                result.onSuccess {
                    if (machine != 0L || startError != null) replace(distro)
                }.onFailure {
                    if (it !is Rootfs.Cancelled) {
                        Log.w(TAG, "cannot download ${distro.label}", it)
                        tell(getString(R.string.download_failed, distro.label, it.message ?: it.toString()))
                    }
                }
                settingsChanged()
            }
        }
    }

    /** 不换了：中止下载（下了一半的不留）。 */
    fun cancelSwitch() {
        val sw = switching ?: return
        sw.cancelled = true
        switching = null
        settingsChanged()
    }

    /**
     * [switchTo] 下载好之后：结束所有会话、停下内核（启动失败时没有）、删掉仓库，导入 [distro]。
     * 停内核、删文件在 [worker] 上做。
     */
    private fun replace(distro: Rootfs.Distro) {
        val m = detach()
        startError = null
        preparing(getString(R.string.preparing, distro.label))
        worker.execute {
            if (m != 0L) Native.stop(m)
            mirrorThread?.join()
            Rootfs.reset(this)
            main.post { start(distro) }
        }
    }

    /** 设置里选了镜像：写进源（在 [worker] 上，要打开仓库）。 */
    fun setMirror(mirror: String) {
        val distro = Rootfs.installed(this) ?: return
        if (machine == 0L || switching != null) return
        worker.execute {
            runCatching { Rootfs.setMirror(this, distro, mirror) }
                .onFailure { Log.w(TAG, "cannot write the package sources", it) }
            main.post { settingsChanged() }
        }
    }

    /** 设置里选了"自动"：重新测速，写进最快的。 */
    fun autoMirror() {
        val distro = Rootfs.installed(this) ?: return
        if (machine == 0L || switching != null || mirrorThread?.isAlive == true) return
        mirrorThread = thread(name = "appine-mirrors") {
            Rootfs.chooseMirror(this, distro)
            main.post { settingsChanged() }
        }
    }

    /** 设置里换了执行层：记下，重启内核。 */
    fun setTier(value: Int) {
        if (value == tier) return
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putInt(KEY_TIER, value).apply()
        restart()
    }

    private fun settingsChanged() {
        updateNotification()
        each { it.onSettingsChanged() }
    }

    /**
     * 全部退出（通知里的按钮、guest 的 `poweroff`、关掉最后一个标签页之后也没有 guest 进程了）：
     * 先按 inittab 关机（OpenRC 停服务，PostgreSQL 这些不必下次做崩溃恢复），完了才停止服务，
     * 这期间前台服务和通知还在，App 进程不会被当作缓存进程杀掉。
     */
    fun exitAll() {
        if (exiting) return
        exiting = true
        // Activity 要到 onStop 才放开，关机可能比它快：先当它们已经走了，关机完了还有界面就是
        // 用户又打开了 App
        switching?.cancelled = true
        switching = null
        each { it.onExitAll() }
        listeners.clear()
        val m = detach()
        updateNotification()
        worker.execute {
            if (m != 0L) Native.shutdown(m)
            main.post {
                exiting = false
                // 关机期间用户又打开了 App：重新启动，不然界面一直停在"正在启动"
                if (listeners.isNotEmpty()) start(null) else stopSelf()
            }
        }
    }

    /**
     * 重启（guest 的 `reboot`、设置里换了执行层）：关机，再启动一个新的内核（之后 Activity 开
     * 新会话）。
     */
    private fun restart() {
        if (machine == 0L) return
        val m = detach()
        each { it.onRestarting() }
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
        when (intent?.action) {
            ACTION_EXIT_ALL -> exitAll()
            ACTION_CANCEL_SWITCH -> cancelSwitch()
        }
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
     * Android 14 起不再发 `RUNNING_*` 这些级别；界面隐藏（`UI_HIDDEN`）由 [removeListener] 处理。
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
        updateNotification()
        each { it.onSessionsChanged() }
    }

    private fun updateNotification() {
        getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification())
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
        val sw = switching
        val text = when {
            exiting -> getString(R.string.stopping)
            machine == 0L && progress != null -> progress
            n == 0 && machine != 0L -> getString(R.string.background_running)
            else -> resources.getQuantityString(R.plurals.sessions_running, n, n)
        }
        val builder = Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(open)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
        if (sw != null) {
            // 换发行版的下载：进度放在第二行，可以在通知里取消
            val cancel = PendingIntent.getService(
                this, 2, Intent(this, TerminalService::class.java).setAction(ACTION_CANCEL_SWITCH),
                PendingIntent.FLAG_IMMUTABLE,
            )
            builder.setSubText(getString(R.string.downloading, sw.distro.label, sw.done shr 20, sw.total shr 20))
            if (sw.total > 0) builder.setProgress(sw.total.toInt(), sw.done.toInt(), false)
            builder.addAction(Notification.Action.Builder(null, getString(R.string.cancel_download), cancel).build())
        }
        return builder
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
        private const val ACTION_CANCEL_SWITCH = "io.github.mthli.appine.CANCEL_SWITCH"
        private const val PREFS = "settings"
        private const val KEY_TIER = "tier"
        private const val TRANSCRIPT_ROWS = 2000
        private const val TRIM_DELAY_MS = 60_000L
    }
}
