package io.github.mthli.appine

import java.nio.ByteBuffer

/**
 * The JNI side of the libghostty-vt terminal (`hosts/android/src/vt.rs`, the android/terminal map):
 * a terminal per [VtSession], fed from its kernel console inside the native library. Present only
 * when the native library was built with the `vt` feature (debug builds with `-Pappine.vt=true`);
 * check [available] first. Without it, tabs use the Termux emulator.
 */
internal object VtNative {
    init {
        // Loads libappine.so and initialises it
        Native.hashCode()
    }

    /** Whether the native library has these functions. */
    val available: Boolean by lazy {
        try {
            destroy(0)
            true
        } catch (_: UnsatisfiedLinkError) {
            false
        }
    }

    /** Creates the terminal of an open [console] of [machine]; returns the tab handle. */
    @JvmStatic external fun create(machine: Long, console: Int, cols: Int, rows: Int, cellWidth: Int, cellHeight: Int): Long

    /** Forgets a tab; a running [pump] keeps its terminal until it returns. */
    @JvmStatic external fun destroy(tab: Long)

    /**
     * Blocks, moving console output into the terminal until the session ends or the console is
     * closed; calls `session.onOutput()` (on this thread) when the screen changed after the last
     * [frame], and `session.onTitle()` when a program set the title.
     */
    @JvmStatic external fun pump(tab: Long, session: VtSession)

    /**
     * The rows that changed since the last frame, all of them with [full] (the frame format is in
     * `hosts/android/src/vt.c`); valid until the next call. Main thread only.
     */
    @JvmStatic external fun frame(tab: Long, full: Boolean): ByteBuffer?

    /** Resizes the terminal, then the console (SIGWINCH). */
    @JvmStatic external fun resize(tab: Long, cols: Int, rows: Int, cellWidth: Int, cellHeight: Int)

    /** Scrolls the viewport by [delta] rows (negative: back); 0 returns to the bottom. */
    @JvmStatic external fun scroll(tab: Long, delta: Int)

    /**
     * Encodes a key for the program ([VtKeys] has the key numbers and modifier bits); null when
     * it fails. [text] is what the key types without Ctrl and Alt.
     */
    @JvmStatic external fun key(tab: Long, key: Int, mods: Int, consumed: Int, action: Int, text: String?, unshifted: Int): ByteArray?

    /**
     * Encodes clipboard text for the program as its modes ask (bracketed paste, line breaks, unsafe
     * control bytes). Null when the text could run commands and [allowUnsafe] is false: ask, then
     * call again with it set. Empty when it failed.
     */
    @JvmStatic external fun paste(tab: Long, text: String, allowUnsafe: Boolean): ByteArray?

    /**
     * Encodes a mouse event at pixel ([x], [y]) of the grid in the program's tracking mode and
     * format: [action] 0 press, 1 release, 2 motion; [button] 0 none, 1 left, 4 and 5 the wheel;
     * [mods] as for [key]. Empty when the program does not take it, null when it fails.
     */
    @JvmStatic external fun mouse(tab: Long, action: Int, button: Int, mods: Int, x: Float, y: Float, pressed: Boolean): ByteArray?

    /** Encodes focus gained or lost; empty when the program did not ask for focus events. */
    @JvmStatic external fun focus(tab: Long, gained: Boolean): ByteArray?

    /**
     * Selects the word at viewport cell ([x], [y]) and anchors later [selectTo] calls to it;
     * false when there is no word there (the selection is then cleared).
     */
    @JvmStatic external fun selectWord(tab: Long, x: Int, y: Int): Boolean

    /**
     * Starts dragging the handle of the selection's last cell ([end]) or first cell: the other end
     * becomes the anchor of [selectTo]. False when there is no selection.
     */
    @JvmStatic external fun selectGrab(tab: Long, end: Boolean): Boolean

    /** Extends the selection from its anchor to viewport cell ([x], [y]). */
    @JvmStatic external fun selectTo(tab: Long, x: Int, y: Int): Boolean

    /** Selects everything, scrollback included. */
    @JvmStatic external fun selectAll(tab: Long): Boolean

    @JvmStatic external fun selectClear(tab: Long)

    /** The selection as plain text (soft wraps joined, trailing blanks trimmed), null for none. */
    @JvmStatic external fun copy(tab: Long): String?

    /** The title a program set (OSC 0/2); null when none. */
    @JvmStatic external fun title(tab: Long): String?

    /**
     * Shows a yellow line of app notice above the cursor's line (a kill report); false on the
     * alternate screen, where nothing is written.
     */
    @JvmStatic external fun message(tab: Long, text: String): Boolean

    /**
     * The primary screen's text with its scrollback (soft wraps joined, trailing blanks trimmed),
     * for saving tabs; null on the alternate screen or on failure.
     */
    @JvmStatic external fun transcript(tab: Long): String?

    /** Shows bytes from the app as if the console had printed them. */
    @JvmStatic external fun print(tab: Long, data: ByteArray)
}
