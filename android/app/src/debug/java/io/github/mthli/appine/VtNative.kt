package io.github.mthli.appine

import java.nio.ByteBuffer

/**
 * The JNI side of the experimental terminal view (`hosts/android/src/vt.rs`, the android/terminal
 * map): a libghostty-vt terminal per tab, fed from its kernel console inside the native library.
 * Present only when the native library was built with the `vt` feature (`-Pappine.vt=true`);
 * check [available] first.
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
     * closed; calls `view.onOutput()` (on this thread) when the screen changed after the last
     * [frame].
     */
    @JvmStatic external fun pump(tab: Long, view: VtView)

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

    /** Whether the program asked for bracketed paste. */
    @JvmStatic external fun bracketedPaste(tab: Long): Boolean

    /** Shows bytes from the app as if the console had printed them. */
    @JvmStatic external fun print(tab: Long, data: ByteArray)
}
