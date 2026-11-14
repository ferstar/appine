package io.github.mthli.appine

import android.app.Activity
import android.app.Application
import android.app.Dialog
import android.app.KeyguardManager
import android.app.NotificationManager
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.os.Bundle
import android.os.PowerManager
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.view.WindowManager.LayoutParams.FLAG_SECURE
import android.view.WindowManager.LayoutParams.MATCH_PARENT
import android.widget.Toast
import androidx.biometric.BiometricManager
import androidx.biometric.BiometricManager.Authenticators.BIOMETRIC_STRONG
import androidx.biometric.BiometricManager.Authenticators.DEVICE_CREDENTIAL
import androidx.biometric.BiometricPrompt
import androidx.core.content.ContextCompat
import androidx.core.graphics.drawable.toDrawable
import androidx.fragment.app.FragmentActivity
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProvider
import java.util.WeakHashMap

/** Process-local authentication. Neither saved instance state nor preferences can unlock the UI. */
internal object Privacy : Application.ActivityLifecycleCallbacks {
    interface Listener { fun onPrivacyUnlocked() }
    private const val AUTHENTICATORS = BIOMETRIC_STRONG or DEVICE_CREDENTIAL

    private val gates = mutableMapOf<Activity, Gate>()
    private val started = mutableSetOf<Activity>()
    private val listeners = mutableSetOf<() -> Unit>()
    private val dialogs = WeakHashMap<Dialog, Unit>()
    private var unlocked = false
    private var epoch = 0

    fun install(app: Application) {
        app.registerActivityLifecycleCallbacks(this)
        ContextCompat.registerReceiver(app, object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                if (intent.action != Intent.ACTION_SCREEN_OFF) return
                epoch++
                lock()
                gates.values.toList().forEach { it.cancel() }
            }
        }, IntentFilter(Intent.ACTION_SCREEN_OFF), ContextCompat.RECEIVER_NOT_EXPORTED)
    }

    fun locked(context: Context): Boolean = Settings.privacyMode(context) &&
        (!unlocked || !deviceAvailable(context) || gates.values.none { it.resumed })

    fun addListener(listener: () -> Unit) { listeners.add(listener) }
    fun removeListener(listener: () -> Unit) { listeners.remove(listener) }

    fun protect(dialog: Dialog) {
        dialogs[dialog] = Unit
        secure(dialog)
    }

    /** Called only after the settings confirmation; the preference changes only on authentication. */
    fun requestMode(activity: FragmentActivity, enabled: Boolean) {
        gates[activity]?.authenticate(if (enabled) Purpose.ENABLE else Purpose.DISABLE)
    }

    override fun onActivityPostCreated(activity: Activity, savedInstanceState: Bundle?) {
        if (activity is FragmentActivity) gates[activity] = Gate(activity).also { it.refresh() }
    }

    override fun onActivityPreStarted(activity: Activity) {
        started.add(activity)
        gates[activity]?.refresh()
    }

    override fun onActivityPostResumed(activity: Activity) {
        gates[activity]?.resume()
    }

    override fun onActivityPaused(activity: Activity) {
        gates[activity]?.pause()
    }

    override fun onActivityStopped(activity: Activity) {
        started.remove(activity)
        // Activity transitions overlap starts/stops. Recreation keeps the same authenticated run.
        if (started.isEmpty() && !activity.isChangingConfigurations) lock()
    }

    override fun onActivityDestroyed(activity: Activity) {
        gates.remove(activity)?.destroy()
        started.remove(activity)
    }

    override fun onActivityCreated(activity: Activity, savedInstanceState: Bundle?) = Unit
    override fun onActivityStarted(activity: Activity) = Unit
    override fun onActivityResumed(activity: Activity) = Unit
    override fun onActivitySaveInstanceState(activity: Activity, outState: Bundle) = Unit

    private fun deviceAvailable(context: Context) =
        context.getSystemService(PowerManager::class.java).isInteractive &&
            !context.getSystemService(KeyguardManager::class.java).isDeviceLocked

    private fun secure(dialog: Dialog) {
        dialog.window?.setFlags(if (Settings.privacyMode(dialog.context)) FLAG_SECURE else 0, FLAG_SECURE)
    }

    private fun changeMode(context: Context, enabled: Boolean) {
        Settings.setPrivacyMode(context, enabled)
        if (enabled) {
            Documents.revokeAccess(context)
            // Remove already-posted details as well as guarding future notifications.
            val manager = context.getSystemService(NotificationManager::class.java)
            manager.activeNotifications.filter { it.id != 1 }.forEach { manager.cancel(it.tag, it.id) }
        }
        Documents.rootsChanged(context)
        dialogs.keys.toList().forEach(::secure)
        listeners.toList().forEach { it() }
        gates.values.toList().forEach { it.refresh() }
    }

    private fun lock() {
        if (unlocked) epoch++
        unlocked = false
        gates.values.toList().forEach { it.cover() }
    }

    enum class Purpose { UNLOCK, ENABLE, DISABLE }

    /** AndroidX retains the prompt across recreation; retain its purpose and generation too. */
    class Request : ViewModel() {
        var purpose: Purpose? = null
        var epoch = 0
        var succeeded = false
    }

    private class Gate(private val activity: FragmentActivity) {
        private val request = ViewModelProvider(activity)[Request::class.java]
        private val content get() = activity.findViewById<View>(android.R.id.content)
        private var shield: Dialog? = null
        var resumed = false
            private set

        private val prompt = BiometricPrompt(activity, ContextCompat.getMainExecutor(activity),
            object : BiometricPrompt.AuthenticationCallback() {
                override fun onAuthenticationSucceeded(result: BiometricPrompt.AuthenticationResult) {
                    request.succeeded = true
                    complete()
                }

                override fun onAuthenticationError(errorCode: Int, errString: CharSequence) {
                    request.purpose = null
                    request.succeeded = false
                    if (resumed && !activity.isFinishing) {
                        Toast.makeText(activity, errString, Toast.LENGTH_SHORT).show()
                    }
                }
            })

        fun refresh() {
            val enabled = Settings.privacyMode(activity)
            activity.window.setFlags(if (enabled) FLAG_SECURE else 0, FLAG_SECURE)
            if (locked(activity)) cover() else if (resumed || !enabled) reveal()
        }

        fun resume() {
            resumed = true
            complete()
            if (!deviceAvailable(activity)) {
                cover()
                return
            }
            if (locked(activity)) {
                // Restored DialogFragments may have opened after onCreate. Cover their windows too.
                dismissShield()
                cover()
                if (request.purpose == null) authenticate(Purpose.UNLOCK)
            } else {
                reveal()
            }
        }

        fun pause() {
            // A paused page can remain visible. The last stop or screen-off locks the UI.
            resumed = false
        }

        fun destroy() = dismissShield()

        fun cover() {
            if (!Settings.privacyMode(activity) || activity.isFinishing || activity.isDestroyed) return
            content.visibility = View.INVISIBLE
            if (shield == null) shield = createShield()
        }

        private fun createShield(): Dialog =
            Dialog(activity, R.style.Theme_Appine_Terminal).apply {
                val parent = window!!.decorView.findViewById<ViewGroup>(android.R.id.content)
                val page = LayoutInflater.from(activity).inflate(R.layout.privacy_shield, parent, false)
                val column = page.findViewById<View>(R.id.privacy_content)
                (column.parent as View).addOnLayoutChangeListener { frame, _, _, _, _, _, _, _, _ ->
                    val width = minOf(activity.dp(360), frame.width - frame.paddingLeft - frame.paddingRight)
                    if (width > 0 && column.layoutParams.width != width) {
                        column.layoutParams = column.layoutParams.apply { this.width = width }
                    }
                }
                page.findViewById<View>(R.id.privacy_unlock).setOnClickListener { authenticate(Purpose.UNLOCK) }
                page.findViewById<View>(R.id.privacy_leave).setOnClickListener { activity.moveTaskToBack(true) }
                setContentView(page)
                setCancelable(false)
                window?.setBackgroundDrawable(activity.themeColor(com.google.android.material.R.attr.colorSurface).toDrawable())
                window?.addFlags(FLAG_SECURE)
                show()
                window?.setLayout(MATCH_PARENT, MATCH_PARENT)
            }

        private fun reveal() {
            dismissShield()
            content.visibility = View.VISIBLE
            if (resumed) (activity as? Listener)?.onPrivacyUnlocked()
        }

        private fun dismissShield() {
            shield?.dismiss()
            shield = null
        }

        fun authenticate(purpose: Purpose) {
            if (request.purpose != null || activity.isFinishing || activity.isDestroyed) return
            if (!deviceAvailable(activity)) return
            if (BiometricManager.from(activity).canAuthenticate(AUTHENTICATORS) != BiometricManager.BIOMETRIC_SUCCESS) {
                Toast.makeText(activity, R.string.privacy_unavailable, Toast.LENGTH_LONG).show()
                return
            }
            // Credential authentication may stop the activity. Lock before taking its generation.
            lock()
            request.purpose = purpose
            request.epoch = epoch
            request.succeeded = false
            prompt.authenticate(BiometricPrompt.PromptInfo.Builder()
                .setTitle(activity.getString(R.string.settings_privacy))
                .setSubtitle(activity.getString(R.string.privacy_authenticate))
                .setAllowedAuthenticators(AUTHENTICATORS)
                .build())
        }

        fun cancel() { if (request.purpose != null) prompt.cancelAuthentication() }

        private fun complete() {
            if (!request.succeeded || !resumed || !deviceAvailable(activity)) return
            val purpose = request.purpose ?: return
            request.purpose = null
            request.succeeded = false
            if (request.epoch != epoch) return
            unlocked = true
            when (purpose) {
                Purpose.ENABLE -> changeMode(activity, true)
                Purpose.DISABLE -> changeMode(activity, false)
                Purpose.UNLOCK -> Unit
            }
            gates.values.toList().forEach { it.refresh() }
        }
    }
}
