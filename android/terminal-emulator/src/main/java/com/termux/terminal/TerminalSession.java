package com.termux.terminal;

import android.annotation.SuppressLint;
import android.os.Handler;
import android.os.Looper;
import android.os.Message;

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.UUID;

/**
 * A terminal session, consisting of a process coupled to a terminal interface.
 * <p>
 * The process is started when the size is made known by a call to
 * {@link #updateSize(int, int, int, int)}, and threads are spawned to handle its I/O.
 * All terminal emulation and callback methods will be performed on the main thread.
 * <p>
 * appine's changes: the original implementation started a subprocess on a host PTY through
 * {@code JNI.createSubprocess}; here it is replaced by the {@link Pty} interface, connected to the
 * master side of a PTY inside the appine process. The thread structure (one thread each for
 * reading, writing and waiting, with emulation on the main thread) and the rest of the public
 * interface are unchanged.
 */
public final class TerminalSession extends TerminalOutput {

    /**
     * The master side of the in-process PTY, implemented by the app. Methods other than
     * {@link #start} are called on background threads.
     */
    public interface Pty {
        /** Starts the shell with the given window size and returns its pid. */
        int start(int columns, int rows) throws IOException;

        /** Reads the shell's output; returns -1 when the session has ended. */
        int read(byte[] buffer) throws IOException;

        /** Input to the shell. */
        void write(byte[] buffer, int offset, int count) throws IOException;

        /** The window size changed. */
        void resize(int columns, int rows);

        /**
         * Waits for the shell to end: returns the exit code, or the negated signal number when a
         * signal terminated it.
         */
        int waitFor();

        /** Closes the PTY: as when a terminal window is closed, the shell receives SIGHUP. */
        void close();
    }

    private static final int MSG_NEW_INPUT = 1;
    private static final int MSG_PROCESS_EXITED = 4;

    public final String mHandle = UUID.randomUUID().toString();

    TerminalEmulator mEmulator;

    /**
     * A queue written to from a separate thread when the process outputs, and read by main thread to process by
     * terminal emulator.
     */
    final ByteQueue mProcessToTerminalIOQueue = new ByteQueue(64 * 1024);
    /**
     * A queue written to from the main thread due to user interaction, and read by another thread which forwards by
     * writing to the {@link Pty}.
     */
    final ByteQueue mTerminalToProcessIOQueue = new ByteQueue(4096);
    /** Buffer to write translate code points into utf8 before writing to mTerminalToProcessIOQueue */
    private final byte[] mUtf8InputBuffer = new byte[5];

    /** Callback which gets notified when a session finishes or changes title. */
    TerminalSessionClient mClient;

    /** The pid of the shell process. 0 if not started and -1 if finished running. */
    int mShellPid;

    /** The exit status of the shell process. Only valid if ${@link #mShellPid} is -1. */
    int mShellExitStatus;

    /** Set by the application for user identification of session, not by terminal. */
    public String mSessionName;

    final Handler mMainThreadHandler = new MainThreadHandler();

    private final Pty mPty;
    private final Integer mTranscriptRows;

    /**
     * For a tab restored after the app was killed: what to show after the emulator is created and
     * before the shell starts (the previous transcript and a notice), discarded once shown.
     */
    private byte[] mRestored;

    private static final String LOG_TAG = "TerminalSession";

    public TerminalSession(Pty pty, Integer transcriptRows, TerminalSessionClient client) {
        this.mPty = pty;
        this.mTranscriptRows = transcriptRows;
        this.mClient = client;
    }

    /**
     * @param client The {@link TerminalSessionClient} interface implementation to allow
     *               for communication between {@link TerminalSession} and its client.
     */
    public void updateTerminalSessionClient(TerminalSessionClient client) {
        mClient = client;

        if (mEmulator != null)
            mEmulator.updateTerminalSessionClient(client);
    }

    /**
     * A restored tab: shows [text] before the shell's output (a terminal byte stream, so line
     * breaks must be \r\n).
     */
    public void setRestored(byte[] text) {
        mRestored = text;
    }

    /**
     * Shows a line of app notice in yellow in the terminal (such as why a process was ended);
     * called on the main thread. By the time the notice arrives the shell has most likely printed a
     * new prompt, so it is inserted above the cursor's line: move down a line and back (at the last
     * line this scrolls the screen up to make room), insert as many lines as the notice takes at
     * the cursor's line, write the notice, and put the cursor back. The prompt and any half-typed
     * command move down with it, and the shell's line editing is unaffected. While the emulator is
     * not created yet (the tab has never been shown), notices are kept and shown first once it is
     * created. In a full-screen program (the alternate screen: vim, less and the like) nothing is
     * shown and false is returned: writing would mess up its display.
     */
    public boolean showMessage(String text) {
        if (mEmulator == null) {
            byte[] message = ("\033[33m" + text + "\033[0m\r\n").getBytes(StandardCharsets.UTF_8);
            byte[] pending = mRestored == null ? new byte[0] : mRestored;
            byte[] joined = new byte[pending.length + message.length];
            System.arraycopy(pending, 0, joined, 0, pending.length);
            System.arraycopy(message, 0, joined, pending.length, message.length);
            mRestored = joined;
            return true;
        }
        if (mEmulator.isAlternateBufferActive()) return false;
        int width = 0;
        for (int i = 0; i < text.length(); ) {
            int c = text.codePointAt(i);
            width += Math.max(0, WcWidth.width(c));
            i += Character.charCount(c);
        }
        int rows = Math.min(Math.max(1, (width + mEmulator.mColumns - 1) / mEmulator.mColumns), mEmulator.mRows - 1);
        StringBuilder s = new StringBuilder();
        for (int i = 0; i < rows; i++) s.append('\n');
        s.append("\033[").append(rows).append('A');
        s.append("\0337\r\033[").append(rows).append('L');
        s.append("\033[33m").append(text).append("\033[0m");
        s.append("\0338\033[").append(rows).append('B');
        byte[] b = s.toString().getBytes(StandardCharsets.UTF_8);
        mEmulator.append(b, b.length);
        mClient.onTextChanged(this);
        return true;
    }

    /**
     * The transcript text (for saving tabs); null while the emulator is not created yet (the tab
     * has never been shown).
     */
    public String getTranscriptText() {
        return mEmulator == null ? null : mEmulator.getScreen().getTranscriptText();
    }

    /** Inform the attached pty of the new size and reflow or initialize the emulator. */
    public void updateSize(int columns, int rows, int cellWidthPixels, int cellHeightPixels) {
        if (mEmulator == null) {
            initializeEmulator(columns, rows, cellWidthPixels, cellHeightPixels);
        } else {
            if (isRunning()) mPty.resize(columns, rows);
            mEmulator.resize(columns, rows, cellWidthPixels, cellHeightPixels);
        }
    }

    /** The terminal title as set through escape sequences or null if none set. */
    public String getTitle() {
        return (mEmulator == null) ? null : mEmulator.getTitle();
    }

    /**
     * Set the terminal emulator's window size and start terminal emulation.
     *
     * @param columns The number of columns in the terminal window.
     * @param rows    The number of rows in the terminal window.
     */
    public void initializeEmulator(int columns, int rows, int cellWidthPixels, int cellHeightPixels) {
        mEmulator = new TerminalEmulator(this, columns, rows, cellWidthPixels, cellHeightPixels, mTranscriptRows, mClient);
        if (mRestored != null) {
            mEmulator.append(mRestored, mRestored.length);
            mRestored = null;
        }

        try {
            mShellPid = mPty.start(columns, rows);
        } catch (IOException e) {
            // Start failed: show the reason in the terminal and treat the session as ended
            Logger.logStackTraceWithMessage(mClient, LOG_TAG, "Cannot start the shell", e);
            byte[] message = ("\r\n[" + e.getMessage() + "]\r\n").getBytes(StandardCharsets.UTF_8);
            mEmulator.append(message, message.length);
            mShellPid = -1;
            mShellExitStatus = 127;
            mClient.onSessionFinished(this);
            return;
        }
        mClient.setTerminalShellPid(this, mShellPid);

        final Thread reader = new Thread("TermSessionInputReader[pid=" + mShellPid + "]") {
            @Override
            public void run() {
                try {
                    final byte[] buffer = new byte[4096];
                    while (true) {
                        int read = mPty.read(buffer);
                        if (read == -1) return;
                        if (!mProcessToTerminalIOQueue.write(buffer, 0, read)) return;
                        mMainThreadHandler.sendEmptyMessage(MSG_NEW_INPUT);
                    }
                } catch (IOException e) {
                    // Ignore, just shutting down.
                }
            }
        };
        reader.start();

        new Thread("TermSessionOutputWriter[pid=" + mShellPid + "]") {
            @Override
            public void run() {
                final byte[] buffer = new byte[4096];
                try {
                    while (true) {
                        int bytesToWrite = mTerminalToProcessIOQueue.read(buffer, true);
                        if (bytesToWrite == -1) return;
                        mPty.write(buffer, 0, bytesToWrite);
                    }
                } catch (IOException e) {
                    // Ignore.
                }
            }
        }.start();

        new Thread("TermSessionWaiter[pid=" + mShellPid + "]") {
            @Override
            public void run() {
                int processExitCode = mPty.waitFor();
                // Output from before the shell exited may not have been read yet: wait a little for
                // the reader thread (a background process may keep the terminal open forever, so it
                // cannot wait without limit)
                try {
                    reader.join(500);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
                mMainThreadHandler.sendMessage(mMainThreadHandler.obtainMessage(MSG_PROCESS_EXITED, processExitCode));
            }
        }.start();

    }

    /** Write data to the shell process. */
    @Override
    public void write(byte[] data, int offset, int count) {
        if (mShellPid > 0) mTerminalToProcessIOQueue.write(data, offset, count);
    }

    /** Write the Unicode code point to the terminal encoded in UTF-8. */
    public void writeCodePoint(boolean prependEscape, int codePoint) {
        if (codePoint > 1114111 || (codePoint >= 0xD800 && codePoint <= 0xDFFF)) {
            // 1114111 (= 2**16 + 1024**2 - 1) is the highest code point, [0xD800,0xDFFF] is the surrogate range.
            throw new IllegalArgumentException("Invalid code point: " + codePoint);
        }

        int bufferPosition = 0;
        if (prependEscape) mUtf8InputBuffer[bufferPosition++] = 27;

        if (codePoint <= /* 7 bits */0b1111111) {
            mUtf8InputBuffer[bufferPosition++] = (byte) codePoint;
        } else if (codePoint <= /* 11 bits */0b11111111111) {
            /* 110xxxxx leading byte with leading 5 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b11000000 | (codePoint >> 6));
            /* 10xxxxxx continuation byte with following 6 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b10000000 | (codePoint & 0b111111));
        } else if (codePoint <= /* 16 bits */0b1111111111111111) {
            /* 1110xxxx leading byte with leading 4 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b11100000 | (codePoint >> 12));
            /* 10xxxxxx continuation byte with following 6 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b10000000 | ((codePoint >> 6) & 0b111111));
            /* 10xxxxxx continuation byte with following 6 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b10000000 | (codePoint & 0b111111));
        } else { /* We have checked codePoint <= 1114111 above, so we have max 21 bits = 0b111111111111111111111 */
            /* 11110xxx leading byte with leading 3 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b11110000 | (codePoint >> 18));
            /* 10xxxxxx continuation byte with following 6 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b10000000 | ((codePoint >> 12) & 0b111111));
            /* 10xxxxxx continuation byte with following 6 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b10000000 | ((codePoint >> 6) & 0b111111));
            /* 10xxxxxx continuation byte with following 6 bits */
            mUtf8InputBuffer[bufferPosition++] = (byte) (0b10000000 | (codePoint & 0b111111));
        }
        write(mUtf8InputBuffer, 0, bufferPosition);
    }

    public TerminalEmulator getEmulator() {
        return mEmulator;
    }

    /** Notify the {@link #mClient} that the screen has changed. */
    protected void notifyScreenUpdate() {
        mClient.onTextChanged(this);
    }

    /** Reset state for terminal emulator state. */
    public void reset() {
        mEmulator.reset();
        notifyScreenUpdate();
    }

    /** Finish this terminal session by hanging up its terminal: the shell receives SIGHUP. */
    public void finishIfRunning() {
        if (isRunning()) mPty.close();
    }

    /** Cleanup resources when the process exits. */
    void cleanupResources(int exitStatus) {
        synchronized (this) {
            mShellPid = -1;
            mShellExitStatus = exitStatus;
        }

        // Stop the reader and writer threads, and close the terminal
        mTerminalToProcessIOQueue.close();
        mProcessToTerminalIOQueue.close();
        mPty.close();
    }

    @Override
    public void titleChanged(String oldTitle, String newTitle) {
        mClient.onTitleChanged(this);
    }

    public synchronized boolean isRunning() {
        return mShellPid != -1;
    }

    /** Only valid if not {@link #isRunning()}. */
    public synchronized int getExitStatus() {
        return mShellExitStatus;
    }

    @Override
    public void onCopyTextToClipboard(String text) {
        mClient.onCopyTextToClipboard(this, text);
    }

    @Override
    public void onPasteTextFromClipboard() {
        mClient.onPasteTextFromClipboard(this);
    }

    @Override
    public void onBell() {
        mClient.onBell(this);
    }

    @Override
    public void onColorsChanged() {
        mClient.onColorsChanged(this);
    }

    public int getPid() {
        return mShellPid;
    }

    @SuppressLint("HandlerLeak")
    class MainThreadHandler extends Handler {

        final byte[] mReceiveBuffer = new byte[64 * 1024];

        MainThreadHandler() {
            super(Looper.getMainLooper());
        }

        @Override
        public void handleMessage(Message msg) {
            int bytesRead = mProcessToTerminalIOQueue.read(mReceiveBuffer, false);
            if (bytesRead > 0) {
                mEmulator.append(mReceiveBuffer, bytesRead);
                notifyScreenUpdate();
            }

            if (msg.what == MSG_PROCESS_EXITED) {
                int exitCode = (Integer) msg.obj;
                cleanupResources(exitCode);

                String exitDescription = "\r\n[Process completed";
                if (exitCode > 0) {
                    // Non-zero process exit.
                    exitDescription += " (code " + exitCode + ")";
                } else if (exitCode < 0) {
                    // Negated signal.
                    exitDescription += " (signal " + (-exitCode) + ")";
                }
                exitDescription += " - press Enter]";

                byte[] bytesToWrite = exitDescription.getBytes(StandardCharsets.UTF_8);
                mEmulator.append(bytesToWrite, bytesToWrite.length);
                notifyScreenUpdate();

                mClient.onSessionFinished(TerminalSession.this);
            }
        }

    }

}
