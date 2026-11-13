package io.github.mthli.appine

import android.app.Dialog
import android.content.ComponentName
import android.content.Context
import android.content.DialogInterface
import android.content.Intent
import android.content.ServiceConnection
import android.os.Bundle
import android.os.IBinder
import android.view.View
import android.widget.Toast
import androidx.activity.enableEdgeToEdge
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.updatePadding
import androidx.preference.ListPreference
import androidx.preference.ListPreferenceDialogFragmentCompat
import androidx.preference.Preference
import androidx.preference.PreferenceCategory
import androidx.preference.PreferenceFragmentCompat
import androidx.preference.PreferenceScreen
import androidx.preference.SwitchPreferenceCompat
import androidx.recyclerview.widget.RecyclerView
import com.google.android.material.appbar.MaterialToolbar
import com.google.android.material.color.DynamicColors
import com.google.android.material.dialog.MaterialAlertDialogBuilder
import kotlin.concurrent.thread

/** Material 3 frame for the standard AndroidX settings list. */
class SettingsActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        DynamicColors.applyToActivityIfAvailable(this)
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        setContentView(R.layout.page)
        findViewById<MaterialToolbar>(R.id.toolbar).apply {
            setTitle(R.string.settings)
            setNavigationOnClickListener { finish() }
        }
        if (savedInstanceState == null) {
            supportFragmentManager.beginTransaction()
                .replace(R.id.content, SettingsFragment())
                .commitNow()
        }
        val appBar = findViewById<View>(R.id.app_bar)
        ViewCompat.setOnApplyWindowInsetsListener(findViewById(R.id.root)) { v, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            v.setPadding(bars.left, 0, bars.right, 0)
            appBar.updatePadding(top = bars.top)
            // The fragment applies the bottom inset to its RecyclerView.
            insets
        }
    }
}

/**
 * Settings only displays and forwards TerminalService state. Preferences never persist values:
 * service setters also update wake locks, document roots and running terminals. Destructive
 * operations still ask first and return to the terminal when replacement or restart begins.
 */
class SettingsFragment : PreferenceFragmentCompat(), TerminalService.Listener {

    private var service: TerminalService? = null
    private var insetsApplied = false
    private lateinit var distro: Preference
    private lateinit var reset: Preference
    private lateinit var account: ListPreference

    /**
     * The regular users in `/etc/passwd` ([Rootfs.regularAccounts]), read in the background each
     * time the settings page is shown.
     */
    private var accounts: List<Rootfs.Account> = emptyList()
    private var accountLoad = 0

    private lateinit var tier: Preference
    private lateinit var keepAwake: SwitchPreferenceCompat
    private lateinit var systemDocuments: SwitchPreferenceCompat
    private lateinit var fillWide: SwitchPreferenceCompat
    private lateinit var cursorStyle: ListPreference
    private lateinit var cursorBlink: SwitchPreferenceCompat
    private lateinit var volumeKeys: SwitchPreferenceCompat
    private lateinit var licenses: Preference

    private val connection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            val s = (binder as TerminalService.LocalBinder).service
            service = s
            s.addListener(this@SettingsFragment)
            render()
            onNotice()
            loadAccounts()
        }

        override fun onServiceDisconnected(name: ComponentName) {
            service = null
            render()
        }
    }

    override fun onCreatePreferences(savedInstanceState: Bundle?, rootKey: String?) {
        preferenceScreen = preferenceManager.createPreferenceScreen(requireContext())
        distro = action(R.string.settings_distro) { onDistro() }
        reset = action(R.string.settings_reset) { onReset() }
        account = choice(R.string.settings_login) { s, value ->
            s.setLogin(value)
            val who = if (value == ConsolePty.LOGIN_AUTO) {
                accounts.firstOrNull()?.name ?: ConsolePty.LOGIN_ROOT
            } else {
                value
            }
            Toast.makeText(requireContext(), getString(R.string.login_changed, who), Toast.LENGTH_SHORT).show()
        }
        tier = action(R.string.settings_tier) { onTier() }
        keepAwake = toggle(R.string.settings_keep_awake, TerminalService::setKeepAwake)
        systemDocuments = toggle(R.string.settings_system_documents, TerminalService::setSystemDocuments)
        fillWide = toggle(R.string.settings_fill_wide, TerminalService::setFillWide)
        cursorStyle = choice(R.string.settings_cursor_style) { s, value -> s.setCursorStyle(value.toInt()) }.apply {
            entries = CURSOR_LABELS.map { getString(it) }.toTypedArray()
            entryValues = CURSOR_STYLES.map { it.toString() }.toTypedArray()
        }
        cursorBlink = toggle(R.string.settings_cursor_blink, TerminalService::setCursorBlink)
        volumeKeys = toggle(R.string.settings_volume_keys, TerminalService::setVolumeKeysSwitchTabs)
        // AGPL sections 0 and 5's "Appropriate Legal Notices": copyright, no warranty, the license,
        // and where to read the full text.
        licenses = action(R.string.settings_licenses) {
            startActivity(Intent(requireContext(), LicensesActivity::class.java))
        }
        licenses.setSummary(R.string.licenses_summary)

        group(R.string.settings_group_system, distro, account, reset)
        group(R.string.settings_group_android, keepAwake, systemDocuments)
        group(R.string.settings_group_terminal, fillWide, cursorStyle, cursorBlink, volumeKeys)
        group(R.string.settings_group_debug, tier)
        group(R.string.settings_group_about, licenses)
        render()
    }

    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        insetsApplied = false
        super.onViewCreated(view, savedInstanceState)
        listView.itemAnimator = null
        listView.clipToPadding = false
        ViewCompat.setOnApplyWindowInsetsListener(view) { _, insets ->
            val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout())
            listView.updatePadding(bottom = bars.bottom)
            insetsApplied = true
            allowStateRestoration()
            WindowInsetsCompat.CONSUMED
        }
        ViewCompat.requestApplyInsets(view)
    }

    override fun onCreateAdapter(preferenceScreen: PreferenceScreen): RecyclerView.Adapter<*> =
        super.onCreateAdapter(preferenceScreen).apply {
            // Empty summaries make rows shorter before binding; restoring now would clamp the
            // saved scroll position to that temporary, shorter list. The bottom inset must also
            // be applied before restoring a position anchored at the list's end.
            stateRestorationPolicy = if (service == null || !insetsApplied) {
                RecyclerView.Adapter.StateRestorationPolicy.PREVENT
            } else {
                RecyclerView.Adapter.StateRestorationPolicy.ALLOW
            }
        }

    private fun allowStateRestoration() {
        if (service != null && insetsApplied && view != null) {
            listView.adapter?.stateRestorationPolicy = RecyclerView.Adapter.StateRestorationPolicy.ALLOW
        }
    }

    override fun onStart() {
        super.onStart()
        val context = requireContext()
        val intent = Intent(context, TerminalService::class.java)
        context.startForegroundService(intent)
        context.bindService(intent, connection, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        service?.removeListener(this)
        requireContext().unbindService(connection)
        service = null
        accountLoad++
        render()
        super.onStop()
    }

    /** Stable keys let AndroidX restore list/dialog state without owning the setting's storage. */
    private fun <T : Preference> T.configure(title: Int): T = apply {
        key = resources.getResourceEntryName(title)
        setTitle(title)
        isPersistent = false
        isIconSpaceReserved = false
        isSingleLineTitle = false
    }

    private fun action(title: Int, onClick: () -> Unit): Preference =
        Preference(requireContext()).configure(title).apply {
            setOnPreferenceClickListener {
                onClick()
                true
            }
        }

    private fun toggle(title: Int, change: (TerminalService, Boolean) -> Unit): SwitchPreferenceCompat =
        SwitchPreferenceCompat(requireContext()).configure(title).apply {
            widgetLayoutResource = R.layout.preference_switch
            setOnPreferenceChangeListener { _, value ->
                val s = service ?: return@setOnPreferenceChangeListener false
                change(s, value as Boolean)
                true
            }
        }

    private fun choice(title: Int, change: (TerminalService, String) -> Unit): ListPreference =
        ListPreference(requireContext()).configure(title).apply {
            setDialogTitle(title)
            setOnPreferenceChangeListener { _, value ->
                val s = service ?: return@setOnPreferenceChangeListener false
                if (!isEnabled) return@setOnPreferenceChangeListener false
                change(s, value as String)
                true
            }
        }

    private fun group(heading: Int, vararg rows: Preference) {
        val category = PreferenceCategory(requireContext()).configure(heading)
        preferenceScreen.addPreference(category)
        for (row in rows) category.addPreference(row)
    }

    override fun onDisplayPreferenceDialog(preference: Preference) {
        if (preference !is ListPreference) {
            super.onDisplayPreferenceDialog(preference)
            return
        }
        if (parentFragmentManager.findFragmentByTag(CHOICE_DIALOG) != null) return
        ChoiceDialog.newInstance(preference.key).apply {
            // Preference's dialog contract still uses the target fragment to find its preference.
            @Suppress("DEPRECATION")
            setTargetFragment(this@SettingsFragment, 0)
        }.show(parentFragmentManager, CHOICE_DIALOG)
    }

    /** AndroidX owns selection and saved state; only the builder changes for Material 3. */
    class ChoiceDialog : ListPreferenceDialogFragmentCompat() {
        override fun onCreateDialog(savedInstanceState: Bundle?): Dialog {
            val builder = MaterialAlertDialogBuilder(requireContext())
                .setTitle(preference.dialogTitle)
                .setNegativeButton(preference.negativeButtonText, this)
            onPrepareDialogBuilder(builder)
            return builder.create().also {
                // Back, Cancel and an outside tap must never commit the current selection.
                onClick(it, DialogInterface.BUTTON_NEGATIVE)
            }
        }

        companion object {
            fun newInstance(key: String) = ChoiceDialog().apply {
                arguments = Bundle().apply { putString(ARG_KEY, key) }
            }
        }
    }

    // ---- TerminalService.Listener ----

    /**
     * Replacing the store (importing a new distro) begins: go back to the terminal to see the
     * progress.
     */
    override fun onProgress(text: String) = requireActivity().finish()

    override fun onMachineReady(error: String?) {
        render()
        loadAccounts()
    }

    override fun onSettingsChanged() = render()

    /** The download for a distro switch failed. */
    override fun onNotice() {
        val text = service?.takeNotice() ?: return
        MaterialAlertDialogBuilder(requireContext())
            .setMessage(text)
            .setPositiveButton(android.R.string.ok, null)
            .show()
    }

    override fun onRestarting() = requireActivity().finish()

    override fun onExitAll() = requireActivity().finish()

    // ---- Display ----

    private fun render() {
        val s = service
        if (s == null) {
            for (row in listOf(distro, reset, account, tier, keepAwake, systemDocuments,
                fillWide, cursorStyle, cursorBlink, volumeKeys)) row.isEnabled = false
            return
        }
        val running = s.machine != 0L
        val sw = s.switching
        val installed = Rootfs.installed(requireContext())
        val idle = running && sw == null
        distro.set(
            when {
                sw != null -> getString(R.string.switch_downloading, sw.distro.label, sw.done shr 20, sw.total shr 20)
                running || s.startError != null -> installed?.label.orEmpty()
                else -> s.progress ?: getString(R.string.starting)
            },
            sw != null || s.canSwitch,
        )
        // Reset is possible even when the start failed: it is the only way to recover a broken
        // store
        reset.set(getString(R.string.reset_summary), s.canSwitch)
        tier.set(getString(if (s.tier == 0) R.string.tier_0_summary else R.string.tier_1_summary), idle)
        account.entries = (listOf(getString(R.string.login_auto_choice), ConsolePty.LOGIN_ROOT) + accounts.map { it.name }).toTypedArray()
        account.entryValues = (listOf(ConsolePty.LOGIN_AUTO, ConsolePty.LOGIN_ROOT) + accounts.map { it.name }).toTypedArray()
        account.value = s.login
        account.set(loginSummary(s.login), installed != null && sw == null)
        // Can be changed at any time: while the kernel is not running it is only recorded, and the
        // lock is held accordingly once it is up
        keepAwake.set(getString(if (s.keepAwake) R.string.keep_awake_on else R.string.keep_awake_off), true, s.keepAwake)
        systemDocuments.set(getString(R.string.system_documents_summary), true, s.systemDocuments)
        fillWide.set(getString(if (s.fillWide) R.string.fill_wide_on else R.string.fill_wide_off), true, s.fillWide)
        cursorStyle.value = s.cursorStyle.toString()
        cursorStyle.set(getString(CURSOR_LABELS[CURSOR_STYLES.indexOf(s.cursorStyle).coerceAtLeast(0)]), true)
        cursorBlink.set(getString(if (s.cursorBlink) R.string.cursor_blink_on else R.string.cursor_blink_off), true, s.cursorBlink)
        volumeKeys.set(
            getString(if (s.volumeKeysSwitchTabs) R.string.volume_keys_on else R.string.volume_keys_off),
            true,
            s.volumeKeysSwitchTabs,
        )
        allowStateRestoration()
    }

    private fun Preference.set(text: CharSequence, enabled: Boolean) {
        summary = text
        isEnabled = enabled
    }

    private fun SwitchPreferenceCompat.set(text: CharSequence, enabled: Boolean, checked: Boolean) {
        set(text, enabled)
        isChecked = checked
    }

    private fun loginSummary(login: String): String = when (login) {
        ConsolePty.LOGIN_AUTO -> getString(R.string.login_auto, accounts.firstOrNull()?.name ?: ConsolePty.LOGIN_ROOT)
        ConsolePty.LOGIN_ROOT -> login
        else -> if (accounts.any { it.name == login }) login else getString(R.string.login_missing, login)
    }

    private fun loadAccounts() {
        val context = requireContext().applicationContext
        val activity = requireActivity()
        val generation = ++accountLoad
        thread(name = "appine-accounts") {
            val list = Rootfs.regularAccounts(context)
            activity.runOnUiThread {
                if (generation == accountLoad && service != null) {
                    accounts = list
                    render()
                }
            }
        }
    }

    // ---- Actions ----

    private fun onDistro() {
        val s = service ?: return
        s.switching?.let { sw ->
            MaterialAlertDialogBuilder(requireContext())
                .setTitle(getString(R.string.cancel_download_title, sw.distro.label))
                .setPositiveButton(R.string.cancel_download) { _, _ -> s.cancelSwitch() }
                .setNegativeButton(R.string.keep_downloading, null)
                .show()
            return
        }
        val distros = Rootfs.Distro.entries
        val installed = Rootfs.installed(requireContext())
        val labels = distros.map { it.label }
        MaterialAlertDialogBuilder(requireContext())
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
        val installed = Rootfs.installed(requireContext()) ?: Rootfs.Distro.ALPINE
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
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(title)
            .setMessage(message)
            .setPositiveButton(R.string.switch_confirm) { _, _ -> service?.switchTo(distro) }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    private fun onTier() {
        val s = service ?: return
        val tiers = intArrayOf(1, 0)
        val labels = arrayOf(getString(R.string.tier_1), getString(R.string.tier_0))
        MaterialAlertDialogBuilder(requireContext())
            .setTitle(R.string.settings_tier)
            .setSingleChoiceItems(labels, tiers.indexOf(s.tier)) { dialog, i ->
                dialog.dismiss()
                if (tiers[i] == s.tier) return@setSingleChoiceItems
                MaterialAlertDialogBuilder(requireContext())
                    .setTitle(R.string.restart_title)
                    .setMessage(R.string.restart_message)
                    .setPositiveButton(R.string.restart) { _, _ -> service?.setTier(tiers[i]) }
                    .setNegativeButton(android.R.string.cancel, null)
                    .show()
            }
            .setNegativeButton(android.R.string.cancel, null)
            .show()
    }

    companion object {
        private const val CHOICE_DIALOG = "settings.choice"

        /** The cursor shapes offered, in order, and their names. */
        private val CURSOR_STYLES = intArrayOf(VtNative.CURSOR_BLOCK, VtNative.CURSOR_BAR, VtNative.CURSOR_UNDERLINE)
        private val CURSOR_LABELS = intArrayOf(R.string.cursor_block, R.string.cursor_bar, R.string.cursor_underline)
    }
}
