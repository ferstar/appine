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

use appine_kernel::{ExitStatus, HostRoute, Machine, MachineOptions, Power, Stdio, Tier};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JObjectArray, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jobjectArray, jstring};

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
/// `<控制台或 -1>\t<pid>\t<进程名>\t<bug|oom>\t<a>\t<b>\t<c>`，bug 的 a 是报告（换行、制表符
/// 换成空格），oom 的 a 是 cgroup（App 的上限时是空的）、b 和 c 是用量和上限（字节）。
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
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    for (i, k) in kills.iter().enumerate() {
        let console = k.console.map_or(-1, i64::from);
        let tail = match &k.reason {
            appine_kernel::KillReason::Bug(report) => format!("bug\t{}\t\t", clean(report)),
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

/// 按 `/etc/inittab` 开机（设计文档 §6）：阻塞到 sysinit、wait 的命令都结束。
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
