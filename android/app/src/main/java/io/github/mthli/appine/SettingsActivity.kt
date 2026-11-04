package io.github.mthli.appine

import android.app.Activity
import android.app.AlertDialog
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.graphics.Color
import android.os.Bundle
import android.os.IBinder
import android.util.TypedValue
import android.view.Gravity
import android.view.ViewGroup
import android.widget.LinearLayout
import android.widget.ScrollView
import android.widget.Switch
import android.widget.TextView
import android.widget.Toast
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import kotlin.concurrent.thread

/**
 * Settings: distro, package mirror, reset rootfs, execution tier (debug builds only; the
 * android/setup map), login account (likewise), keep running with the screen off (the
 * android/service map), open source licenses (design doc §13.4). All the state is in
 * [TerminalService]; this only displays and forwards it, and while the kernel is not running
 * (starting, replacing the store) everything can be viewed but not changed. When replacing the
 * store or a restart begins, it goes back to the terminal, which shows the progress.
 */
class SettingsActivity : Activity(), TerminalService.Listener {

    private var service: TerminalService? = null
    private lateinit var distro: Row
    private lateinit var mirror: Row
    private lateinit var reset: Row
    private lateinit var account: Row

    /**
     * The regular users in `/etc/passwd` ([Rootfs.regularAccounts]), read in the background each
     * time the settings page is shown.
     */
    private var accounts: List<Rootfs.Account> = emptyList()

    /** Only in debug builds: a release build always runs Tier 1 ([TerminalService.tier]). */
    private var tier: Row? = null
    private lateinit var keepAwake: Row
    private lateinit var systemDocuments: Row
    private lateinit var licenses: Row

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val s = (binder as TerminalService.LocalBinder).service
            service = s
            s.addListener(this@SettingsActivity)
            render()
            onNotice()
            loadAccounts()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            service = null
        }
    }

    /**
     * One settings row: a title, with the current value or a description below; a [toggle] row has
     * a switch on the right (tapping the whole row toggles it; the switch itself only displays).
     */
    private inner class Row(title: Int, toggle: Boolean = false, onClick: () -> Unit) {
        private val summary = TextView(this@SettingsActivity).apply {
            setTextColor(Color.GRAY)
            setTextSize(TypedValue.COMPLEX_UNIT_SP, 14f)
        }

        private val switch = if (toggle) {
            Switch(this@SettingsActivity).apply {
                isClickable = false
                isFocusable = false
            }
        } else {
            null
        }

        val view = LinearLayout(this@SettingsActivity).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setPadding(dp(20), dp(14), dp(20), dp(14))
            addView(LinearLayout(this@SettingsActivity).apply {
                orientation = LinearLayout.VERTICAL
                addView(TextView(this@SettingsActivity).apply {
                    setText(title)
                    setTextColor(Color.WHITE)
                    setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
                })
                addView(summary)
            }, LinearLayout.LayoutParams(0, ViewGroup.LayoutParams.WRAP_CONTENT, 1f))
            switch?.let { addView(it) }
            val ripple = TypedValue()
            theme.resolveAttribute(android.R.attr.selectableItemBackground, ripple, true)
            setBackgroundResource(ripple.resourceId)
            setOnClickListener { onClick() }
        }

        fun set(text: CharSequence, enabled: Boolean, checked: Boolean = false) {
            summary.text = text
            switch?.isChecked = checked
            view.isEnabled = enabled
            view.alpha = if (enabled) 1f else 0.4f
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        distro = Row(R.string.settings_distro) { onDistro() }
        mirror = Row(R.string.settings_mirror) { onMirror() }
        reset = Row(R.string.settings_reset) { onReset() }
        account = Row(R.string.settings_login) { onLogin() }
        if (BuildConfig.DEBUG) tier = Row(R.string.settings_tier) { onTier() }
        keepAwake = Row(R.string.settings_keep_awake, toggle = true) { service?.let { it.setKeepAwake(!it.keepAwake) } }
        systemDocuments = Row(R.string.settings_system_documents, toggle = true) {
            service?.let { it.setSystemDocuments(!it.systemDocuments) }
        }
        // AGPL sections 0 and 5's "Appropriate Legal Notices": copyright, no warranty, the license,
        // and where to read the full text
        licenses = Row(R.string.settings_licenses) { startActivity(Intent(this, LicensesActivity::class.java)) }
        licenses.set(getString(R.string.licenses_summary), true)

        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            gravity = Gravity.CENTER_VERTICAL
            setBackgroundColor(MainActivity.BAR_COLOR)
            addView(TextView(this@SettingsActivity).apply {
                text = "←"
                setTextColor(Color.LTGRAY)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 18f)
                setPadding(dp(16), dp(10), dp(16), dp(10))
                setOnClickListener { finish() }
            })
            addView(TextView(this@SettingsActivity).apply {
                setText(R.string.settings)
                setTextColor(Color.WHITE)
                setTextSize(TypedValue.COMPLEX_UNIT_SP, 16f)
            })
        }
        val list = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            for (row in listOfNotNull(distro, mirror, reset, account, keepAwake, systemDocuments, tier, licenses)) {
                addView(row.view)
            }
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            setBackgroundColor(MainActivity.BAR_COLOR)
            addView(bar)
            addView(ScrollView(this@SettingsActivity).apply {
                setBackgroundColor(Color.BLACK)
                addView(list)
            }, LinearLayout.LayoutParams(ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f))
        }
        WindowCompat.setDecorFitsSystemWindows(window, false)
        ViewCompat.setOnApplyWindowInsetsListener(root) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars())
            v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
            WindowInsetsCompat.CONSUMED
        }
        setContentView(root)
    }

    override fun onStart() {
        super.onStart()
        val intent = Intent(this, TerminalService::class.java)
        startForegroundService(intent)
        bindService(intent, connection, BIND_AUTO_CREATE)
    }

    override fun onStop() {
        service?.removeListener(this)
        unbindService(connection)
        service = null
        super.onStop()
    }

    // ---- TerminalService.Listener ----

    override fun onMachineReady(error: String?) {
        render()
        loadAccounts()
    }

    override fun onSettingsChanged() = render()

    /**
     * Replacing the store (importing a new distro) begins: go back to the terminal to see the
     * progress.
     */
    override fun onProgress(text: String) = finish()

    override fun onRestarting() = finish()

    override fun onExitAll() = finish()

    /** The download for a distro switch failed. */
    override fun onNotice() {
        val text = service?.takeNotice() ?: return
        AlertDialog.Builder(this)
            .setMessage(text)
            .setPositiveButton(android.R.string.ok, null)
            .show()
    }

    // ---- Display ----

    private fun render() {
        val s = service ?: return
        val running = s.machine != 0L
        val sw = s.switching
        val installed = Rootfs.installed(this)
        val idle = running && sw == null
        distro.set(
            when {
                sw != null -> getString(R.string.switch_downloading, sw.distro.label, sw.done shr 20, sw.total shr 20)
                running || s.startError != null -> installed?.label.orEmpty()
                else -> s.progress ?: getString(R.string.starting)
            },
            sw != null || s.canSwitch,
        )
        val current = Rootfs.mirror(this)
        mirror.set(
            when {
                s.measuring -> getString(R.string.mirror_measuring)
                current == null -> ""
                Rootfs.isMirrorManual(this) -> Rootfs.host(current)
                else -> getString(R.string.mirror_measured, Rootfs.host(current))
            },
            idle && !s.measuring,
        )
        // Reset is possible even when the start failed: it is the only way to recover a broken
        // store
        reset.set(getString(R.string.reset_summary, (installed ?: Rootfs.Distro.ALPINE).label), s.canSwitch)
        tier?.set(getString(if (s.tier == 0) R.string.tier_0 else R.string.tier_1), idle)
        account.set(loginSummary(s.login), installed != null && sw == null)
        // Can be changed at any time: while the kernel is not running it is only recorded, and the
        // lock is held accordingly once it is up
        keepAwake.set(getString(if (s.keepAwake) R.string.keep_awake_on else R.string.keep_awake_off), true, s.keepAwake)
        systemDocuments.set(getString(R.string.system_documents_summary), true, s.systemDocuments)
    }

    // ---- Actions ----

    private fun onDistro() {
        val s = service ?: return
        s.switching?.let { sw ->
            AlertDialog.Builder(this)
                .setTitle(getString(R.string.cancel_download_title, sw.distro.label))
                .setPositiveButton(R.string.cancel_download) { _, _ -> s.cancelSwitch() }
                .setNegativeButton(R.string.keep_downloading, null)
                .show()
            return
        }
        val distros = Rootfs.Distro.entries
        val installed = Rootfs.installed(this)
        val labels = distros.map { d ->
            d.download?.let { getString(R.string.distro_download, d.label, it.size shr 20) } ?: d.label
        }
        AlertDialog.Builder(this)
            .setTitle(R.string.settings_distro)
            .setSingleChoiceItems(labels.toTypedArray(), distros.indexOf(installed)) { dialog, i ->
                dialog.dismiss()
                if (distros[i] != installed) confirmSwitch(distros[i], getString(R.string.switch_title, distros[i].label))
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    /** When the very first import failed (no store), resets to Alpine, as on the first run. */
    private fun onReset() {
        val installed = Rootfs.installed(this) ?: Rootfs.Distro.ALPINE
        confirmSwitch(installed, getString(R.string.reset_title))
    }

    /**
     * Confirms before switching to [distro] (a reset switches to the installed one): every file in
     * the rootfs is deleted.
     */
    private fun confirmSwitch(distro: Rootfs.Distro, title: String) {
        val download = distro.download
        val message = if (download != null) {
            getString(R.string.switch_message_download, distro.label, download.size shr 20)
        } else {
            getString(R.string.switch_message, distro.label)
        }
        AlertDialog.Builder(this)
            .setTitle(title)
            .setMessage(message)
            .setPositiveButton(R.string.switch_confirm) { _, _ -> service?.switchTo(distro) }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun onMirror() {
        val distro = Rootfs.installed(this) ?: return
        val current = Rootfs.mirror(this)
        val checked = if (Rootfs.isMirrorManual(this)) distro.mirrors.indexOf(current) + 1 else 0
        val labels = listOf(getString(R.string.mirror_auto)) + distro.mirrors.map(Rootfs::host)
        AlertDialog.Builder(this)
            .setTitle(R.string.settings_mirror)
            .setSingleChoiceItems(labels.toTypedArray(), checked) { dialog, i ->
                dialog.dismiss()
                if (i == 0) service?.autoMirror() else service?.setMirror(distro.mirrors[i - 1])
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun loadAccounts() {
        thread(name = "appine-accounts") {
            val list = Rootfs.regularAccounts(this)
            runOnUiThread {
                accounts = list
                render()
            }
        }
    }

    private fun loginSummary(login: String): String = when (login) {
        ConsolePty.LOGIN_AUTO -> accounts.firstOrNull()?.let { getString(R.string.login_auto_user, it.name) }
            ?: getString(R.string.login_auto_root)
        ConsolePty.LOGIN_ROOT -> login
        else -> if (accounts.any { it.name == login }) login else getString(R.string.login_missing, login)
    }

    /**
     * Chooses the login account for new tabs: automatic, root, or a regular user in `/etc/passwd`.
     */
    private fun onLogin() {
        val s = service ?: return
        val values = listOf(ConsolePty.LOGIN_AUTO, ConsolePty.LOGIN_ROOT) + accounts.map { it.name }
        val labels = listOf(getString(R.string.login_auto_choice), ConsolePty.LOGIN_ROOT) + accounts.map { it.name }
        AlertDialog.Builder(this)
            .setTitle(R.string.settings_login)
            .setSingleChoiceItems(labels.toTypedArray(), values.indexOf(s.login)) { dialog, i ->
                dialog.dismiss()
                s.setLogin(values[i])
                val who = if (values[i] == ConsolePty.LOGIN_AUTO) {
                    accounts.firstOrNull()?.name ?: ConsolePty.LOGIN_ROOT
                } else {
                    values[i]
                }
                Toast.makeText(this, getString(R.string.login_changed, who), Toast.LENGTH_SHORT).show()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun onTier() {
        val s = service ?: return
        val tiers = intArrayOf(1, 0)
        val labels = arrayOf(getString(R.string.tier_1), getString(R.string.tier_0))
        AlertDialog.Builder(this)
            .setTitle(R.string.settings_tier)
            .setSingleChoiceItems(labels, tiers.indexOf(s.tier)) { dialog, i ->
                dialog.dismiss()
                if (tiers[i] == s.tier) return@setSingleChoiceItems
                AlertDialog.Builder(this)
                    .setTitle(R.string.restart_title)
                    .setMessage(R.string.restart_message)
                    .setPositiveButton(R.string.restart) { _, _ -> service?.setTier(tiers[i]) }
                    .setNegativeButton(android.R.string.cancel, null)
                    .show()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun dp(v: Int) = (v * resources.displayMetrics.density).toInt()
}
