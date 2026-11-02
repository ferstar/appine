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
 * appine 的修改：原来的实现经由 {@code JNI.createSubprocess} 在宿主 PTY 上启动子进程，
 * 这里换成 {@link Pty} 接口，接到 appine 进程内 PTY 的主设备上。线程结构（读、写、等待
 * 各一个线程，模拟在主线程上进行）和其余对外接口保持不变。
 */
public final class TerminalSession extends TerminalOutput {

    /** 进程内 PTY 的主设备一侧，由 App 实现。除了 {@link #start} 之外的方法在后台线程上调用。 */
    public interface Pty {
        /** 以给定的窗口大小启动 shell，返回它的 pid。 */
        int start(int columns, int rows) throws IOException;

        /** 读 shell 的输出；返回 -1 表示会话结束。 */
        int read(byte[] buffer) throws IOException;

        /** 向 shell 输入。 */
        void write(byte[] buffer, int offset, int count) throws IOException;

        /** 窗口大小变了。 */
        void resize(int columns, int rows);

        /** 等 shell 结束：返回退出码，被信号终止时返回负的信号编号。 */
        int waitFor();

        /** 关闭 PTY：与关闭终端窗口一样，shell 收到 SIGHUP。 */
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
     * App 被杀之后恢复的标签页：模拟器建好、shell 启动之前先显示的内容（上次的回看记录和提示），
     * 显示过就丢掉。
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

    /** 恢复的标签页：在 shell 的输出之前显示 [text]（终端的字节流，换行要是 \r\n）。 */
    public void setRestored(byte[] text) {
        mRestored = text;
    }

    /**
     * 在终端里用黄色显示一行 App 的提示（进程被结束的原因这些），在主线程上调用。提示到的时候
     * shell 多半已经印出了新的提示符，所以插在光标所在的行上面：先换行再回来（在最后一行时让
     * 屏幕上卷，腾出地方），在光标的行插入提示占的行数，写提示，光标回到原处。提示符和输入了
     * 一半的命令跟着下移，shell 的行编辑不受影响。模拟器还没建（标签页还没显示过）时攒着，
     * 建好之后先显示。正在全屏程序（备用屏幕，vim、less 这些）里时不显示、返回 false：写进去会
     * 弄乱它的画面。
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

    /** 回看记录的文本（保存标签页用）；模拟器还没建（标签页还没显示过）时是 null。 */
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
            // 启动失败：把原因显示在终端里，会话按已结束处理
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
                // shell 退出前的输出可能还没读完：等一会儿读线程（后台进程可能一直开着终端，
                // 所以不能无限等）
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
