package io.github.mthli.appine

import android.graphics.Bitmap
import java.nio.ByteBuffer

/**
 * The JNI side of the libghostty-vt terminal (`hosts/android/src/vt.rs`, the android/terminal map):
 * a terminal per [VtSession], fed from its kernel console inside the native library.
 */
internal object VtNative {
    init {
        // Loads libappine.so and initialises it
        Native.hashCode()
    }

    /** Creates the terminal of an open [console] of [machine]; returns the tab handle. */
    @JvmStatic external fun create(machine: Long, console: Int, cols: Int, rows: Int, cellWidth: Int, cellHeight: Int): Long

    /**
     * Creates the terminal of an open [console] of [machine] from a tab's saved [snapshot]
     * (screens, scrollback, images, colours), settled for a new shell: what the programs that ran
     * in it set up for themselves (modes, the alternate screen, the title) is undone and the cursor
     * starts a line of its own. Returns the tab handle, 0 when the snapshot cannot be decoded.
     */
    @JvmStatic external fun restore(machine: Long, console: Int, snapshot: ByteArray, cols: Int, rows: Int, cellWidth: Int, cellHeight: Int): Long

    /** Shows bytes from the app as if the console had printed them. */
    @JvmStatic external fun print(tab: Long, data: ByteArray)

    /**
     * Blocks, moving console output into the terminal until the session ends or the console is
     * closed; calls `session.onOutput()` (on this thread) when the screen changed after the last
     * [frame], `session.onTitle()` when a program set the title, the session's `onBell`,
     * `onClipboard`, `onNotification`, `onProgress`, `onStatus` and `onClipboardRead` for what
     * programs asked of the app, and `onForeground()` once the process group in the foreground when
     * progress or a status was last reported has given the terminal back.
     */
    @JvmStatic external fun pump(tab: Long, session: VtSession)

    /** The title a program set (OSC 0/2); null when none. */
    @JvmStatic external fun title(tab: Long): String?

    /**
     * Whether the shell is at its prompt or in the input typed at it, so no command runs in the
     * foreground, as the shell's prompt marks (OSC 133, `/etc/profile.d/appine.sh`) tell; false
     * without them and on the alternate screen.
     */
    @JvmStatic external fun atPrompt(tab: Long): Boolean

    /**
     * The rows that changed since the last frame, all of them with [full] (the frame format is in
     * `hosts/android/src/vt.c`); valid until the next call. Main thread only.
     */
    @JvmStatic external fun frame(tab: Long, full: Boolean): ByteBuffer?

    /**
     * Copies the pixels of the Kitty image [id] at [generation], as a [frame] named it, into
     * [bitmap] (mutable, `ARGB_8888`, the image's size). False when the image is no longer that
     * generation or size (a later frame names the new one), its pixels have not all arrived, or the
     * bitmap does not fit.
     */
    @JvmStatic external fun image(tab: Long, id: Int, generation: Long, bitmap: Bitmap): Boolean

    /**
     * Resizes the terminal, then the console (SIGWINCH); returns the terminal's replies for the
     * console, such as the size report of mode 2048 (empty when none, null when the tab is gone).
     */
    @JvmStatic external fun resize(tab: Long, cols: Int, rows: Int, cellWidth: Int, cellHeight: Int): ByteArray?

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
     * format: [action] 0 press, 1 release, 2 motion; [button] 0 none, 1 left, 2 right, 3 middle,
     * 4 and 5 the wheel; [mods] as for [key]; [pressed] whether a button is held after this event.
     * Empty when the program does not take it (motion is sent once per cell, and only as its
     * tracking mode asks), null when it fails.
     */
    @JvmStatic external fun mouse(tab: Long, action: Int, button: Int, mods: Int, x: Float, y: Float, pressed: Boolean): ByteArray?

    /** Encodes focus gained or lost; empty when the program did not ask for focus events. */
    @JvmStatic external fun focus(tab: Long, gained: Boolean): ByteArray?

    /** Scrolls the viewport by [delta] rows (negative: back); 0 returns to the bottom. */
    @JvmStatic external fun scroll(tab: Long, delta: Int)

    /**
     * Scrolls the viewport to the prompt [delta] prompts above (negative) or below its top, or as
     * far as there are prompts, which the shell's prompt marks (OSC 133) tell; nothing without them.
     */
    @JvmStatic external fun jumpPrompt(tab: Long, delta: Int)

    /**
     * Selects the word at viewport cell ([x], [y]) and anchors later [selectTo] calls to it;
     * false when there is no word there (the selection is then cleared).
     */
    @JvmStatic external fun selectWord(tab: Long, x: Int, y: Int): Boolean

    /**
     * Selects the line at viewport cell ([x], [y]) as Ghostty's triple click does (soft wraps
     * joined, blanks at its ends left out, a prompt left out when OSC 133 marks it) and anchors
     * later [selectTo] calls to it; false when the line is blank (the selection is then cleared).
     */
    @JvmStatic external fun selectLine(tab: Long, x: Int, y: Int): Boolean

    /**
     * Starts dragging the handle of the selection's last cell ([end]) or first cell: the other end
     * becomes the anchor of [selectTo]. False when there is no selection.
     */
    @JvmStatic external fun selectGrab(tab: Long, end: Boolean): Boolean

    /**
     * Extends the selection from its anchor to viewport cell ([x], [y]); a rectangle stays one,
     * with the anchor and the cell at opposite corners.
     */
    @JvmStatic external fun selectTo(tab: Long, x: Int, y: Int): Boolean

    /**
     * Moves the selection's end as Ghostty's adjust_selection does ([adjustment] is one of the
     * `ADJUST_` constants: to the next or previous non-blank cell, a row or a page up or down, the
     * screen's start or end) and scrolls the viewport to show it; false without a selection.
     */
    @JvmStatic external fun selectAdjust(tab: Long, adjustment: Int): Boolean

    /**
     * Makes the selection a rectangle ([on]) with its first and last cells at opposite corners, or
     * linear again from its top left to its bottom right corner; false when there is none.
     */
    @JvmStatic external fun selectRectangle(tab: Long, on: Boolean): Boolean

    /** Selects everything, scrollback included. */
    @JvmStatic external fun selectAll(tab: Long): Boolean

    /**
     * Whether viewport cell ([x], [y]) is in a command's output, between the prompt marks (OSC 133)
     * of the command and of the next one, or above the first prompt; if so, the cell is kept, as
     * output moves it, for [selectOutput].
     */
    @JvmStatic external fun outputAt(tab: Long, x: Int, y: Int): Boolean

    /**
     * Selects the whole output the last [outputAt] found, as it is now, and anchors later
     * [selectTo] calls to it; false, leaving the selection alone, when it has left the scrollback.
     */
    @JvmStatic external fun selectOutput(tab: Long): Boolean

    @JvmStatic external fun selectClear(tab: Long)

    /** The selection as plain text (soft wraps joined, trailing blanks trimmed), null for none. */
    @JvmStatic external fun copy(tab: Long): String?

    /**
     * The link at viewport cell ([x], [y]): its OSC 8 URI (any scheme), else an http or https URL
     * written there, soft wraps joined; null when there is none.
     */
    @JvmStatic external fun link(tab: Long, x: Int, y: Int): String?

    /**
     * Searches the screen and its scrollback for [needle] (ASCII letters in either case, all else
     * exactly), restarting when it changed; empty finds nothing. The frames that follow find its
     * matches, mark them in their cells and carry their count; none is selected until
     * [searchSelect]. False when the search cannot be made.
     */
    @JvmStatic external fun search(tab: Long, needle: String): Boolean

    /**
     * Selects the next match towards older output ([older]; the first selects the newest) or newer
     * output, going round at either end, and scrolls the viewport to it unless it is shown; false
     * when there is no match.
     */
    @JvmStatic external fun searchSelect(tab: Long, older: Boolean): Boolean

    /** Ends the search; the next frame takes its marks away. */
    @JvmStatic external fun searchEnd(tab: Long)

    /**
     * Clears the screen and the scrollback as Ghostty's clear_screen does: at the shell's prompt
     * the whole screen (returns 1: send the shell a form feed to draw its prompt again), elsewhere
     * the rows above the cursor's, which moves to the top, and the screen's images (returns 0).
     * Returns -1 on the alternate screen, where nothing changes.
     */
    @JvmStatic external fun clear(tab: Long): Int

    /**
     * Resets the terminal (a full reset, RIS: screens, scrollback, modes, title, images); true when
     * the shell was at its prompt, which then gets a form feed to draw it again.
     */
    @JvmStatic external fun reset(tab: Long): Boolean

    /**
     * Shows a yellow line of app notice above the cursor's line (a kill report); false on the
     * alternate screen, where nothing is written.
     */
    @JvmStatic external fun message(tab: Long, text: String): Boolean

    /**
     * The primary screen's text with its scrollback (soft wraps joined, trailing blanks trimmed),
     * for saving tabs; null on the alternate screen, when nothing changed the screen since the
     * last transcript (the caller keeps that one: formatting restores the compressed scrollback),
     * or on failure.
     */
    @JvmStatic external fun transcript(tab: Long): String?

    /**
     * A snapshot of the whole terminal for saving the tab, which [restore] brings back; null when
     * nothing changed since the last one (the caller keeps that) or the tab is gone, empty when
     * none can be taken of the terminal as it is (output stopped inside a long sequence, or the
     * snapshot is larger than 16 MiB, mostly images). Any thread.
     */
    @JvmStatic external fun snapshot(tab: Long): ByteArray?

    /**
     * The version the snapshots are recorded with (the Ghostty source and libghostty-vt's own
     * version): one from another version is not restored.
     */
    @JvmStatic external fun version(): String?

    /**
     * Compresses every tab's cold scrollback at once (memory trimming); otherwise a tab's is
     * compressed once its terminal has been idle for a moment.
     */
    @JvmStatic external fun compressAll()

    /** Forgets a tab; a running [pump] keeps its terminal until it returns. */
    @JvmStatic external fun destroy(tab: Long)

    // libghostty-vt's GhosttySelectionAdjust ([selectAdjust])
    const val ADJUST_LEFT = 0
    const val ADJUST_RIGHT = 1
    const val ADJUST_UP = 2
    const val ADJUST_DOWN = 3
    const val ADJUST_HOME = 4
    const val ADJUST_END = 5
    const val ADJUST_PAGE_UP = 6
    const val ADJUST_PAGE_DOWN = 7
}
