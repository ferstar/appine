package io.github.mthli.appine

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.annotation.SuppressLint
import android.app.Service
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
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
import android.os.PowerManager
import android.util.Log
import com.termux.terminal.TerminalSession
import com.termux.terminal.TerminalSessionClient
import java.util.concurrent.Executors
import java.text.DateFormat
import java.text.DecimalFormat
import java.util.Date
import kotlin.concurrent.thread
import kotlin.concurrent.write

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
                    val kills = Native.takeKills(m)
                    val power = Native.takePower(m)
                    val processes = Native.processes(m)
                    main.post {
                        if (machine != m) return@post
                        lines?.forEach(::notifyListen)
                        kills?.forEach(::reportKill)
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

    /** 各个会话的 PTY（存标签页时要它的控制台和登录账号）。 */
    private val ptys = HashMap<TerminalSession, ConsolePty>()

    /** 恢复出来、还没显示过的会话存下的那份（模拟器还没建，回看记录和目录都还是原来的）。 */
    private val unshown = HashMap<TerminalSession, Tabs.Tab>()

    /** 上次被系统杀掉时存下的标签页，内核第一次起来之后恢复（设计文档 §11.2）。 */
    private var restore: Tabs.Saved? = null

    /** 恢复之后该显示的标签页（Activity 取用）。 */
    var focus: TerminalSession? = null
        private set

    /** Activity 正在显示的标签页（存标签页时记下是第几个）。 */
    var current: TerminalSession? = null

    /** 内核运行期间每 [SAVE_MS] 存一次标签页，界面隐藏、内存紧张时马上存。 */
    private val periodicSave = object : Runnable {
        override fun run() {
            saveTabs()
            main.postDelayed(this, SAVE_MS)
        }
    }

    /** 标签页开了、关了：稍等一下再存，接连几次只存一次。 */
    private val saveSoon = Runnable { saveTabs() }

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
            // 界面不在了，App 随时可能被杀：先存一份标签页
            saveTabs()
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
        // 上次没有正常退出：内核起来之后恢复标签页
        restore = Tabs.takeKilled(this)
        // 第一次运行（或者上次导入没完成）：导入 APK 里的 Alpine，别的发行版在设置里换
        start(if (Rootfs.isReady(this)) null else Rootfs.Distro.ALPINE)
    }

    /** 执行层（设置里选，设计文档 §4）：1 是 Tier 1，0 是解释器。下次启动内核时生效。 */
    val tier: Int get() = getSharedPreferences(PREFS, MODE_PRIVATE).getInt(KEY_TIER, 1)

    /**
     * 新标签页的登录账号（设置里选，设计文档 §11.1）：[ConsolePty.LOGIN_AUTO]、
     * [ConsolePty.LOGIN_ROOT] 或账号名。换了发行版、重置 rootfs 之后回到自动。
     */
    val login: String get() = login(this)

    /** 设置里选了登录账号：之后新开的标签页用它，开着的不变；"文件"App 里的家目录跟着换。 */
    fun setLogin(value: String) {
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putString(KEY_LOGIN, value).apply()
        Documents.rootsChanged(this)
        settingsChanged()
    }

    /** "文件"App 里是不是还显示只读的整个系统（设置里的开关，默认关，设计文档 §11.3）。 */
    val systemDocuments: Boolean get() = showSystemDocuments(this)

    fun setSystemDocuments(on: Boolean) {
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putBoolean(KEY_SYSTEM_DOCUMENTS, on).apply()
        Documents.rootsChanged(this)
        settingsChanged()
    }

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
                Rootfs.ensureLoginTools(this)
                Rootfs.updateProfile(this)
                // 内核打开仓库时要是唯一的用户（DocumentsProvider 另外打开的不算，见 Rootfs.access）
                Rootfs.access.write { Native.start(Rootfs.store(this).path, tier) }
            }
            main.post {
                result.onSuccess {
                    machine = it
                    progress = null
                    // 仓库可能是刚导入的（第一次运行、换了发行版）
                    Documents.rootsChanged(this)
                    updateWakeLock()
                    Tabs.markRunning(this)
                    restore?.let { saved -> restoreTabs(saved) }
                    restore = null
                    main.removeCallbacks(periodicSave)
                    main.postDelayed(periodicSave, SAVE_MS)
                    // 开机要等 sysinit、wait 的命令（OpenRC 起服务），不占 worker：标签页照常打开
                    thread(name = "appine-boot") {
                        runCatching {
                            val argv = arrayOf("sh", "-c", Rootfs.PREFER_OSCLOCK)
                            Native.waitFor(it, Native.spawn(it, -1, "/bin/sh", argv, ConsolePty.ENVIRONMENT))
                        }.onFailure { e -> Log.w(TAG, "cannot prefer osclock", e) }
                        Native.boot(it)
                        // 开机完成：OpenRC 有服务没起来时发通知（输出在 boot.log 里，没人看）
                        val failed = Rootfs.failedServices(this)
                        if (failed.isNotEmpty()) main.post { if (machine == it) notifyBootFailures(failed) }
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
        // 原来的账号都随仓库删了
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().remove(KEY_LOGIN).apply()
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

    /** 熄屏时保持运行（设置里、通知里的开关，默认关，设计文档 §11.2）。 */
    val keepAwake: Boolean get() = getSharedPreferences(PREFS, MODE_PRIVATE).getBoolean(KEY_KEEP_AWAKE, false)

    fun setKeepAwake(on: Boolean) {
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putBoolean(KEY_KEEP_AWAKE, on).apply()
        updateWakeLock()
        settingsChanged()
    }

    /** 熄屏时保持运行时持有的 `PARTIAL_WAKE_LOCK`。 */
    private var wakeLock: PowerManager.WakeLock? = null

    /**
     * 按开关和内核的状态持有、放开 `PARTIAL_WAKE_LOCK`：开着、内核在运行或者正在关机（[exitAll]，
     * 服务停下来之前）时持有，不然熄屏之后 CPU 休眠，guest 的进程跟着停住。
     *
     * App 一直有前台服务，Doze 里这个锁照样有效：`PowerManagerService` 只禁用进程状态在
     * `BOUND_FOREGROUND_SERVICE` 之后的 App 的 partial wake lock，网络也按同一个门槛放行
     * （`NetworkPolicyManager.FOREGROUND_THRESHOLD_STATE`），不必申请电池优化的豁免。
     */
    // 用户打开的开关：持有到内核停下或者用户关掉，没有固定的时长，也不在同一次调用里放开
    @SuppressLint("WakelockTimeout", "Wakelock")
    private fun updateWakeLock() {
        val want = keepAwake && (machine != 0L || exiting)
        val held = wakeLock
        if (want && held == null) {
            wakeLock = getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, WAKE_LOCK_TAG)
                .apply {
                    setReferenceCounted(false)
                    acquire()
                }
        } else if (!want && held != null) {
            held.release()
            wakeLock = null
        } else {
            return
        }
        updateNotification()
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
                updateWakeLock()
                // 正常退出：下次不恢复标签页
                Tabs.clear(this)
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
        updateWakeLock()
        main.removeCallbacks(poll)
        main.removeCallbacks(periodicSave)
        for (s in sessions) s.finishIfRunning()
        sessions.clear()
        ptys.clear()
        unshown.clear()
        sessionsChanged()
        return m
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_EXIT_ALL -> exitAll()
            ACTION_CANCEL_SWITCH -> cancelSwitch()
            ACTION_TOGGLE_KEEP_AWAKE -> setKeepAwake(!keepAwake)
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        main.removeCallbacks(trimWhenHidden)
        main.removeCallbacks(poll)
        main.removeCallbacks(periodicSave)
        main.removeCallbacks(saveSoon)
        // 服务正常停下（exitAll 之后）：下次不恢复。被杀时不会走到这里
        Tabs.clear(this)
        if (watchingNetwork) getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(network)
        for (s in sessions) s.finishIfRunning()
        sessions.clear()
        val m = machine
        machine = 0
        // 没经过 exitAll 就被停止了：照样关机，尽力而为。要等 guest 线程退出，不在主线程上做
        if (m != 0L) worker.execute { Native.shutdown(m) }
        worker.shutdown()
        wakeLock?.release()
        wakeLock = null
        super.onDestroy()
    }

    /**
     * 宿主内存紧张时立即丢掉内核里可以重建的缓存（池里保留的 Tier 1 翻译最多七八十 MB）。
     * Android 14 起不再发 `RUNNING_*` 这些级别；界面隐藏（`UI_HIDDEN`）由 [removeListener] 处理。
     */
    @Suppress("DEPRECATION") // RUNNING_LOW、RUNNING_CRITICAL 只在 Android 14 之前发
    override fun onTrimMemory(level: Int) {
        super.onTrimMemory(level)
        if (level >= TRIM_MEMORY_RUNNING_LOW && level != TRIM_MEMORY_UI_HIDDEN) {
            trimMemory()
            saveTabs()
        }
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

    /**
     * 新开一个终端会话（登录 shell），以 [account] 登录、从 [cwd] 开始（null 是家目录）。shell
     * 在第一次知道窗口大小时启动。
     */
    fun createSession(account: String = login, cwd: String? = null): TerminalSession {
        val pty = ConsolePty(machine, ConsolePty.script(this), account, cwd)
        val session = TerminalSession(pty, TRANSCRIPT_ROWS, client)
        sessions.add(session)
        ptys[session] = pty
        sessionsChanged()
        return session
    }

    /** 关闭会话：shell 收到 SIGHUP。 */
    fun removeSession(session: TerminalSession) {
        session.finishIfRunning()
        sessions.remove(session)
        ptys.remove(session)
        unshown.remove(session)
        sessionsChanged()
    }

    private fun sessionsChanged() {
        updateNotification()
        each { it.onSessionsChanged() }
        main.removeCallbacks(saveSoon)
        if (machine != 0L) main.postDelayed(saveSoon, SAVE_SOON_MS)
    }

    /**
     * 恢复上次被杀时的标签页（设计文档 §11.2）：每个起新的 shell，以原来的账号登录、从原来的
     * 目录开始，先显示原来的回看记录和一行提示。不恢复进程。
     */
    private fun restoreTabs(saved: Tabs.Saved) {
        val time = DateFormat.getDateTimeInstance(DateFormat.SHORT, DateFormat.SHORT).format(Date(saved.time))
        val notice = "\u001b[33m[appine] " + getString(R.string.restored_notice, time) + "\u001b[0m\r\n\r\n"
        for (tab in saved.tabs) {
            val session = createSession(tab.login, tab.cwd)
            val body = tab.transcript.trimEnd().replace("\r\n", "\n").replace("\n", "\r\n")
            session.setRestored(((if (body.isEmpty()) "" else body + "\r\n") + notice).toByteArray())
            unshown[session] = tab
        }
        focus = sessions.getOrNull(saved.current) ?: sessions.lastOrNull()
    }

    /**
     * 存下标签页（[Tabs]）：回看记录在主线程上取（模拟器在主线程上改），前台进程的当前目录要取
     * BKL，和写文件一起在 [worker] 上做。已经结束的标签页不存。
     */
    private fun saveTabs() {
        val m = machine
        if (m == 0L) return
        main.removeCallbacks(saveSoon)
        val live = sessions.filter { it.isRunning && it in ptys }
        val snapshot = live.map { s ->
            val pty = ptys.getValue(s)
            val text = s.transcriptText
            // 还没显示过的恢复出来的标签页：shell 还没起，存原来的那份
            val old = if (text == null) unshown[s] else null.also { unshown.remove(s) }
            Triple(pty, text ?: old?.transcript.orEmpty(), old?.cwd)
        }
        val index = live.indexOf(current).coerceAtLeast(0)
        runCatching {
            worker.execute {
                val tabs = snapshot.map { (pty, text, oldCwd) ->
                    val console = pty.console
                    val cwd = if (console >= 0) Native.consoleCwd(m, console) else oldCwd
                    Tabs.Tab(pty.login, cwd, text)
                }
                Tabs.save(this, tabs, index)
            }
        }
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
        val problems = NotificationChannel(
            PROBLEMS_CHANNEL_ID, getString(R.string.channel_problems), NotificationManager.IMPORTANCE_DEFAULT,
        )
        getSystemService(NotificationManager::class.java)
            .createNotificationChannels(listOf(channel, services, problems))
    }

    /**
     * [Native.takeKills] 的一项：内核层结束了一个进程（内核 bug、引擎故障、OOM killer）。它的控制
     * 终端是某个标签页时在那个标签页里说明原因（shell 只会打印 "Killed"、"Segmentation fault"），
     * 不在标签页里（后台的服务）、标签页正在全屏程序里或者界面不在时发通知。
     */
    private fun reportKill(line: String) {
        val f = line.split('\t')
        if (f.size != 7) return
        val console = f[0].toIntOrNull() ?: return
        val pid = f[1].toIntOrNull() ?: return
        val comm = f[2]
        // memory.max 的 16M 是 16 MiB：用 1024 进制，不然上限显示成 17 MB，看不出超了
        val size = { bytes: String ->
            val b = bytes.toLongOrNull() ?: 0
            if (b >= 1L shl 30) DecimalFormat("0.#").format(b / 1073741824.0) + " GiB"
            else DecimalFormat("0.#").format(b / 1048576.0) + " MiB"
        }
        val text = when (f[3]) {
            "oom" -> if (f[4].isEmpty()) {
                getString(R.string.killed_oom_app, comm, pid, size(f[5]), size(f[6]))
            } else {
                getString(R.string.killed_oom_cgroup, comm, pid, f[4], size(f[5]), size(f[6]))
            }
            else -> getString(R.string.killed_bug, comm, pid)
        }
        val session = sessions.firstOrNull { ptys[it]?.console == console && console >= 0 }
        val shown = session?.showMessage("[appine] $text") == true
        if (shown && listeners.isNotEmpty()) return
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, PROBLEMS_CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.killed_title, comm))
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        getSystemService(NotificationManager::class.java).notify(KILL_NOTIFICATION_BASE + pid % 1000, notification)
    }

    /** 开机时 OpenRC 有服务没起来：发一条通知，说是哪几个、输出在哪里。 */
    private fun notifyBootFailures(services: List<String>) {
        val title = resources.getQuantityString(R.plurals.boot_failed_title, services.size, services.size)
        val text = getString(R.string.boot_failed_text, services.joinToString(", "))
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, PROBLEMS_CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setAutoCancel(true)
            .build()
        getSystemService(NotificationManager::class.java).notify(BOOT_NOTIFICATION_ID, notification)
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
        val state = when {
            exiting -> getString(R.string.stopping)
            machine == 0L && progress != null -> progress
            n == 0 && machine != 0L -> getString(R.string.background_running)
            else -> resources.getQuantityString(R.plurals.sessions_running, n, n)
        }
        val text = if (wakeLock != null) getString(R.string.kept_awake, state) else state
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
        if (!exiting) {
            val toggle = PendingIntent.getService(
                this, 3, Intent(this, TerminalService::class.java).setAction(ACTION_TOGGLE_KEEP_AWAKE),
                PendingIntent.FLAG_IMMUTABLE,
            )
            val label = getString(if (keepAwake) R.string.allow_sleep else R.string.keep_awake)
            builder.addAction(Notification.Action.Builder(null, label, toggle).build())
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
        private const val PROBLEMS_CHANNEL_ID = "problems"
        /** 开机失败的通知；进程被结束的按 pid 编号（与监听的错开）。 */
        private const val BOOT_NOTIFICATION_ID = 2
        private const val KILL_NOTIFICATION_BASE = 3000
        private const val NOTIFICATION_ID = 1
        /** 监听的通知按端口编号，与 [NOTIFICATION_ID] 错开。 */
        private const val LISTEN_NOTIFICATION_BASE = 1000
        private const val POLL_MS = 2_000L
        private const val ACTION_EXIT_ALL = "io.github.mthli.appine.EXIT_ALL"
        private const val ACTION_CANCEL_SWITCH = "io.github.mthli.appine.CANCEL_SWITCH"
        private const val ACTION_TOGGLE_KEEP_AWAKE = "io.github.mthli.appine.TOGGLE_KEEP_AWAKE"
        private const val KEY_KEEP_AWAKE = "keep_awake"
        private const val WAKE_LOCK_TAG = "appine:keep-awake"
        private const val PREFS = "settings"
        private const val KEY_TIER = "tier"
        private const val KEY_LOGIN = "login"
        private const val KEY_SYSTEM_DOCUMENTS = "system_documents"

        /** 设置里的登录账号（见实例的 [login]）；DocumentsProvider 也要用，不经过服务。 */
        fun login(context: Context): String =
            context.getSharedPreferences(PREFS, MODE_PRIVATE).getString(KEY_LOGIN, null) ?: ConsolePty.LOGIN_AUTO

        /** 见实例的 [systemDocuments]。 */
        fun showSystemDocuments(context: Context): Boolean =
            context.getSharedPreferences(PREFS, MODE_PRIVATE).getBoolean(KEY_SYSTEM_DOCUMENTS, false)
        private const val TRANSCRIPT_ROWS = 2000
        private const val TRIM_DELAY_MS = 60_000L
        private const val SAVE_MS = 60_000L
        private const val SAVE_SOON_MS = 2_000L
    }
}
