package io.github.mthli.appine

import android.app.Application
import android.content.ContentResolver
import android.content.Context
import android.net.Uri
import android.os.CancellationSignal
import android.os.Handler
import android.os.Looper
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.LiveData
import androidx.lifecycle.MutableLiveData
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.IOException
import java.io.InputStream
import java.util.concurrent.CancellationException
import java.util.concurrent.ExecutionException
import java.util.concurrent.FutureTask
import kotlin.concurrent.read
import kotlin.concurrent.thread

/** Owns a picker request and its copy across activity recreation, but never process death. */
internal class TerminalFiles(application: Application) : AndroidViewModel(application) {
    data class Result(val paths: List<String>, val failures: List<String>)

    private val notifications = MutableLiveData(Unit)
    val changes: LiveData<Unit> get() = notifications

    val copying: Boolean get() = batch != null
    var result: Result? = null
        private set
    private var selecting = false
    private var login = ""
    private var selectedUris = emptyList<Uri>()
    private var batch: ImportBatch? = null
    private val main = Handler(Looper.getMainLooper())

    val occupied: Boolean get() = selecting || copying || selectedUris.isNotEmpty() || result != null

    fun begin(login: String): Boolean {
        if (occupied) return false
        this.login = login
        selecting = true
        changed()
        return true
    }

    fun selected(uris: List<Uri>) {
        if (!selecting) return
        selecting = false
        selectedUris = uris.distinct()
        changed()
    }

    /** Starts a pending copy once; the caller must be resumed with its privacy gate open. */
    fun importSelected(): Boolean {
        if (selectedUris.isEmpty() || copying) return false
        val uris = selectedUris
        val requestedLogin = login
        selectedUris = emptyList()
        val active = ImportBatch()
        batch = active
        changed()
        thread(name = "terminal-import") {
            val imported = copySelected(uris, requestedLogin, active)
            main.post {
                batch = null
                result = imported
                changed()
            }
        }
        return true
    }

    fun cancelImport() { batch?.cancel() }

    override fun onCleared() { cancelImport() }

    private fun copySelected(uris: List<Uri>, login: String, active: ImportBatch): Result {
        val context = getApplication<Application>()
        val resolver = context.contentResolver
        val paths = mutableListOf<String>()
        val failures = mutableListOf<String>()
        try {
            val (generation, account) = Rootfs.access.read {
                if (!Rootfs.isReady(context)) throw IOException("The filesystem is not ready")
                Rootfs.generation to Rootfs.loginAccount(context, login)
            }
            for (uri in uris) {
                active.check()
                var name = context.getString(R.string.import_file_name)
                try {
                    active.stage(context.cacheDir, { output, signal ->
                        resolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null, signal)?.use { cursor ->
                            if (cursor.moveToFirst()) name = cursor.getString(0)?.takeIf(String::isNotEmpty) ?: name
                        }
                        resolver.copyDocument(uri, output, signal)
                    }) { input ->
                        Rootfs.access.read {
                            active.check()
                            if (Rootfs.generation != generation) throw CancellationException()
                            paths += copyIntoHome(Rootfs.store(context).path, account, name, context, active::check) { input }
                        }
                    }
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    active.check()
                    failures += context.getString(R.string.import_file_failed, name, e.message ?: e.javaClass.simpleName)
                }
            }
        } catch (_: CancellationException) {
            failures += context.getString(R.string.import_files_cancelled)
        } catch (e: Exception) {
            failures += e.message ?: e.javaClass.simpleName
        }
        return Result(paths, failures)
    }

    private fun ContentResolver.copyDocument(uri: Uri, output: FileOutputStream, signal: CancellationSignal) {
        val source = openAssetFileDescriptor(uri, "r", signal)
            ?: throw IOException("Cannot open the selected file")
        source.use { asset ->
            asset.createInputStream().use { input ->
                signal.setOnCancelListener { runCatching { input.close() } }
                try {
                    signal.throwIfCanceled()
                    input.copyTo(output)
                } finally {
                    signal.setOnCancelListener(null)
                }
            }
        }
    }

    fun dismissResult() {
        result = null
        changed()
    }

    private fun changed() { notifications.value = Unit }

    companion object {
        /** Publishes the completed file only after both streams close successfully. */
        internal fun copyIntoHome(
            store: String,
            account: Rootfs.Account,
            displayName: String,
            context: Context? = null,
            check: () -> Unit = {},
            openInput: () -> InputStream,
        ): String {
            val top = Documents.documentId(account.home)?.takeIf { it != "/" }
                ?: throw IOException("The login account has no home directory")
            val safeName = displayName.map { if (Character.isISOControl(it)) '_' else it }.joinToString("")
            val name = Documents.fileName(safeName, "", false)
            val created = Native.browseImport(store, top, name, account.uid, account.gid) { fd ->
                openInput().use { input ->
                    ParcelFileDescriptor.AutoCloseOutputStream(ParcelFileDescriptor.fromFd(fd)).use { output ->
                        val buffer = ByteArray(DEFAULT_BUFFER_SIZE)
                        while (true) {
                            check()
                            val count = input.read(buffer)
                            if (count < 0) break
                            output.write(buffer, 0, count)
                        }
                    }
                }
                check()
            }
            context?.let { Documents.dirsChanged(it, arrayOf(top), store) }
            return Documents.child(account.home.trimEnd('/'), Documents.displayName(created))
        }

        /** Escapes imported absolute paths as shell arguments without appending a newline. */
        fun clipboardPaths(paths: List<String>): String = paths.joinToString(" ") { path ->
            buildString {
                for (char in path) {
                    if (char.isWhitespace() || char in "\\'\"`$&|;<>*?()[]{}!#~^") append('\\')
                    append(char)
                }
            }
        }
    }
}

/** A provider may ignore cancellation. Only its worker touches the provider, outside the store. */
internal class ImportBatch {
    private val signal = CancellationSignal()
    @Volatile private var cancelled = false
    private var provider: FutureTask<Unit>? = null

    fun check() { if (cancelled) throw CancellationException() }

    @Synchronized fun cancel() {
        if (cancelled) return
        cancelled = true
        provider?.cancel(true)
        // Cancellation may make a synchronous binder call to a slow provider.
        thread(name = "terminal-import-cancel") { signal.cancel() }
    }

    fun stage(
        directory: File,
        copy: (FileOutputStream, CancellationSignal) -> Unit,
        publish: (FileInputStream) -> Unit,
    ) {
        check()
        val temp = File.createTempFile("terminal-import-", null, directory)
        var linked = true
        try {
            FileInputStream(temp).use { input ->
                FileOutputStream(temp).use { output ->
                    // Open both streams before unlinking so process death leaves no cache file.
                    if (!temp.delete()) throw IOException("Cannot unlink the staging file")
                    linked = false
                    runProvider { copy(output, signal) }
                }
                check()
                publish(input)
            }
        } finally {
            if (linked) temp.delete()
        }
    }

    private fun runProvider(read: () -> Unit) {
        val task = FutureTask { read() }
        synchronized(this) {
            check()
            provider = task
            thread(name = "terminal-import-provider") { task.run() }
        }
        try {
            task.get()
        } catch (e: ExecutionException) {
            throw e.cause ?: e
        } finally {
            synchronized(this) { provider = null }
        }
    }
}
