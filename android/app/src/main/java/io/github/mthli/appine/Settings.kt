package io.github.mthli.appine

import android.content.Context
import androidx.core.content.edit

/**
 * The settings kept in the `settings` preferences (the android/service map). Settings shows and
 * changes them through [TerminalService], which acts on each change; [Documents] and
 * [MainActivity] (before it binds the service) read them here.
 */
internal object Settings {
    private const val PREFS = "settings"
    private const val KEY_TIER = "tier"
    private const val KEY_LOGIN = "login"
    private const val KEY_KEEP_AWAKE = "keep_awake"
    private const val KEY_SYSTEM_DOCUMENTS = "system_documents"
    private const val KEY_FILL_WIDE = "fill_wide"
    private const val KEY_CURSOR_STYLE = "cursor_style"
    private const val KEY_CURSOR_BLINK = "cursor_blink"
    private const val KEY_VOLUME_KEYS_SWITCH_TABS = "volume_keys_switch_tabs"

    // The cursor shapes as recorded, Ghostty's cursor-style values
    private const val CURSOR_BLOCK = "block"
    private const val CURSOR_BAR = "bar"
    private const val CURSOR_UNDERLINE = "underline"

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    /**
     * The execution tier (see the engine map): 1 is Tier 1, 0 is the interpreter. A debug build
     * takes it from Settings, effective at the next kernel start; a release build always runs Tier
     * 1, ignoring a value recorded by an earlier debug install.
     */
    fun tier(context: Context): Int = if (BuildConfig.DEBUG) prefs(context).getInt(KEY_TIER, 1) else 1

    fun setTier(context: Context, value: Int) = prefs(context).edit { putInt(KEY_TIER, value) }

    /**
     * The login account of new tabs (the android/setup map): [ConsolePty.LOGIN_AUTO],
     * [ConsolePty.LOGIN_ROOT] or an account name. Goes back to automatic after a distro switch or
     * a rootfs reset ([clearLogin]).
     */
    fun login(context: Context): String = prefs(context).getString(KEY_LOGIN, null) ?: ConsolePty.LOGIN_AUTO

    fun setLogin(context: Context, value: String) = prefs(context).edit { putString(KEY_LOGIN, value) }

    /** Back to automatic: the accounts went with the store. */
    fun clearLogin(context: Context) = prefs(context).edit { remove(KEY_LOGIN) }

    /**
     * Keep running with the screen off (a switch in Settings and in the notification, off by
     * default, the android/service map).
     */
    fun keepAwake(context: Context): Boolean = prefs(context).getBoolean(KEY_KEEP_AWAKE, false)

    fun setKeepAwake(context: Context, on: Boolean) = prefs(context).edit { putBoolean(KEY_KEEP_AWAKE, on) }

    /**
     * Whether the Files app also shows the whole system, read-only (a switch in Settings, off by
     * default, the android/documents map).
     */
    fun systemDocuments(context: Context): Boolean = prefs(context).getBoolean(KEY_SYSTEM_DOCUMENTS, false)

    fun setSystemDocuments(context: Context, on: Boolean) = prefs(context).edit { putBoolean(KEY_SYSTEM_DOCUMENTS, on) }

    /**
     * Whether the terminal enlarges CJK characters to fill their two cells (a switch in Settings,
     * off by default, the android/vtview map); [MainActivity] reads it whenever it starts.
     */
    fun fillWide(context: Context): Boolean = prefs(context).getBoolean(KEY_FILL_WIDE, false)

    fun setFillWide(context: Context, on: Boolean) = prefs(context).edit { putBoolean(KEY_FILL_WIDE, on) }

    /**
     * The cursor's default shape, as Ghostty's cursor-style (a choice in Settings): a block by
     * default ([VtNative.CURSOR_BLOCK]), a bar or an underline. Every tab takes it at once, unless
     * a program chose its own cursor (DECSCUSR), which the program's reset brings back to it.
     */
    fun cursorStyle(context: Context): Int = when (prefs(context).getString(KEY_CURSOR_STYLE, null)) {
        CURSOR_BAR -> VtNative.CURSOR_BAR
        CURSOR_UNDERLINE -> VtNative.CURSOR_UNDERLINE
        else -> VtNative.CURSOR_BLOCK
    }

    fun setCursorStyle(context: Context, style: Int) {
        val name = when (style) {
            VtNative.CURSOR_BAR -> CURSOR_BAR
            VtNative.CURSOR_UNDERLINE -> CURSOR_UNDERLINE
            else -> CURSOR_BLOCK
        }
        prefs(context).edit { putString(KEY_CURSOR_STYLE, name) }
    }

    /**
     * Whether the cursor blinks by default, as Ghostty's cursor-style-blink (a switch in Settings,
     * on by default); programs may still choose otherwise.
     */
    fun cursorBlink(context: Context): Boolean = prefs(context).getBoolean(KEY_CURSOR_BLINK, true)

    fun setCursorBlink(context: Context, on: Boolean) = prefs(context).edit { putBoolean(KEY_CURSOR_BLINK, on) }

    /**
     * Whether the volume keys switch tabs in the terminal, up to the previous and down to the next
     * (a switch in Settings, on by default, the android/terminal map); off, they change the volume.
     * [MainActivity] reads it at each press.
     */
    fun volumeKeysSwitchTabs(context: Context): Boolean = prefs(context).getBoolean(KEY_VOLUME_KEYS_SWITCH_TABS, true)

    fun setVolumeKeysSwitchTabs(context: Context, on: Boolean) =
        prefs(context).edit { putBoolean(KEY_VOLUME_KEYS_SWITCH_TABS, on) }
}
