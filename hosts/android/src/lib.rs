//! appine 的 Android 原生库（设计文档 §3、§11）：Kotlin 外壳经由 JNI 使用内核层。
//!
//! 对应的 Kotlin 声明是 `android/app/src/main/java/io/github/mthli/appine/Native.kt`。
//! [`Machine`] 登记在一张表里，交给 Kotlin 的是编号（`long` 句柄）。每次调用临时取一个
//! 引用，所以 `stop` 和别的线程上正在阻塞的读并发时也是安全的：最后一个引用消失时才析构。
//! `stop` 把它从表里删掉，之后用这个句柄的调用（会话结束之后才清理的 `closeConsole` 等）
//! 查不到它，当作控制台已经关闭。
//! 出错时抛出 `java.io.IOException`。阻塞的调用（读控制台、等进程结束）由 Kotlin 在
//! 后台线程上进行。内核层的 bug（panic）在 [`Machine`] 的方法里就兜住了，变成这些调用
//! 的错误，不会穿过 JNI 边界让 App 进程 abort。
//!
//! 只为 Android 编译，其他宿主上是一个空库。
#![cfg(target_os = "android")]

use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Once, PoisonError};

use appine_kernel::vfs::browse;
use appine_kernel::{ExitStatus, HostRoute, Machine, MachineOptions, Power, Stdio, Tier};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JObjectArray, JString};
use jni::sys::{jboolean, jbooleanArray, jbyteArray, jint, jlong, jobjectArray, jstring};

/// 抛出 `java.io.IOException`；JNI 调用之后由 Kotlin 看到。
fn throw(env: &mut JNIEnv<'_>, message: impl AsRef<str>) {
    let _ = env.throw_new("java/io/IOException", message.as_ref());
}

fn string(env: &mut JNIEnv<'_>, s: &JString<'_>) -> Option<String> {
    env.get_string(s).ok().map(Into::into)
}

fn strings(env: &mut JNIEnv<'_>, array: &JObjectArray<'_>) -> Option<Vec<Vec<u8>>> {
    let n = env.get_array_length(array).ok()?;
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let item = JString::from(env.get_object_array_element(array, i).ok()?);
        out.push(string(env, &item)?.into_bytes());
    }
    Some(out)
}

/// 句柄 → Machine（模块文档）。
static MACHINES: Mutex<BTreeMap<jlong, Arc<Machine>>> = Mutex::new(BTreeMap::new());
static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);

fn machines() -> std::sync::MutexGuard<'static, BTreeMap<jlong, Arc<Machine>>> {
    MACHINES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 句柄对应的 Machine，调用期间持有一个引用；已经 stop 了是 None。
fn machine(handle: jlong) -> Option<Arc<Machine>> {
    machines().get(&handle).cloned()
}

/// 把 stderr（内核层的日志、内部错误）转到 logcat。
fn redirect_stderr() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        #[link(name = "log")]
        unsafe extern "C" {
            fn __android_log_write(
                prio: i32,
                tag: *const libc::c_char,
                text: *const libc::c_char,
            ) -> i32;
        }
        const ANDROID_LOG_INFO: i32 = 4;
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 || unsafe { libc::dup2(fds[1], 2) } < 0 {
            return;
        }
        unsafe { libc::close(fds[1]) };
        let read_end = fds[0];
        let _ = std::thread::Builder::new()
            .name("stderr-log".into())
            .spawn(move || {
                use std::io::{BufRead, BufReader};
                use std::os::fd::FromRawFd;
                let input = unsafe { std::fs::File::from_raw_fd(read_end) };
                let tag = c"appine";
                for line in BufReader::new(input).split(b'\n').map_while(Result::ok) {
                    if let Ok(text) = CString::new(line) {
                        unsafe {
                            __android_log_write(ANDROID_LOG_INFO, tag.as_ptr(), text.as_ptr())
                        };
                    }
                }
            });
    });
}

/// 第一次调用别的函数之前调用：stderr 转到 logcat，`verbose` 时打开内核层的调试日志。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_init<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    verbose: jboolean,
) {
    redirect_stderr();
    if verbose != 0 {
        // SAFETY: 在用到内核层之前设置，这时还没有别的线程读环境变量
        unsafe { std::env::set_var("APPINE_LOG", "1") };
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_importRootfs<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    tarball: JString<'l>,
) {
    let (Some(store), Some(tarball)) = (string(&mut env, &store), string(&mut env, &tarball))
    else {
        return;
    };
    if let Err(e) = appine_kernel::import_rootfs(store.as_ref(), tarball.as_ref()) {
        throw(&mut env, format!("cannot import the rootfs: {e}"));
    }
}

/// 把仓库写到盘上（导入、写好配置之后，改名就位之前）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_syncRootfs<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
) {
    let Some(store) = string(&mut env, &store) else {
        return;
    };
    if let Err(e) = appine_kernel::sync_store(store.as_ref()) {
        throw(&mut env, format!("cannot sync the rootfs: {e}"));
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_writeFile<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    path: JString<'l>,
    data: JByteArray<'l>,
    mode: jint,
    overwrite: jboolean,
) {
    let (Some(store), Some(path)) = (string(&mut env, &store), string(&mut env, &path)) else {
        return;
    };
    let Ok(data) = env.convert_byte_array(&data) else {
        return;
    };
    let r = appine_kernel::write_file(store.as_ref(), &path, &data, mode as u32, overwrite != 0);
    if let Err(e) = r {
        throw(&mut env, format!("cannot write {path}: {e}"));
    }
}

/// 读仓库里的一个普通文件（宿主看配置文件用，比如设置里列出账号）：不存在时返回 null，别的
/// 错误抛出 IOException。内核运行着也可以读：另外打开仓库时，内核推迟的写事务先提交。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_readFile<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    path: JString<'l>,
) -> jbyteArray {
    let (Some(store), Some(path)) = (string(&mut env, &store), string(&mut env, &path)) else {
        return std::ptr::null_mut();
    };
    match appine_kernel::read_file(store.as_ref(), &path) {
        Ok(data) => env
            .byte_array_from_slice(&data)
            .map(JByteArray::into_raw)
            .unwrap_or(std::ptr::null_mut()),
        Err(appine_kernel::errno::Errno::ENOENT) => std::ptr::null_mut(),
        Err(e) => {
            throw(&mut env, format!("cannot read {path}: {e}"));
            std::ptr::null_mut()
        }
    }
}

// ---- 宿主浏览仓库（DocumentsProvider，设计文档 §11.3）----
//
// 路径在 Java 一侧是字符串（文档 ID）：guest 的文件名是任意字节，合法 UTF-8 的部分照原样，
// `%`、控制字符（列表的每一行用制表符分隔）和不是 UTF-8 的字节写成 `%XX`，能原样换回来。
// 每一项是 `<类型 d|f>\t<字节数>\t<修改时间（毫秒）>\t<权限位>`，列表、搜索的结果前面再加
// 名字或路径和一个制表符。出错时抛出 FileNotFoundException（IOException 的子类）。

fn stat_fields(st: &appine_kernel::vfs::store::Stat) -> String {
    let kind = if st.is_dir() { 'd' } else { 'f' };
    let perm = st.mode & 0o7777;
    format!(
        "{kind}\t{}\t{}\t{perm}",
        st.size,
        st.mtime_ns.div_euclid(1_000_000)
    )
}

fn not_found(env: &mut JNIEnv<'_>, what: &str, path: &str, e: appine_kernel::errno::Errno) {
    let _ = env.throw_new(
        "java/io/FileNotFoundException",
        format!("cannot {what} {path}: {e}"),
    );
}

fn string_array(env: &mut JNIEnv<'_>, items: &[String]) -> jobjectArray {
    let Ok(array) = env.new_object_array(items.len() as jint, "java/lang/String", JObject::null())
    else {
        return std::ptr::null_mut();
    };
    for (i, item) in items.iter().enumerate() {
        let Ok(s) = env.new_string(item) else {
            return std::ptr::null_mut();
        };
        if env.set_object_array_element(&array, i as jint, s).is_err() {
            return std::ptr::null_mut();
        }
    }
    array.into_raw()
}

/// 路径（文档 ID）指向的目录或普通文件（符号链接跟随）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseStat<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    path: JString<'l>,
) -> jstring {
    let (Some(store), Some(path)) = (string(&mut env, &store), string(&mut env, &path)) else {
        return std::ptr::null_mut();
    };
    let r = appine_kernel::browse(store.as_ref(), |s| {
        browse::stat(s, &browse::decode_path(&path))
    });
    match r {
        Ok(st) => env
            .new_string(stat_fields(&st))
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        Err(e) => {
            not_found(&mut env, "find", &path, e);
            std::ptr::null_mut()
        }
    }
}

/// 列出目录：每一项是 `<名字>\t<类型>\t…`，名字按 [`browse::encode_path`] 写。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseList<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    dir: JString<'l>,
) -> jobjectArray {
    let (Some(store), Some(dir)) = (string(&mut env, &store), string(&mut env, &dir)) else {
        return std::ptr::null_mut();
    };
    let r = appine_kernel::browse(store.as_ref(), |s| {
        browse::list(s, &browse::decode_path(&dir))
    });
    match r {
        Ok(entries) => {
            let lines: Vec<String> = entries
                .iter()
                .map(|e| format!("{}\t{}", browse::encode_path(&e.name), stat_fields(&e.stat)))
                .collect();
            string_array(&mut env, &lines)
        }
        Err(e) => {
            not_found(&mut env, "list", &dir, e);
            std::ptr::null_mut()
        }
    }
}

/// 打开普通文件读，返回宿主的 fd（交给 ParcelFileDescriptor.adoptFd，由它关）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseOpen<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    path: JString<'l>,
) -> jint {
    use std::os::fd::IntoRawFd;
    let (Some(store), Some(path)) = (string(&mut env, &store), string(&mut env, &path)) else {
        return -1;
    };
    let r = appine_kernel::browse(store.as_ref(), |s| {
        browse::open(s, &browse::decode_path(&path))
    });
    match r {
        Ok(file) => file.into_raw_fd(),
        Err(e) => {
            not_found(&mut env, "open", &path, e);
            -1
        }
    }
}

/// 这些路径指向的普通文件看起来是不是文本（[`browse::looks_like_text`]），一次打开仓库查完。
/// 只看前 1000 个，之后的都当作不是。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseSniff<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    paths: JObjectArray<'l>,
) -> jbooleanArray {
    /// 一次最多看多少个文件：每个要在宿主上打开、读 4 KB，大目录里不能一直看下去。
    const SNIFF_MAX: usize = 1000;
    let (Some(store), Some(paths)) = (string(&mut env, &store), strings(&mut env, &paths)) else {
        return std::ptr::null_mut();
    };
    let r = appine_kernel::browse(store.as_ref(), |s| {
        Ok(paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let p = browse::decode_path(&String::from_utf8_lossy(p));
                jboolean::from(i < SNIFF_MAX && browse::looks_like_text(s, &p))
            })
            .collect::<Vec<jboolean>>())
    });
    let texts = match r {
        Ok(t) => t,
        Err(e) => {
            not_found(&mut env, "open", &store, e);
            return std::ptr::null_mut();
        }
    };
    let Ok(array) = env.new_boolean_array(texts.len() as jint) else {
        return std::ptr::null_mut();
    };
    if env.set_boolean_array_region(&array, 0, &texts).is_err() {
        return std::ptr::null_mut();
    }
    array.into_raw()
}

/// 在目录下面按名字找（不分大小写）：每一项是 `<路径>\t<类型>\t…`。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseSearch<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    dir: JString<'l>,
    query: JString<'l>,
    limit: jint,
) -> jobjectArray {
    let (Some(store), Some(dir), Some(query)) = (
        string(&mut env, &store),
        string(&mut env, &dir),
        string(&mut env, &query),
    ) else {
        return std::ptr::null_mut();
    };
    let r = appine_kernel::browse(store.as_ref(), |s| {
        browse::search(
            s,
            &browse::decode_path(&dir),
            &query,
            limit.max(0) as usize,
            SEARCH_BUDGET,
        )
    });
    match r {
        Ok(found) => {
            let lines: Vec<String> = found
                .iter()
                .map(|(p, st)| format!("{}\t{}", browse::encode_path(p), stat_fields(st)))
                .collect();
            string_array(&mut env, &lines)
        }
        Err(e) => {
            not_found(&mut env, "search", &dir, e);
            std::ptr::null_mut()
        }
    }
}

// 修改：都限制在 `top`（登录账号的家目录）里面，按真实路径算（[`browse`] 的说明）。新的名字
// 是用户给的，照 UTF-8 原样用，不解 `%XX`；返回的名字按 [`browse::encode_path`] 写。

/// 在目录 [dir] 里新建文件或目录（[directory]），属主是 [uid]、[gid]；名字有了时换一个，
/// 返回实际用的名字。
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseCreate<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    dir: JString<'l>,
    name: JString<'l>,
    directory: jboolean,
    uid: jint,
    gid: jint,
) -> jstring {
    let (Some(store), Some(top), Some(dir), Some(name)) = (
        string(&mut env, &store),
        string(&mut env, &top),
        string(&mut env, &dir),
        string(&mut env, &name),
    ) else {
        return std::ptr::null_mut();
    };
    let r = appine_kernel::browse_changing(store.as_ref(), |s, changes| {
        browse::create(
            s,
            &browse::decode_path(&top),
            &browse::decode_path(&dir),
            name.as_bytes(),
            directory != 0,
            (uid as u32, gid as u32),
            changes,
        )
    });
    match r {
        Ok(name) => env
            .new_string(browse::encode_path(&name))
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        Err(e) => {
            not_found(&mut env, "create in", &dir, e);
            std::ptr::null_mut()
        }
    }
}

/// [browseOpenWrite] 交出去的文件关闭了：告诉运行中的内核它被写过（guest 里监视着的程序
/// 收到 `IN_MODIFY`、`IN_CLOSE_WRITE`）。文件这期间不在了也不报错。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseWritten<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
) {
    let (Some(store), Some(top), Some(path)) = (
        string(&mut env, &store),
        string(&mut env, &top),
        string(&mut env, &path),
    ) else {
        return;
    };
    let _ = appine_kernel::browse_changing(store.as_ref(), |s, changes| {
        browse::written(
            s,
            &browse::decode_path(&top),
            &browse::decode_path(&path),
            changes,
        )
    });
}

/// 删除文件、符号链接或者整个目录树。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseRemove<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
) {
    let (Some(store), Some(top), Some(path)) = (
        string(&mut env, &store),
        string(&mut env, &top),
        string(&mut env, &path),
    ) else {
        return;
    };
    let r = appine_kernel::browse_changing(store.as_ref(), |s, changes| {
        browse::remove(
            s,
            &browse::decode_path(&top),
            &browse::decode_path(&path),
            changes,
        )
    });
    if let Err(e) = r {
        not_found(&mut env, "delete", &path, e);
    }
}

/// 在同一个目录里改名（[name] 是新的名字），不覆盖。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseRename<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
    name: JString<'l>,
) -> jstring {
    let (Some(store), Some(top), Some(path), Some(name)) = (
        string(&mut env, &store),
        string(&mut env, &top),
        string(&mut env, &path),
        string(&mut env, &name),
    ) else {
        return std::ptr::null_mut();
    };
    let r = appine_kernel::browse_changing(store.as_ref(), |s, changes| {
        browse::rename(
            s,
            &browse::decode_path(&top),
            &browse::decode_path(&path),
            name.as_bytes(),
            changes,
        )
    });
    match r {
        Ok(()) => env
            .new_string(browse::encode_path(name.as_bytes()))
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut()),
        Err(e) => {
            not_found(&mut env, "rename", &path, e);
            std::ptr::null_mut()
        }
    }
}

/// 移到另一个目录 [to]，名字不变，不覆盖。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseMove<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
    to: JString<'l>,
) {
    let (Some(store), Some(top), Some(path), Some(to)) = (
        string(&mut env, &store),
        string(&mut env, &top),
        string(&mut env, &path),
        string(&mut env, &to),
    ) else {
        return;
    };
    let r = appine_kernel::browse_changing(store.as_ref(), |s, changes| {
        browse::move_to(
            s,
            &browse::decode_path(&top),
            &browse::decode_path(&path),
            &browse::decode_path(&to),
            changes,
        )
    });
    if let Err(e) = r {
        not_found(&mut env, "move", &path, e);
    }
}

/// 打开普通文件读写（[truncate] 时先截断，[append] 时以 O_APPEND 打开），返回宿主的 fd。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_browseOpenWrite<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    top: JString<'l>,
    path: JString<'l>,
    truncate: jboolean,
    append: jboolean,
) -> jint {
    use std::os::fd::IntoRawFd;
    let (Some(store), Some(top), Some(path)) = (
        string(&mut env, &store),
        string(&mut env, &top),
        string(&mut env, &path),
    ) else {
        return -1;
    };
    let r = appine_kernel::browse_changing(store.as_ref(), |s, changes| {
        browse::open_write(
            s,
            &browse::decode_path(&top),
            &browse::decode_path(&path),
            truncate != 0,
            append != 0,
            changes,
        )
    });
    match r {
        Ok(file) => file.into_raw_fd(),
        Err(e) => {
            not_found(&mut env, "open", &path, e);
            -1
        }
    }
}

/// 一次搜索最多看这么多项（目录项）：/usr 这样的大目录树里不会一直找下去。
const SEARCH_BUDGET: usize = 50_000;

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_start<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    store: JString<'l>,
    tier: jint,
) -> jlong {
    let Some(store) = string(&mut env, &store) else {
        return 0;
    };
    let options = MachineOptions {
        tier: if tier == 1 {
            Tier::Dbt
        } else {
            Tier::Interpreter
        },
        rootfs: Some(PathBuf::from(store)),
        memory_limit: None,
        low_port_offset: None,
        always_map_low_ports: false,
    };
    match Machine::start(&options) {
        Ok(m) => {
            let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
            machines().insert(handle, Arc::new(m));
            handle
        }
        Err(e) => {
            throw(&mut env, format!("cannot start: {e}"));
            0
        }
    }
}

/// 结束所有 guest 进程，从表里删掉句柄。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_stop<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    // 先删掉再结束：kill_all 要等，不能拿着表的锁
    let m = machines().remove(&handle);
    if let Some(m) = m {
        m.kill_all();
    }
}

/// 宿主内存紧张：丢掉可以重建的缓存（池里保留的翻译等），提交推迟的写事务。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_trimMemory<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    if let Some(m) = machine(handle) {
        m.trim_memory();
    }
}

/// `Native.kt` 交来的一条路由：`<目标网段的十六进制>/<前缀长度> <网关的十六进制或者 -> <接口>`。
/// 度量与 Linux 上同样的路由相同：IPv4 是 0，IPv6 的默认路由 1024（RA），别的 256（内核按
/// 地址加的网段）。
fn parse_route(line: &str) -> Option<HostRoute> {
    let hex = |s: &str| -> Option<Vec<u8>> {
        s.len()
            .is_multiple_of(2)
            .then(|| {
                (0..s.len() / 2)
                    .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok())
                    .collect()
            })
            .flatten()
    };
    let mut f = line.split_whitespace();
    let (dst, prefix) = f.next()?.split_once('/')?;
    let (dst, prefix): (Vec<u8>, u8) = (hex(dst)?, prefix.parse().ok()?);
    let gateway = match f.next()? {
        "-" => None,
        g => Some(hex(g)?),
    };
    let iface = f.next()?.to_owned();
    let (family, metric) = match dst.len() {
        4 => (libc::AF_INET, 0),
        16 if prefix == 0 => (libc::AF_INET6, 1024),
        16 => (libc::AF_INET6, 256),
        _ => return None,
    };
    if gateway.as_ref().is_some_and(|g| g.len() != dst.len()) {
        return None;
    }
    Some(HostRoute {
        family,
        dst,
        prefix,
        gateway,
        iface,
        metric,
    })
}

/// 宿主的默认网络变了：guest 看到的路由表换成 `routes`（每一项见 [`parse_route`]），null 时
/// 改回按接口的地址合成。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_setRoutes<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    routes: JObject<'l>,
) {
    let routes = if routes.is_null() {
        None
    } else {
        let Some(lines) = strings(&mut env, &JObjectArray::from(routes)) else {
            return;
        };
        Some(
            lines
                .iter()
                .filter_map(|l| parse_route(std::str::from_utf8(l).ok()?))
                .collect(),
        )
    };
    if let Some(m) = machine(handle) {
        m.set_routes(routes);
    }
}

/// 取走内核层结束了的 guest 进程（内核 bug、引擎故障、OOM killer）：每一项是
/// `<控制台或 -1>\t<pid>\t<进程名>\t<bug|oom>\t<a>\t<b>\t<c>`，bug 的 a 是报告、b 是内核日志的
/// 最后几行（以 U+001E 分隔），oom 的 a 是 cgroup（App 的上限时是空的）、b 和 c 是用量和上限
/// （字节）。各项里的换行、制表符、U+001E 换成空格。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_takeKills<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jobjectArray {
    let kills = machine(handle).map(|m| m.take_kills()).unwrap_or_default();
    let Ok(array) = env.new_object_array(kills.len() as jint, "java/lang/String", JObject::null())
    else {
        return std::ptr::null_mut();
    };
    let clean = |s: &str| s.replace(['\t', '\n', '\r', '\u{1e}'], " ");
    for (i, k) in kills.iter().enumerate() {
        let console = k.console.map_or(-1, i64::from);
        let tail = match &k.reason {
            appine_kernel::KillReason::Bug { report, log } => {
                let log: Vec<String> = log.iter().map(|l| clean(l)).collect();
                format!("bug\t{}\t{}\t", clean(report), log.join("\u{1e}"))
            }
            appine_kernel::KillReason::OutOfMemory {
                cgroup,
                used,
                limit,
            } => format!(
                "oom\t{}\t{used}\t{limit}",
                clean(cgroup.as_deref().unwrap_or(""))
            ),
        };
        let line = format!("{console}\t{}\t{}\t{tail}", k.pid, clean(&k.comm));
        let Ok(s) = env.new_string(line) else {
            return std::ptr::null_mut();
        };
        if env.set_object_array_element(&array, i as jint, s).is_err() {
            return std::ptr::null_mut();
        }
    }
    array.into_raw()
}

/// 取走 guest 第一次在非回环地址上监听的端口（设计文档 §12 #7）：每一项是
/// `<进程名>\t<端口>\t<宿主上的端口>`，没有时是空数组。
/// guest 改过目录项的目录（[`browse::encode_path`] 的写法，就是文档 ID）："文件"App 刷新它们。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_takeChangedDirs<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jobjectArray {
    let dirs: Vec<String> = machine(handle)
        .map(|m| m.take_changed_dirs())
        .unwrap_or_default()
        .iter()
        .map(|d| browse::encode_path(d))
        .collect();
    string_array(&mut env, &dirs)
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_takeListens<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jobjectArray {
    let events = machine(handle)
        .map(|m| m.take_listens())
        .unwrap_or_default();
    let Ok(array) = env.new_object_array(events.len() as jint, "java/lang/String", JObject::null())
    else {
        return std::ptr::null_mut();
    };
    for (i, e) in events.iter().enumerate() {
        let line = format!("{}\t{}\t{}", e.comm, e.port, e.host_port);
        let Ok(s) = env.new_string(line) else {
            return std::ptr::null_mut();
        };
        if env.set_object_array_element(&array, i as jint, s).is_err() {
            return std::ptr::null_mut();
        }
    }
    array.into_raw()
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_openConsole<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    rows: jint,
    cols: jint,
) -> jint {
    let (rows, cols) = (rows.clamp(1, 0xffff) as u16, cols.clamp(1, 0xffff) as u16);
    let Some(m) = machine(handle) else {
        throw(&mut env, "the machine is stopped");
        return -1;
    };
    match m.open_console(rows, cols) {
        Ok(id) => id as jint,
        Err(e) => {
            throw(&mut env, format!("cannot open a console: {e}"));
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_spawn<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    program: JString<'l>,
    argv: JObjectArray<'l>,
    envp: JObjectArray<'l>,
) -> jint {
    let Some(program) = string(&mut env, &program) else {
        return -1;
    };
    let (Some(argv), Some(envp)) = (strings(&mut env, &argv), strings(&mut env, &envp)) else {
        return -1;
    };
    // 负数：不要控制台，用宿主的 stdin/stdout/stderr（App 的是 /dev/null 和 logcat）
    let stdio = u32::try_from(console).map_or(Stdio::Host, Stdio::Console);
    let Some(m) = machine(handle) else {
        throw(&mut env, "the machine is stopped");
        return -1;
    };
    match m.spawn(program.as_bytes(), &argv, &envp, stdio) {
        Ok(pid) => pid as jint,
        Err(e) => {
            throw(&mut env, format!("{program}: {e}"));
            -1
        }
    }
}

/// 读控制台的输出：返回读到的字节数，0 表示会话结束，-1 表示控制台已经关闭。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_read<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    buffer: JByteArray<'l>,
) -> jint {
    let Ok(len) = env.get_array_length(&buffer) else {
        return -1;
    };
    let Some(m) = machine(handle) else {
        return -1;
    };
    let mut buf = vec![0u8; len as usize];
    match m.read_console(console as u32, &mut buf) {
        Ok(n) => {
            // SAFETY: u8 和 i8 的布局相同
            let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<i8>(), n) };
            match env.set_byte_array_region(&buffer, 0, bytes) {
                Ok(()) => n as jint,
                Err(_) => -1,
            }
        }
        Err(_) => -1,
    }
}

/// 向控制台输入：返回写入的字节数，-1 表示会话已经结束或者控制台已经关闭。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_write<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    buffer: JByteArray<'l>,
    offset: jint,
    length: jint,
) -> jint {
    let mut data = vec![0i8; length.max(0) as usize];
    if env
        .get_byte_array_region(&buffer, offset, &mut data)
        .is_err()
    {
        return -1;
    }
    // SAFETY: u8 和 i8 的布局相同
    let data = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), data.len()) };
    let Some(m) = machine(handle) else {
        return -1;
    };
    let mut done = 0;
    while done < data.len() {
        match m.write_console(console as u32, &data[done..]) {
            Ok(n) => done += n,
            Err(_) => return -1,
        }
    }
    done as jint
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_resize<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    rows: jint,
    cols: jint,
) {
    if let Some(m) = machine(handle) {
        let _ = m.resize_console(
            console as u32,
            rows.clamp(1, 0xffff) as u16,
            cols.clamp(1, 0xffff) as u16,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_closeConsole<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
) {
    if let Some(m) = machine(handle) {
        let _ = m.close_console(console as u32);
    }
}

/// 等进程结束：返回退出码，被信号终止时返回负的信号编号（与 Termux 的约定相同）。内核已经
/// stop 时进程都被 SIGKILL 结束了。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_waitFor<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    pid: jint,
) -> jint {
    let Some(m) = machine(handle) else {
        return -9;
    };
    match m.wait(pid as u32) {
        ExitStatus::Exited(code) => code,
        ExitStatus::Signaled { signal, .. } => -signal.number(),
    }
}

/// 按 `/etc/inittab` 开机（kernel/process 的 map）：阻塞到 sysinit、wait 的命令都结束。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_boot<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    if let Some(m) = machine(handle) {
        m.boot();
    }
}

/// 关机（[`Machine::shutdown`]）：运行 inittab 里关机的命令，给所有进程 SIGTERM，再结束
/// 剩下的，然后从表里删掉句柄。关机的命令期间别的调用照常能用。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_shutdown<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    if let Some(m) = machine(handle) {
        m.shutdown(Power::PowerOff, appine_kernel::SHUTDOWN_TIMEOUT);
    }
    machines().remove(&handle);
}

/// 取走 guest 的关机请求（busybox 的 reboot、halt、poweroff）：0 没有，1 重启，2 停机，
/// 3 关机。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_takePower<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jint {
    match machine(handle).and_then(|m| m.take_power()) {
        None => 0,
        Some(Power::Reboot) => 1,
        Some(Power::Halt) => 2,
        Some(Power::PowerOff) => 3,
    }
}

/// 还在运行的 guest 进程数（不算 init）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_processes<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jint {
    machine(handle).map_or(0, |m| m.processes() as jint)
}

/// 控制台前台进程组的当前目录（App 被杀之后恢复标签页用）：取不到时（shell 已经退出、
/// 句柄无效）返回 null。路径不是 UTF-8 时按 UTF-8 的规则替换。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_consoleCwd<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
) -> jstring {
    let Some(path) = machine(handle).and_then(|m| m.console_cwd(console as u32).ok()) else {
        return std::ptr::null_mut();
    };
    env.new_string(String::from_utf8_lossy(&path))
        .map(JString::into_raw)
        .unwrap_or(std::ptr::null_mut())
}
