package io.github.mthli.appine

import android.content.Context
import android.database.Cursor
import android.database.MatrixCursor
import android.os.CancellationSignal
import android.os.Handler
import android.os.HandlerThread
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.os.storage.StorageManager
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.system.ErrnoException
import android.system.Os
import android.util.Log
import android.webkit.MimeTypeMap
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import kotlin.concurrent.read

/**
 * appine in the system Files app (the android/documents map): the login account's home directory is
 * one root, and once turned on in Settings there is also the whole system (`/`), read-only.
 *
 * Document IDs are the guest's absolute paths (written as described in [Native]) and are looked up
 * directly in the store ([Native.browseList] and the like), without the kernel running: while it
 * runs, what is seen is what it last committed, and mounts such as `/proc`, `/dev` and `/run` are
 * not visible (they are empty directories in the store). Symbolic links are followed. Only
 * directories and regular files; device nodes, FIFOs and sockets are not listed.
 *
 * Inside the home directory files can be created, written, deleted, renamed and moved (the home
 * directory itself only allows creating inside it), and new ones belong to the login account;
 * everywhere else (the whole-system root) is read-only. Confined by real path: writing out of the
 * home directory through a symbolic link is not allowed. After a change, the Files app is told to
 * refresh that directory, and the running kernel is told as well, so programs watching in the guest
 * receive inotify events ([Native.browseWritten]). The other way round, when the guest changes
 * directory entries (create, delete, rename), the service takes them periodically
 * ([Native.takeChangedDirs]) and tells the Files app to refresh that directory ([dirsChanged]).
 *
 * With the whole system turned off, only documents inside the home directory are recognised: grants
 * on `/` given to other apps while it was on no longer work after it is turned off; likewise for
 * the old home directory after the login account changes.
 */
class Documents : DocumentsProvider() {
    override fun onCreate(): Boolean = true

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val result = MatrixCursor(projection ?: ROOT_COLUMNS)
        val context = context ?: return result
        if (!Rootfs.isReady(context)) return result
        val distro = Rootfs.installed(context)?.label.orEmpty()
        // Free space includes caches the system can clear, matching what Settings shows
        val free = runCatching {
            val storage = context.getSystemService(StorageManager::class.java)
            storage.getAllocatableBytes(storage.getUuidForPath(context.filesDir))
        }.getOrNull()
        // Without a home directory there is no home root, but the whole system is still shown when
        // it is turned on
        val home = home(context)
        if (home != null) result.newRow().apply {
            add(Root.COLUMN_ROOT_ID, ROOT_HOME)
            add(Root.COLUMN_DOCUMENT_ID, home.path)
            add(Root.COLUMN_TITLE, context.getString(R.string.app_name))
            add(Root.COLUMN_SUMMARY, context.getString(R.string.documents_home, home.account, distro))
            // Creating is supported: so it also appears in other apps' "Save to"
            val flags = Root.FLAG_LOCAL_ONLY or Root.FLAG_SUPPORTS_IS_CHILD or Root.FLAG_SUPPORTS_SEARCH or
                Root.FLAG_SUPPORTS_CREATE
            add(Root.COLUMN_FLAGS, flags)
            add(Root.COLUMN_ICON, R.mipmap.ic_launcher)
            add(Root.COLUMN_AVAILABLE_BYTES, free)
        }
        if (TerminalService.showSystemDocuments(context)) {
            result.newRow().apply {
                add(Root.COLUMN_ROOT_ID, ROOT_SYSTEM)
                add(Root.COLUMN_DOCUMENT_ID, "/")
                add(Root.COLUMN_TITLE, context.getString(R.string.app_name))
                add(Root.COLUMN_SUMMARY, context.getString(R.string.documents_system, distro))
                add(Root.COLUMN_FLAGS, Root.FLAG_LOCAL_ONLY or Root.FLAG_SUPPORTS_IS_CHILD or Root.FLAG_SUPPORTS_SEARCH)
                add(Root.COLUMN_ICON, R.mipmap.ic_launcher)
                add(Root.COLUMN_AVAILABLE_BYTES, free)
            }
        }
        result.setNotificationUri(context.contentResolver, DocumentsContract.buildRootsUri(authority(context)))
        return result
    }

    override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor {
        val context = attached()
        val path = visible(context, documentId)
        val meta = browse { Native.browseStat(it, path) }
        val texts = sniff(listOf(path to meta))
        return MatrixCursor(projection ?: DOCUMENT_COLUMNS).also { row(it, path, meta, home(context), texts) }
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val context = attached()
        val dir = visible(context, parentDocumentId)
        val entries = browse { Native.browseList(it, dir) }
            .map { child(dir, it.substringBefore('\t')) to it.substringAfter('\t') }
        val result = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        val home = home(context)
        val texts = sniff(entries)
        for ((path, meta) in entries) row(result, path, meta, home, texts)
        result.setNotificationUri(
            context.contentResolver,
            DocumentsContract.buildChildDocumentsUri(authority(context), parentDocumentId),
        )
        return result
    }

    override fun openDocument(documentId: String, mode: String, signal: CancellationSignal?): ParcelFileDescriptor {
        val context = attached()
        val flags = ParcelFileDescriptor.parseMode(mode)
        if (flags == ParcelFileDescriptor.MODE_READ_ONLY) {
            val path = visible(context, documentId)
            return ParcelFileDescriptor.adoptFd(browse { Native.browseOpen(it, path) })
        }
        val home = writableHome(context)
        val path = writable(home, documentId)
        val truncate = flags and ParcelFileDescriptor.MODE_TRUNCATE != 0
        val append = flags and ParcelFileDescriptor.MODE_APPEND != 0
        val opened = ParcelFileDescriptor.adoptFd(browse { Native.browseOpenWrite(it, home.path, path, truncate, append) })
        // The other side writes the host object directly, and the kernel sees neither what was
        // written nor when it is closed: reopen it by the object's path with a close callback, and
        // tell the running kernel on close (programs watching in the guest receive IN_MODIFY and
        // IN_CLOSE_WRITE). Truncation has already been done
        return try {
            val file = File(Os.readlink("/proc/self/fd/${opened.fd}"))
            val mode = flags and ParcelFileDescriptor.MODE_TRUNCATE.inv()
            ParcelFileDescriptor.open(file, mode, closer) {
                runCatching { browse { Native.browseWritten(it, home.path, path) } }
            }
        } catch (e: IOException) {
            Log.w(TAG, "cannot watch $documentId for closing", e)
            ParcelFileDescriptor.dup(opened.fileDescriptor)
        } catch (e: ErrnoException) {
            Log.w(TAG, "cannot watch $documentId for closing", e)
            ParcelFileDescriptor.dup(opened.fileDescriptor)
        } finally {
            opened.close()
        }
    }

    /** Close callbacks for files handed out for writing run on this thread ([openDocument]). */
    private val closer: Handler by lazy { Handler(HandlerThread("documents-close").apply { start() }.looper) }

    override fun createDocument(parentDocumentId: String, mimeType: String, displayName: String): String {
        val context = attached()
        val home = writableHome(context)
        val dir = writable(home, parentDocumentId, top = true)
        val directory = mimeType == Document.MIME_TYPE_DIR
        val name = fileName(displayName, mimeType, directory)
        val created = browse { Native.browseCreate(it, home.path, dir, name, directory, home.uid, home.gid) }
        childrenChanged(context, dir)
        return child(dir, created)
    }

    override fun deleteDocument(documentId: String) {
        val context = attached()
        val home = writableHome(context)
        val path = writable(home, documentId)
        browse { Native.browseRemove(it, home.path, path) }
        revokeDocumentPermission(documentId)
        childrenChanged(context, parent(path))
    }

    override fun renameDocument(documentId: String, displayName: String): String {
        val context = attached()
        val home = writableHome(context)
        val path = writable(home, documentId)
        val name = browse { Native.browseRename(it, home.path, path, fileName(displayName, "", true)) }
        val renamed = child(parent(path), name)
        if (renamed != path) revokeDocumentPermission(documentId)
        childrenChanged(context, parent(path))
        return renamed
    }

    override fun moveDocument(
        sourceDocumentId: String,
        sourceParentDocumentId: String,
        targetParentDocumentId: String,
    ): String {
        val context = attached()
        val home = writableHome(context)
        val path = writable(home, sourceDocumentId)
        val target = writable(home, targetParentDocumentId, top = true)
        browse { Native.browseMove(it, home.path, path, target) }
        revokeDocumentPermission(sourceDocumentId)
        childrenChanged(context, parent(path))
        childrenChanged(context, target)
        return child(target, path.substringAfterLast('/'))
    }

    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean {
        val parent = canonical(parentDocumentId) ?: return false
        val doc = canonical(documentId) ?: return false
        return doc != parent && doc.startsWith(if (parent == "/") "/" else "$parent/")
    }

    override fun querySearchDocuments(rootId: String, query: String, projection: Array<out String>?): Cursor {
        val context = attached()
        val dir = when (rootId) {
            ROOT_HOME -> home(context)?.path
            ROOT_SYSTEM -> "/".takeIf { TerminalService.showSystemDocuments(context) }
            else -> null
        } ?: throw FileNotFoundException("no root $rootId")
        val result = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        val home = home(context)
        val found = browse { Native.browseSearch(it, dir, query, SEARCH_LIMIT) }
            .map { it.substringBefore('\t') to it.substringAfter('\t') }
        val texts = sniff(found)
        for ((path, meta) in found) row(result, path, meta, home, texts)
        return result
    }

    /**
     * The login account, its UID and GID, and the home directory (as a document ID). null while
     * there is no store or no home directory; a home of `/` counts as none, since it would make the
     * whole system writable (the native side refuses it too).
     */
    private data class Home(val account: String, val uid: Int, val gid: Int, val path: String)

    private fun home(context: Context): Home? {
        if (!Rootfs.isReady(context)) return null
        val login = TerminalService.login(context)
        cache?.let { (time, key, home) ->
            if (key == login && SystemClock.elapsedRealtime() - time < CACHE_MS) return home
        }
        val home = Rootfs.access.read {
            val account = Rootfs.loginAccount(context, login)
            val path = canonical("/" + account.home.trim('/'))?.takeIf { it != "/" } ?: return@read null
            // No root is listed when the home directory does not exist (the account has none)
            runCatching { Native.browseStat(Rootfs.store(context).path, path) }.getOrNull() ?: return@read null
            Home(account.name, account.uid, account.gid, path)
        }
        cache = Triple(SystemClock.elapsedRealtime(), login, home)
        return home
    }

    /**
     * The home directory computed last: one browse queries several times, so /etc/passwd need not
     * be read every time.
     */
    @Volatile
    private var cache: Triple<Long, String, Home?>? = null

    /**
     * Returns the document ID when it is canonical and inside a root shown now; otherwise treated
     * as nonexistent.
     */
    private fun visible(context: Context, documentId: String): String {
        val path = canonical(documentId) ?: throw FileNotFoundException("bad document $documentId")
        if (TerminalService.showSystemDocuments(context)) return path
        val home = home(context)?.path ?: throw FileNotFoundException("no home directory")
        if (path == home || isChildDocument(home, path)) return path
        throw FileNotFoundException("$documentId is outside $home")
    }

    private fun writableHome(context: Context): Home =
        home(context) ?: throw FileNotFoundException("no home directory")

    /**
     * A document to modify: canonical and inside the home directory (with [top], also the home
     * directory itself); otherwise read-only.
     */
    private fun writable(home: Home, documentId: String, top: Boolean = false): String {
        val path = canonical(documentId) ?: throw FileNotFoundException("bad document $documentId")
        if ((top && path == home.path) || isChildDocument(home.path, path)) return path
        throw UnsupportedOperationException("$documentId is read-only")
    }

    /** Something in directory [dir] changed: the Files app lists it again. */
    private fun childrenChanged(context: Context, dir: String) {
        context.contentResolver.notifyChange(DocumentsContract.buildChildDocumentsUri(authority(context), dir), null)
    }

    /** ContentProvider's requireContext needs API 30. */
    private fun attached(): Context = context ?: throw IllegalStateException("the provider is not attached")

    private fun <T> browse(call: (String) -> T): T {
        val store = Rootfs.store(attached()).path
        return try {
            Rootfs.access.read { call(store) }
        } catch (e: FileNotFoundException) {
            throw e
        } catch (e: IOException) {
            throw FileNotFoundException(e.message)
        }
    }

    /**
     * The regular files in [docs] (paths and metadata) whose type the extension cannot tell and
     * whose content looks like text ([Native.browseSniff], all checked with one opening of the
     * store). That makes `/etc/profile`, `.bashrc`, scripts and `Makefile` text/plain, so tapping
     * them opens an editor that can handle them.
     */
    private fun sniff(docs: List<Pair<String, String>>): Set<String> {
        val unknown = docs.filter { (path, meta) -> meta.startsWith("f\t") && guessType(displayName(path)) == null }
            .map { it.first }
        if (unknown.isEmpty()) return emptySet()
        val texts = runCatching { browse { Native.browseSniff(it, unknown.toTypedArray()) } }.getOrNull()
            ?: return emptySet()
        return unknown.filterIndexed { i, _ -> texts.getOrElse(i) { false } }.toSet()
    }

    private fun row(cursor: MatrixCursor, path: String, meta: String, home: Home?, texts: Set<String>) {
        val f = meta.split('\t')
        if (f.size < 4) return
        val dir = f[0] == "d"
        val name = displayName(path)
        val mime = when {
            dir -> Document.MIME_TYPE_DIR
            path in texts -> "text/plain"
            else -> mimeType(name)
        }
        cursor.newRow().apply {
            add(Document.COLUMN_DOCUMENT_ID, path)
            add(Document.COLUMN_DISPLAY_NAME, name)
            add(Document.COLUMN_MIME_TYPE, mime)
            add(Document.COLUMN_SIZE, if (dir) null else f[1].toLongOrNull())
            add(Document.COLUMN_LAST_MODIFIED, f[2].toLongOrNull())
            add(Document.COLUMN_FLAGS, flags(path, dir, home?.path))
        }
    }

    /**
     * What a document allows: inside the home directory, deleting, renaming and moving, writing for
     * files, creating inside for directories; the home directory itself only allows creating inside
     * it; everywhere else is read-only. Judged by the literal path; leading out through a symbolic
     * link is refused by the native library.
     */
    private fun flags(path: String, dir: Boolean, home: String?): Int {
        if (home == null) return 0
        if (path == home) return if (dir) Document.FLAG_DIR_SUPPORTS_CREATE else 0
        if (!isChildDocument(home, path)) return 0
        val common = Document.FLAG_SUPPORTS_DELETE or Document.FLAG_SUPPORTS_RENAME or Document.FLAG_SUPPORTS_MOVE
        return common or if (dir) Document.FLAG_DIR_SUPPORTS_CREATE else Document.FLAG_SUPPORTS_WRITE
    }

    companion object {
        const val ROOT_HOME = "home"
        const val ROOT_SYSTEM = "system"

        private const val TAG = "appine"

        /** The most results one search gives. */
        private const val SEARCH_LIMIT = 200

        private const val CACHE_MS = 5_000L

        private val ROOT_COLUMNS = arrayOf(
            Root.COLUMN_ROOT_ID, Root.COLUMN_DOCUMENT_ID, Root.COLUMN_TITLE, Root.COLUMN_SUMMARY,
            Root.COLUMN_FLAGS, Root.COLUMN_ICON, Root.COLUMN_AVAILABLE_BYTES,
        )
        private val DOCUMENT_COLUMNS = arrayOf(
            Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE,
            Document.COLUMN_SIZE, Document.COLUMN_LAST_MODIFIED, Document.COLUMN_FLAGS,
        )

        fun authority(context: Context) = "${context.packageName}.documents"

        /**
         * The login account, the store or "show the whole system" changed: the Files app reads the
         * roots again.
         */
        fun rootsChanged(context: Context) {
            context.contentResolver.notifyChange(DocumentsContract.buildRootsUri(authority(context)), null)
        }

        /**
         * The guest changed something in these directories ([Native.takeChangedDirs]): the Files
         * app lists them again if it is looking at them.
         */
        fun dirsChanged(context: Context, dirs: Array<String>) {
            for (dir in dirs) {
                context.contentResolver.notifyChange(DocumentsContract.buildChildDocumentsUri(authority(context), dir), null)
            }
        }

        /**
         * A canonical path: starts with `/`, has no empty, `.` or `..` components, and does not end
         * with `/` (except `/` itself). null when it is not.
         */
        fun canonical(path: String): String? {
            if (path == "/") return path
            if (!path.startsWith("/") || path.endsWith("/")) return null
            val parts = path.substring(1).split('/')
            if (parts.any { it.isEmpty() || it == "." || it == ".." }) return null
            return path
        }

        fun child(dir: String, name: String) = if (dir == "/") "/$name" else "$dir/$name"

        fun parent(path: String) = path.substringBeforeLast('/').ifEmpty { "/" }

        /**
         * The name used when creating and renaming: `/` and NUL become `_`, and empty, `.` and `..`
         * become `_`. When creating a file whose name's extension does not match [mime], one is
         * added as Android's own storage does ("text", text/plain → "text.txt"); not for
         * directories or application/octet-stream.
         */
        fun fileName(displayName: String, mime: String, directory: Boolean): String {
            val name = displayName.replace('/', '_').replace('\u0000', '_')
                .let { if (it.isEmpty() || it == "." || it == "..") "_" else it }
            if (directory || mime.isEmpty() || mime == "application/octet-stream") return name
            if (mimeType(name) == mime) return name
            val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(mime) ?: return name
            return "$name.$ext"
        }

        /**
         * The human-readable name of the last component of a path: `%XX` turned back into bytes and
         * decoded as UTF-8 (bytes that are not UTF-8 show as �).
         */
        fun displayName(path: String): String {
            val last = path.substringAfterLast('/').ifEmpty { return "/" }
            val out = ByteArrayOutputStream()
            var i = 0
            while (i < last.length) {
                val c = last[i]
                if (c == '%' && i + 3 <= last.length) {
                    val hex = last.substring(i + 1, i + 3)
                    if (hex.all { it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F' }) {
                        out.write(hex.toInt(16))
                        i += 3
                        continue
                    }
                }
                val end = if (Character.isHighSurrogate(c) && i + 1 < last.length) i + 2 else i + 1
                out.write(last.substring(i, end).toByteArray())
                i = end
            }
            return String(out.toByteArray(), Charsets.UTF_8)
        }

        /** Guesses the MIME type by extension; application/octet-stream when it cannot tell. */
        fun mimeType(name: String): String = guessType(name) ?: "application/octet-stream"

        /**
         * Guesses the MIME type by extension; null when it cannot tell (no extension, an unknown
         * extension). Dot files like `.bashrc` have what follows the dot taken as the extension,
         * which is usually unknown.
         */
        fun guessType(name: String): String? {
            val ext = name.substringAfterLast('.', "").lowercase()
            if (ext.isEmpty()) return null
            return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext)
        }
    }
}
