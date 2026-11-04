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
import android.system.Os
import android.system.OsConstants
import android.util.Log
import android.widget.Toast
import java.util.concurrent.Executors
import java.text.DateFormat
import java.text.DecimalFormat
import java.util.Date
import kotlin.concurrent.thread
import kotlin.concurrent.write

/**
 * The foreground service that holds the guest kernel and all terminal sessions (the android/service
 * map). The sessions' lifecycle hangs on the service, not on an Activity: shells keep running after
 * the Activity is closed. The notification shows the number of sessions and offers "Exit all".
 *
 * Once the kernel starts it boots by `/etc/inittab` (OpenRC services on Alpine), and on exit it
 * first shuts down by it ([exitAll]). After the last tab is closed the service stays in the
 * background while guest processes (services) are still running, and exits only when there are
 * none; the guest's `poweroff` and `halt` mean exit all, and `reboot` restarts the kernel.
 *
 * On the first run the Alpine in the APK is imported; switching distros or resetting the rootfs in
 * Settings downloads in the background first while the old system keeps running, and only once the
 * download is done is the kernel stopped and the store replaced ([switchTo]).
 */
class TerminalService : Service() {

    /**
     * Events the UI (terminal, settings) cares about. Callbacks all run on the main thread; each UI
     * implements only the ones it needs.
     */
    interface Listener {
        /** The kernel has started (or failed to start, with [error] non-null). */
        fun onMachineReady(error: String?) {}

        /** Progress of preparing the rootfs ([progress]). */
        fun onProgress(text: String) {}

        /** The session list, a title or a state changed. */
        fun onSessionsChanged() {}

        /** Exit all: the user chose "Exit all" in the notification, or the guest shut down. */
        fun onExitAll() {}

        /**
         * About to restart: the guest's `reboot`, or (in a debug build) the execution tier changed
         * in Settings. All sessions have ended; the kernel shuts down and starts again (followed by
         * [onMachineReady]).
         */
        fun onRestarting() {}

        /**
         * State shown in Settings changed: the download progress of a distro switch ([switching]),
         * the mirror.
         */
        fun onSettingsChanged() {}

        /** There is something to tell the user ([takeNotice]): a distro switch failed. */
        fun onNotice() {}
    }

    /**
     * A distro switch in Settings (resetting the rootfs is a switch to the same one): the tarball
     * of [distro] is downloading in the background while the old system keeps running.
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
     * Releasing caches and stopping the kernel are done in order on this thread: both take the BKL,
     * so they stay off the main thread; and after the stop, no cache-releasing call can use the
     * released handle.
     */
    private val worker = Executors.newSingleThreadExecutor { Thread(it, "appine-worker") }

    /**
     * Caches are released only when the UI has been hidden for a while and has not come back
     * ([addListener]).
     */
    private val trimWhenHidden = Runnable { trimMemory() }

    /**
     * Every [POLL_MS], asks the kernel on [worker] (this takes the BKL):
     * - when the guest first listens on a port on a non-loopback address, posts a notification
     *   (design doc §12 item 7): other devices can reach it, and a port below 1024 that the host
     *   refuses was moved, so the user must be told which one to connect to;
     * - the guest's power requests (busybox's reboot, halt, poweroff);
     * - directories the guest changed: the Files app is told to refresh them (the android/documents
     *   map);
     * - whether guest processes remain when there are no sessions and no UI: if none, exit (the
     *   android/service map).
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
                    Native.takeChangedDirs(m)?.let { Documents.dirsChanged(this@TerminalService, it) }
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

    /** Shutting down to exit ([exitAll]). */
    private var exiting = false

    /** The kernel handle, 0 before the start. */
    var machine = 0L
        private set

    /** Why the start failed. */
    var startError: String? = null
        private set

    /**
     * Progress while preparing the rootfs (import, distro switch); null once the kernel has
     * started.
     */
    var progress: String? = null
        private set

    /** A distro switch from Settings that is downloading ([switchTo]). */
    internal var switching: Switch? = null
        private set

    /**
     * The distro switch whose download thread has not finished yet. After cancelling, the thread
     * may still be stuck connecting or reading (for at most one timeout), and no new download
     * starts meanwhile: two threads would write the same `.part`, and the one that finishes first
     * would also delete it.
     */
    private var downloader: Switch? = null

    /** Something to tell the user that has not been told yet ([Listener.onNotice]). */
    private var notice: String? = null

    /**
     * The thread that probes mirror speeds after an import or after "automatic" is chosen in
     * Settings: replacing the store must wait until it has written the sources.
     */
    @Volatile
    private var mirrorThread: Thread? = null

    /** Mirror speeds are being probed. */
    val measuring: Boolean get() = mirrorThread?.isAlive == true

    /** [network] is registered. */
    private var watchingNetwork = false

    /** The tabs. */
    internal val sessions = mutableListOf<VtSession>()

    /**
     * The saved copy of restored sessions not shown yet (their emulator is not created yet, so the
     * transcript and the directory are still the original ones).
     */
    private val unshown = HashMap<VtSession, Tabs.Tab>()

    /**
     * Tabs saved when the system last killed the app, restored after the kernel first comes up (the
     * android/service map).
     */
    private var restore: Tabs.Saved? = null

    /** The tab to show after restoring (taken by the Activity). */
    internal var focus: VtSession? = null
        private set

    /** The tab the Activity is showing (saving tabs records which one it is). */
    internal var current: VtSession? = null

    /**
     * Tabs are saved every [SAVE_MS] while the kernel runs, and at once when the UI is hidden or
     * memory is short.
     */
    private val periodicSave = object : Runnable {
        override fun run() {
            saveTabs()
            main.postDelayed(this, SAVE_MS)
        }
    }

    /** A tab was opened or closed: save a little later, once for several in a row. */
    private val saveSoon = Runnable { saveTabs() }

    /**
     * The default network's routes go to the guest (the kernel/net map): since Android 10 apps
     * cannot read the host's routing table, and the guest's `ip route` and `/proc/net/route` rely
     * on this. Callbacks come on system threads and are handed to [worker] to run in order.
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
     * The visible UIs (terminal, settings), added by Activities in onStart and removed in onStop;
     * when all have stopped (switched to another app, screen off) and none has come back after
     * [TRIM_DELAY_MS], the kernel's rebuildable caches are released: since Android 14 a process
     * with a foreground service no longer gets `onTrimMemory`'s `UI_HIDDEN`, so it has to watch
     * whether the UI is there itself; when the user switches back quickly, nothing has to be
     * translated again either.
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
            // The UI is gone and the app may be killed at any time: save the tabs first
            saveTabs()
        }
    }

    /** A UI may remove itself during a callback, so iterate over a copy. */
    private fun each(f: (Listener) -> Unit) = listeners.toList().forEach(f)

    /**
     * What the sessions report (title changes, ends) goes to the current Activity (if any); their
     * screens go to their views directly.
     */
    private val sessionEvents = object : VtSession.Events {
        override fun onTitleChanged(session: VtSession) {
            each { it.onSessionsChanged() }
        }

        override fun onFinished(session: VtSession) {
            each { it.onSessionsChanged() }
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
        // The last run did not exit normally: restore the tabs once the kernel is up
        restore = Tabs.takeKilled(this)
        // First run (or the last import did not finish): import the Alpine in the APK; other
        // distros are switched to in Settings
        start(if (Rootfs.isReady(this)) null else Rootfs.Distro.ALPINE)
    }

    /**
     * The execution tier (see the engine map): 1 is Tier 1, 0 is the interpreter. A debug build
     * takes it from Settings, effective at the next kernel start; a release build always runs Tier
     * 1, ignoring a value recorded by an earlier debug install.
     */
    val tier: Int
        get() = if (BuildConfig.DEBUG) getSharedPreferences(PREFS, MODE_PRIVATE).getInt(KEY_TIER, 1) else 1

    /**
     * The login account of new tabs (chosen in Settings, the android/setup map):
     * [ConsolePty.LOGIN_AUTO], [ConsolePty.LOGIN_ROOT] or an account name. Goes back to automatic
     * after a distro switch or a rootfs reset.
     */
    val login: String get() = login(this)

    /**
     * A login account was chosen in Settings: tabs opened from now on use it, open ones are
     * unchanged; the home directory in the Files app follows.
     */
    fun setLogin(value: String) {
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putString(KEY_LOGIN, value).apply()
        Documents.rootsChanged(this)
        settingsChanged()
    }

    /**
     * Whether the Files app also shows the whole system, read-only (a switch in Settings, off by
     * default, the android/documents map).
     */
    val systemDocuments: Boolean get() = showSystemDocuments(this)

    fun setSystemDocuments(on: Boolean) {
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putBoolean(KEY_SYSTEM_DOCUMENTS, on).apply()
        Documents.rootsChanged(this)
        settingsChanged()
    }

    /**
     * Starts the kernel; when [distro] is non-null, prepares its rootfs first (the store must
     * already be deleted).
     */
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
                // The kernel must be the only user of the store when it opens it (a separate
                // opening by the DocumentsProvider does not count, see Rootfs.access)
                Rootfs.access.write { Native.start(Rootfs.store(this).path, tier) }
            }
            main.post {
                result.onSuccess {
                    machine = it
                    progress = null
                    // The store may have just been imported (first run, distro switch)
                    Documents.rootsChanged(this)
                    updateWakeLock()
                    Tabs.markRunning(this)
                    restore?.let { saved -> restoreTabs(saved) }
                    restore = null
                    main.removeCallbacks(periodicSave)
                    main.postDelayed(periodicSave, SAVE_MS)
                    // Booting waits for the sysinit and wait commands (OpenRC starting services)
                    // without occupying worker: tabs open as usual
                    thread(name = "appine-boot") {
                        runCatching {
                            val argv = arrayOf("sh", "-c", Rootfs.PREFER_OSCLOCK)
                            Native.waitFor(it, Native.spawn(it, -1, "/bin/sh", argv, ConsolePty.ENVIRONMENT))
                        }.onFailure { e -> Log.w(TAG, "cannot prefer osclock", e) }
                        Native.boot(it)
                        // Boot finished: notify when OpenRC services did not start (the output is
                        // in boot.log, which nobody reads)
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
                        // The import failed while switching to another distro (the download was
                        // already verified, so most likely space ran out): the old store is already
                        // deleted, so fall back to the Alpine in the APK rather than leave the user
                        // facing an app that does not work
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

    /**
     * Records something to tell the user; a visible UI takes it at once ([takeNotice]), otherwise
     * it waits for the UI to come back.
     */
    private fun tell(text: String) {
        notice = text
        each { it.onNotice() }
    }

    /** Takes what there is to tell the user (given to one UI only). */
    fun takeNotice(): String? = notice.also { notice = null }

    /**
     * Switching distros or resetting the rootfs is possible: the kernel is running, or the start
     * failed (this is how a broken store is recovered).
     */
    val canSwitch: Boolean get() = switching == null && downloader == null && (machine != 0L || startError != null)

    /**
     * Switches to [distro] (from Settings; resetting the rootfs switches to the installed one). The
     * tarball is first downloaded and verified in the background while the old system keeps
     * running, which can be cancelled ([cancelSwitch]); only when the download is done are all
     * sessions ended, the kernel stopped and the store deleted, and the new one imported. If the
     * download fails the old system stays untouched.
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
                    // Already cancelled: the result is not wanted, it is just that switching is
                    // possible again now
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

    /** No switch after all: abort the download (keeping no partial file). */
    fun cancelSwitch() {
        val sw = switching ?: return
        sw.cancelled = true
        switching = null
        settingsChanged()
    }

    /**
     * After [switchTo] has downloaded: ends all sessions, stops the kernel (there is none if the
     * start failed), deletes the store and imports [distro]. Stopping the kernel and deleting files
     * are done on [worker].
     */
    private fun replace(distro: Rootfs.Distro) {
        // The old accounts were all deleted with the store
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

    /**
     * A mirror was chosen in Settings: write it into the sources (on [worker], as the store has to
     * be opened).
     */
    fun setMirror(mirror: String) {
        val distro = Rootfs.installed(this) ?: return
        if (machine == 0L || switching != null) return
        worker.execute {
            val written = runCatching { Rootfs.setMirror(this, distro, mirror) }
                .onFailure { Log.w(TAG, "cannot write the package sources", it) }
                .getOrDefault(true)
            main.post {
                if (!written) mirrorNotFound()
                settingsChanged()
            }
        }
    }

    /**
     * The sources contain no built-in mirror at all (all replaced by mirrors outside the list), so
     * the mirror chosen in Settings was not written: tell the user to edit them by hand. Since
     * Android 12 toasts have at most two lines, so no path is given.
     */
    private fun mirrorNotFound() {
        Toast.makeText(this, R.string.mirror_not_found, Toast.LENGTH_LONG).show()
    }

    /** "Automatic" was chosen in Settings: probe again and write the fastest. */
    fun autoMirror() {
        val distro = Rootfs.installed(this) ?: return
        if (machine == 0L || switching != null || mirrorThread?.isAlive == true) return
        mirrorThread = thread(name = "appine-mirrors") {
            val written = Rootfs.chooseMirror(this, distro)
            main.post {
                if (!written) mirrorNotFound()
                settingsChanged()
            }
        }
    }

    /**
     * The execution tier was changed in Settings (debug builds only): record it and restart the
     * kernel.
     */
    fun setTier(value: Int) {
        if (!BuildConfig.DEBUG || value == tier) return
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putInt(KEY_TIER, value).apply()
        restart()
    }

    private fun settingsChanged() {
        updateNotification()
        each { it.onSettingsChanged() }
    }

    /**
     * Keep running with the screen off (a switch in Settings and in the notification, off by
     * default, the android/service map).
     */
    val keepAwake: Boolean get() = getSharedPreferences(PREFS, MODE_PRIVATE).getBoolean(KEY_KEEP_AWAKE, false)

    fun setKeepAwake(on: Boolean) {
        getSharedPreferences(PREFS, MODE_PRIVATE).edit().putBoolean(KEY_KEEP_AWAKE, on).apply()
        updateWakeLock()
        settingsChanged()
    }

    /** The `PARTIAL_WAKE_LOCK` held while keeping running with the screen off. */
    private var wakeLock: PowerManager.WakeLock? = null

    /**
     * Holds or releases `PARTIAL_WAKE_LOCK` according to the switch and the kernel's state: held
     * while the switch is on and the kernel is running or shutting down ([exitAll], before the
     * service stops); otherwise the CPU sleeps after the screen goes off and the guest's processes
     * stop with it.
     *
     * The app always has a foreground service, so the lock stays effective in Doze:
     * `PowerManagerService` disables partial wake locks only for apps whose process state comes
     * after `BOUND_FOREGROUND_SERVICE`, and the network is let through by the same threshold
     * (`NetworkPolicyManager.FOREGROUND_THRESHOLD_STATE`), so no battery optimisation exemption is
     * needed.
     */
    // A switch the user turned on: held until the kernel stops or the user turns it off, with no
    // fixed duration, and not released within the same call
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
     * Exit all (the button in the notification, the guest's `poweroff`, or no guest processes left
     * after the last tab was closed): first shut down by inittab (OpenRC stops the services, so
     * PostgreSQL and the like need no crash recovery next time), and only then stop the service;
     * meanwhile the foreground service and the notification remain, so the app process is not
     * killed as a cached process.
     */
    fun exitAll() {
        if (exiting) return
        exiting = true
        // Activities release only in onStop, and the shutdown may be faster than that: treat them
        // as already gone, and a UI present after the shutdown means the user opened the app again
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
                // Normal exit: do not restore the tabs next time
                Tabs.clear(this)
                // The user opened the app again during the shutdown: start again, otherwise the UI
                // stays at "Starting…" forever
                if (listeners.isNotEmpty()) start(null) else stopSelf()
            }
        }
    }

    /**
     * Restart (the guest's `reboot`, or the execution tier changed in a debug build's Settings):
     * shut down, then start a new kernel (after which the Activity opens new sessions).
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
     * Stops using the current kernel: stops listening to network callbacks and polling, ends all
     * sessions, and returns the old handle. The handle is cleared first: otherwise, seeing the
     * kernel still there when the sessions end, the Activity would think the last tab was closed.
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
        for (s in sessions) s.release()
        sessions.clear()
        unshown.clear()
        sessionsChanged()
        return m
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_EXIT_ALL -> exitAll()
            ACTION_CANCEL_SWITCH -> cancelSwitch()
            ACTION_TOGGLE_KEEP_AWAKE -> setKeepAwake(!keepAwake)
            ACTION_COPY_REPORT -> copyReport(
                intent.getStringExtra(EXTRA_REPORT).orEmpty(),
                intent.getIntExtra(EXTRA_NOTIFICATION, 0),
            )
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        main.removeCallbacks(trimWhenHidden)
        main.removeCallbacks(poll)
        main.removeCallbacks(periodicSave)
        main.removeCallbacks(saveSoon)
        // The service stopped normally (after exitAll): do not restore next time. Being killed
        // never gets here
        Tabs.clear(this)
        if (watchingNetwork) getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(network)
        for (s in sessions) s.release()
        sessions.clear()
        val m = machine
        machine = 0
        // Stopped without going through exitAll: shut down anyway, best effort. It has to wait for
        // guest threads to exit, so not on the main thread
        if (m != 0L) worker.execute { Native.shutdown(m) }
        worker.shutdown()
        wakeLock?.release()
        wakeLock = null
        super.onDestroy()
    }

    /**
     * When host memory is short, drops the kernel's rebuildable caches at once (the Tier 1
     * translations kept in the pool take up to 70–80 MB). Since Android 14 the `RUNNING_*` levels
     * are no longer sent; the UI being hidden (`UI_HIDDEN`) is handled by [removeListener].
     */
    @Suppress("DEPRECATION") // RUNNING_LOW and RUNNING_CRITICAL are sent only before Android 14
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
        // Callbacks may interleave with onDestroy: once worker is shut down there is no need to
        // hand it over
        if (m != 0L) runCatching { worker.execute { Native.setRoutes(m, routes) } }
    }

    /**
     * Opens a new terminal session (a login shell), logged in as [account] and starting in [cwd]
     * (null is the home directory). The shell starts when the window size is first known.
     */
    internal fun createSession(account: String = login, cwd: String? = null): VtSession {
        val pty = ConsolePty(machine, ConsolePty.script(this), account, cwd)
        val session = VtSession(pty, machine, sessionEvents)
        sessions.add(session)
        sessionsChanged()
        return session
    }

    /** Closes a session: the shell receives SIGHUP. */
    internal fun removeSession(session: VtSession) {
        session.release()
        sessions.remove(session)
        unshown.remove(session)
        sessionsChanged()
    }

    /**
     * Names a tab ([VtSession.name]); a blank [name] clears it, so the program's title shows again.
     * Saved with the tabs soon after.
     */
    internal fun renameSession(session: VtSession, name: String) {
        if (session !in sessions) return
        session.name = name.trim().takeIf { it.isNotEmpty() }
        sessionsChanged()
    }

    private fun sessionsChanged() {
        updateNotification()
        each { it.onSessionsChanged() }
        main.removeCallbacks(saveSoon)
        if (machine != 0L) main.postDelayed(saveSoon, SAVE_SOON_MS)
    }

    /**
     * Restores the tabs from when the app was last killed (the android/service map): each starts a
     * new shell, logged in as the original account and starting in the original directory, showing
     * the original transcript and a line of notice first. Processes are not restored.
     */
    private fun restoreTabs(saved: Tabs.Saved) {
        val time = DateFormat.getDateTimeInstance(DateFormat.SHORT, DateFormat.SHORT).format(Date(saved.time))
        val notice = "\u001b[33m[appine] " + getString(R.string.restored_notice, time) + "\u001b[0m\r\n\r\n"
        for (tab in saved.tabs) {
            val session = createSession(tab.login, tab.cwd)
            session.name = tab.name
            val body = tab.transcript.trimEnd().replace("\r\n", "\n").replace("\n", "\r\n")
            session.setRestored(((if (body.isEmpty()) "" else body + "\r\n") + notice).toByteArray())
            unshown[session] = tab
        }
        focus = sessions.getOrNull(saved.current) ?: sessions.lastOrNull()
    }

    /**
     * Saves the tabs ([Tabs]): transcripts are taken on the main thread (the emulator changes them
     * on the main thread); the foreground process's current directory needs the BKL, so it is taken
     * on [worker] together with writing the files. Tabs that have ended are not saved.
     */
    private fun saveTabs() {
        val m = machine
        if (m == 0L) return
        main.removeCallbacks(saveSoon)
        val live = sessions.filter { it.isRunning }
        val snapshot = live.map { s ->
            val text = s.transcriptText
            // A restored tab not shown yet: its shell has not started, so save the original copy
            val old = if (text == null) unshown[s] else null.also { unshown.remove(s) }
            // The directory is filled in on the worker
            s to Tabs.Tab(s.login, old?.cwd, text ?: old?.transcript.orEmpty(), s.name)
        }
        val index = live.indexOf(current).coerceAtLeast(0)
        runCatching {
            worker.execute {
                val tabs = snapshot.map { (session, tab) ->
                    val console = session.console
                    if (console >= 0) tab.copy(cwd = Native.consoleCwd(m, console)) else tab
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
     * An entry of [Native.takeKills]: the kernel layer ended a process (kernel bug, engine fault,
     * OOM killer). When its controlling terminal is a tab, the reason is explained in that tab (the
     * shell only prints "Killed" or "Segmentation fault"); when it is not in a tab (a background
     * service), when the tab is in a full-screen program, or when the UI is gone, a notification is
     * posted. appine's own bugs always get a notification, with "Copy report" ([bugReport]) on it,
     * for the user to paste into feedback.
     */
    private fun reportKill(line: String) {
        val f = line.split('\t')
        if (f.size != 7) return
        val console = f[0].toIntOrNull() ?: return
        val pid = f[1].toIntOrNull() ?: return
        val comm = f[2]
        // memory.max's 16M is 16 MiB: use powers of 1024, otherwise the limit shows as 17 MB and
        // the overrun is not visible
        val size = { bytes: String ->
            val b = bytes.toLongOrNull() ?: 0
            if (b >= 1L shl 30) DecimalFormat("0.#").format(b / 1073741824.0) + " GiB"
            else DecimalFormat("0.#").format(b / 1048576.0) + " MiB"
        }
        val bug = f[3] == "bug"
        val text = when (f[3]) {
            "oom" -> if (f[4].isEmpty()) {
                getString(R.string.killed_oom_app, comm, pid, size(f[5]), size(f[6]))
            } else {
                getString(R.string.killed_oom_cgroup, comm, pid, f[4], size(f[5]), size(f[6]))
            }
            else -> getString(R.string.killed_bug, comm, pid)
        }
        val session = sessions.firstOrNull { it.console == console && console >= 0 }
        val shown = session?.showMessage("[appine] $text") == true
        if (shown && listeners.isNotEmpty() && !bug) return
        val id = KILL_NOTIFICATION_BASE + pid % 1000
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val builder = Notification.Builder(this, PROBLEMS_CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_notification)
            .setContentTitle(getString(R.string.killed_title, comm))
            .setContentText(text)
            .setStyle(Notification.BigTextStyle().bigText(text))
            .setContentIntent(open)
            .setAutoCancel(true)
        if (bug) {
            val copy = Intent(this, TerminalService::class.java).setAction(ACTION_COPY_REPORT)
                .putExtra(EXTRA_REPORT, bugReport(f[4], f[5]))
                .putExtra(EXTRA_NOTIFICATION, id)
            val pending = PendingIntent.getService(
                this, id, copy, PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
            )
            builder.addAction(Notification.Action.Builder(null, getString(R.string.copy_report), pending).build())
        }
        getSystemService(NotificationManager::class.java).notify(id, builder.build())
    }

    /**
     * The report copied for the user to paste into feedback: the app's version, the device, the
     * distro and the execution tier, plus the kernel layer's report (process, thread, last guest
     * pc, the panic message and location, the same as the line in `dmesg`), and the kernel log
     * entries before it and itself ([log] separated by U+001E, with timestamps as `dmesg` writes
     * them).
     */
    private fun bugReport(report: String, log: String): String = buildString {
        appendLine("appine ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE}, ${BuildConfig.BUILD_TYPE})")
        appendLine(
            "Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT}), ${Build.MANUFACTURER} ${Build.MODEL}, " +
                "${Build.SUPPORTED_ABIS.firstOrNull()}, page size ${Os.sysconf(OsConstants._SC_PAGESIZE)}",
        )
        appendLine("${Rootfs.installed(this@TerminalService)?.label ?: "unknown distribution"}, Tier $tier")
        append(report)
        if (log.isNotEmpty()) {
            append("\n\nKernel log:\n")
            append(log.split('\u001e').joinToString("\n"))
        }
    }

    /**
     * "Copy report" on the notification: puts it on the clipboard and dismisses that notification.
     * Since Android 13 the system shows its own copied confirmation.
     */
    private fun copyReport(report: String, notification: Int) {
        if (report.isEmpty()) return
        val clipboard = getSystemService(ClipboardManager::class.java)
        clipboard.setPrimaryClip(ClipData.newPlainText(getString(R.string.copy_report), report))
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) {
            Toast.makeText(this, R.string.report_copied, Toast.LENGTH_SHORT).show()
        }
        getSystemService(NotificationManager::class.java).cancel(notification)
    }

    /**
     * OpenRC services did not start at boot: posts a notification saying which ones and where the
     * output is.
     */
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

    /**
     * An entry of [Native.takeListens]: `<process name>\t<port>\t<host port>`. One notification per
     * port.
     */
    private fun notifyListen(line: String) {
        val fields = line.split('\t')
        if (fields.size != 3) return
        val comm = fields[0]
        val port = fields[1].toIntOrNull() ?: return
        val hostPort = fields[2].toIntOrNull() ?: return
        val text = if (hostPort != port) {
            getString(R.string.listening_mapped, hostPort, port)
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
            // A distro switch download: progress on the second line, cancellable from the
            // notification
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

        /**
         * The format of [Native.setRoutes]. Before Android 13 the route type cannot be told, so all
         * count as unicast.
         */
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
        /**
         * The boot failure notification; notifications of ended processes are numbered by pid
         * (offset from the listen ones, but a listen on port 2000–2999 gets the same id as a kill).
         */
        private const val BOOT_NOTIFICATION_ID = 2
        private const val KILL_NOTIFICATION_BASE = 3000
        private const val NOTIFICATION_ID = 1
        /** Listen notifications are numbered by port, offset from [NOTIFICATION_ID]. */
        private const val LISTEN_NOTIFICATION_BASE = 1000
        private const val POLL_MS = 2_000L
        private const val ACTION_EXIT_ALL = "io.github.mthli.appine.EXIT_ALL"
        private const val ACTION_CANCEL_SWITCH = "io.github.mthli.appine.CANCEL_SWITCH"
        private const val ACTION_TOGGLE_KEEP_AWAKE = "io.github.mthli.appine.TOGGLE_KEEP_AWAKE"
        private const val ACTION_COPY_REPORT = "io.github.mthli.appine.COPY_REPORT"
        private const val EXTRA_REPORT = "report"
        private const val EXTRA_NOTIFICATION = "notification"
        private const val KEY_KEEP_AWAKE = "keep_awake"
        private const val WAKE_LOCK_TAG = "appine:keep-awake"
        private const val PREFS = "settings"
        private const val KEY_TIER = "tier"
        private const val KEY_LOGIN = "login"
        private const val KEY_SYSTEM_DOCUMENTS = "system_documents"

        /**
         * The login account in Settings (see the instance's [login]); the DocumentsProvider needs
         * it too, without going through the service.
         */
        fun login(context: Context): String =
            context.getSharedPreferences(PREFS, MODE_PRIVATE).getString(KEY_LOGIN, null) ?: ConsolePty.LOGIN_AUTO

        /** See the instance's [systemDocuments]. */
        fun showSystemDocuments(context: Context): Boolean =
            context.getSharedPreferences(PREFS, MODE_PRIVATE).getBoolean(KEY_SYSTEM_DOCUMENTS, false)
        private const val TRIM_DELAY_MS = 60_000L
        private const val SAVE_MS = 60_000L
        private const val SAVE_SOON_MS = 2_000L
    }
}
