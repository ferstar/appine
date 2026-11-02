package io.github.mthli.appine

import android.content.Context
import android.database.Cursor
import android.database.MatrixCursor
import android.os.CancellationSignal
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import android.os.storage.StorageManager
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import android.provider.DocumentsContract.Root
import android.provider.DocumentsProvider
import android.webkit.MimeTypeMap
import java.io.ByteArrayOutputStream
import java.io.FileNotFoundException
import java.io.IOException
import kotlin.concurrent.read

/**
 * 系统"文件"App 里的 appine（设计文档 §11.3）：登录账号的家目录是一个根，设置里打开之后还有
 * 只读的整个系统（`/`）。
 *
 * 文档 ID 就是 guest 的绝对路径（写法见 [Native] 的说明），直接在仓库里查（[Native.browseList]
 * 这些），不需要内核在运行：运行着时看到的是它最新提交的，`/proc`、`/dev`、`/run` 这些挂载
 * 看不到（是仓库里的空目录）。符号链接跟随。只有目录和普通文件，设备节点、FIFO、socket 不列。
 *
 * 家目录里面可以新建、写、删除、改名、移动（家目录本身只能在里面新建），新建的属于登录账号；
 * 别处（整个系统那个根）只读。按真实路径限制：经由符号链接写到家目录外面去的不行。改了之后
 * 通知"文件"App 刷新那个目录；guest 里的改动它不知道（内核没有告诉 provider）。
 *
 * 没打开整个系统时，只认家目录里面的文档：以前打开时授予别的 App 的 `/` 的权限，关掉之后也
 * 用不了了；换了登录账号，原来那个家目录也一样。
 */
class Documents : DocumentsProvider() {
    override fun onCreate(): Boolean = true

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val result = MatrixCursor(projection ?: ROOT_COLUMNS)
        val context = context ?: return result
        val home = home(context) ?: return result
        val distro = Rootfs.installed(context)?.label.orEmpty()
        // 可用空间算上系统能清掉的缓存，与"设置"里显示的一致
        val free = runCatching {
            val storage = context.getSystemService(StorageManager::class.java)
            storage.getAllocatableBytes(storage.getUuidForPath(context.filesDir))
        }.getOrNull()
        result.newRow().apply {
            add(Root.COLUMN_ROOT_ID, ROOT_HOME)
            add(Root.COLUMN_DOCUMENT_ID, home.path)
            add(Root.COLUMN_TITLE, context.getString(R.string.app_name))
            add(Root.COLUMN_SUMMARY, context.getString(R.string.documents_home, home.account, distro))
            // 能新建：别的 App 的"保存到"里也有它
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
        return MatrixCursor(projection ?: DOCUMENT_COLUMNS).also { row(it, path, meta, home(context)) }
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val context = attached()
        val dir = visible(context, parentDocumentId)
        val entries = browse { Native.browseList(it, dir) }
        val result = MatrixCursor(projection ?: DOCUMENT_COLUMNS)
        val home = home(context)
        for (line in entries) {
            val name = line.substringBefore('\t')
            row(result, child(dir, name), line.substringAfter('\t'), home)
        }
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
        return ParcelFileDescriptor.adoptFd(browse { Native.browseOpenWrite(it, home.path, path, truncate, append) })
    }

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
        for (line in browse { Native.browseSearch(it, dir, query, SEARCH_LIMIT) }) {
            row(result, line.substringBefore('\t'), line.substringAfter('\t'), home)
        }
        return result
    }

    /** 登录账号、它的 UID 和 GID、家目录（文档 ID 的写法）。仓库还没有、家目录不在时是 null。 */
    private data class Home(val account: String, val uid: Int, val gid: Int, val path: String)

    private fun home(context: Context): Home? {
        if (!Rootfs.isReady(context)) return null
        val login = TerminalService.login(context)
        cache?.let { (time, key, home) ->
            if (key == login && SystemClock.elapsedRealtime() - time < CACHE_MS) return home
        }
        val home = Rootfs.access.read {
            val account = Rootfs.loginAccount(context, login)
            val path = canonical("/" + account.home.trim('/')) ?: return@read null
            // 家目录不在（账号没有建家目录）时不列这个根
            runCatching { Native.browseStat(Rootfs.store(context).path, path) }.getOrNull() ?: return@read null
            Home(account.name, account.uid, account.gid, path)
        }
        cache = Triple(SystemClock.elapsedRealtime(), login, home)
        return home
    }

    /** 最近一次算出的家目录：一次浏览要查好几次，不必每次都读 /etc/passwd。 */
    @Volatile
    private var cache: Triple<Long, String, Home?>? = null

    /** 文档 ID 规范、在现在显示的根里面时返回它，不然当作不存在。 */
    private fun visible(context: Context, documentId: String): String {
        val path = canonical(documentId) ?: throw FileNotFoundException("bad document $documentId")
        if (TerminalService.showSystemDocuments(context)) return path
        val home = home(context)?.path ?: throw FileNotFoundException("no home directory")
        if (path == home || isChildDocument(home, path)) return path
        throw FileNotFoundException("$documentId is outside $home")
    }

    private fun writableHome(context: Context): Home =
        home(context) ?: throw FileNotFoundException("no home directory")

    /** 要修改的文档：规范、在家目录里面（[top] 时也可以是家目录本身），不然只读。 */
    private fun writable(home: Home, documentId: String, top: Boolean = false): String {
        val path = canonical(documentId) ?: throw FileNotFoundException("bad document $documentId")
        if ((top && path == home.path) || isChildDocument(home.path, path)) return path
        throw UnsupportedOperationException("$documentId is read-only")
    }

    /** 目录 [dir] 里的东西变了："文件"App 重新列它。 */
    private fun childrenChanged(context: Context, dir: String) {
        context.contentResolver.notifyChange(DocumentsContract.buildChildDocumentsUri(authority(context), dir), null)
    }

    /** ContentProvider 的 requireContext 要 API 30。 */
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

    private fun row(cursor: MatrixCursor, path: String, meta: String, home: Home?) {
        val f = meta.split('\t')
        if (f.size < 4) return
        val dir = f[0] == "d"
        val name = displayName(path)
        cursor.newRow().apply {
            add(Document.COLUMN_DOCUMENT_ID, path)
            add(Document.COLUMN_DISPLAY_NAME, name)
            add(Document.COLUMN_MIME_TYPE, if (dir) Document.MIME_TYPE_DIR else mimeType(name))
            add(Document.COLUMN_SIZE, if (dir) null else f[1].toLongOrNull())
            add(Document.COLUMN_LAST_MODIFIED, f[2].toLongOrNull())
            add(Document.COLUMN_FLAGS, flags(path, dir, home?.path))
        }
    }

    /**
     * 文档能做什么：家目录里面的能删除、改名、移动，文件能写，目录能在里面新建；家目录本身只能
     * 在里面新建；别处只读。按路径的字面判断，经由符号链接走出去的由原生库拒绝。
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

        /** 一次搜索最多给出这么多个。 */
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

        /** 登录账号、仓库、"显示整个系统"变了：让"文件"App 重新读根。 */
        fun rootsChanged(context: Context) {
            context.contentResolver.notifyChange(DocumentsContract.buildRootsUri(authority(context)), null)
        }

        /** 规范的路径：`/` 开头，没有空的、`.`、`..` 组件，不以 `/` 结尾（`/` 本身除外）。不是时是 null。 */
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
         * 新建、改名时用的名字：`/` 和 NUL 换成 `_`，空的、`.`、`..` 换成 `_`。新建文件时名字的
         * 扩展名与 [mime] 不符的，照 Android 自己的存储那样补上（"文本"、text/plain → "文本.txt"）；
         * 目录、application/octet-stream 不补。
         */
        fun fileName(displayName: String, mime: String, directory: Boolean): String {
            val name = displayName.replace('/', '_').replace('\u0000', '_')
                .let { if (it.isEmpty() || it == "." || it == "..") "_" else it }
            if (directory || mime.isEmpty() || mime == "application/octet-stream") return name
            if (mimeType(name) == mime) return name
            val ext = MimeTypeMap.getSingleton().getExtensionFromMimeType(mime) ?: return name
            return "$name.$ext"
        }

        /** 路径最后一个组件给人看的名字：`%XX` 换回字节，按 UTF-8 解码（不是 UTF-8 的字节显示成 �）。 */
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

        /** 按扩展名猜 MIME 类型，猜不出时是 application/octet-stream。 */
        fun mimeType(name: String): String {
            val ext = name.substringAfterLast('.', "").lowercase()
            if (ext.isEmpty()) return "application/octet-stream"
            return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext) ?: "application/octet-stream"
        }
    }
}
