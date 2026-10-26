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
 */
class TerminalService : Service() {

    /** Activity 关心的事件。回调都在主线程上。 */
    interface Listener {
        /** 内核已经启动（或启动失败，[error] 不为 null）。 */
        fun onMachineReady(error: String?)

        /** 会话列表、标题或状态变了。 */
        fun onSessionsChanged()

        /** 会话的屏幕内容变了。 */
        fun onTextChanged(session: TerminalSession)

        /** 用户在通知里选了"全部退出"。 */
        fun onExitAll()
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

    /** 内核句柄，启动之前是 0。 */
    var machine = 0L
        private set

    /** 启动失败的原因。 */
    var startError: String? = null
        private set

    val sessions = mutableListOf<TerminalSession>()

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
        thread(name = "appine-start") {
            val result = runCatching {
                val firstRun = !Rootfs.isReady(this)
                Rootfs.prepare(this)
                if (firstRun) thread(name = "appine-mirrors") { Rootfs.chooseMirror(this) }
                Native.start(Rootfs.store(this).path, 1)
            }
            main.post {
                result.onSuccess { machine = it }.onFailure {
                    Log.e(TAG, "cannot start", it)
                    startError = it.message ?: it.toString()
                }
                listener?.onMachineReady(startError)
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_EXIT_ALL) {
            listener?.onExitAll()
            stopSelf()
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        main.removeCallbacks(trimWhenHidden)
        for (s in sessions) s.finishIfRunning()
        sessions.clear()
        val m = machine
        machine = 0
        // 结束所有 guest 进程要等它们的线程退出，不在主线程上做
        if (m != 0L) worker.execute { Native.stop(m) }
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
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
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
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(resources.getQuantityString(R.plurals.sessions_running, n, n))
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, getString(R.string.exit_all), exit).build())
            .build()
    }

    companion object {
        private const val TAG = "appine"
        private const val CHANNEL_ID = "sessions"
        private const val NOTIFICATION_ID = 1
        private const val ACTION_EXIT_ALL = "io.github.mthli.appine.EXIT_ALL"
        private const val TRANSCRIPT_ROWS = 2000
        private const val TRIM_DELAY_MS = 60_000L
    }
}
