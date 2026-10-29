//! appine 的 Android 原生库（设计文档 §3、§11）：Kotlin 外壳经由 JNI 使用内核层。
//!
//! 对应的 Kotlin 声明是 `android/app/src/main/java/io/github/mthli/appine/Native.kt`。
//! [`Machine`] 以 `Arc` 的裸指针交给 Kotlin（`long` 句柄），每次调用临时增加一个引用，
//! 所以 `stop` 和别的线程上正在阻塞的读并发时也是安全的：最后一个引用消失时才析构。
//! 出错时抛出 `java.io.IOException`。阻塞的调用（读控制台、等进程结束）由 Kotlin 在
//! 后台线程上进行。内核层的 bug（panic）在 [`Machine`] 的方法里就兜住了，变成这些调用
//! 的错误，不会穿过 JNI 边界让 App 进程 abort。
//!
//! 只为 Android 编译，其他宿主上是一个空库。
#![cfg(target_os = "android")]

use std::ffi::CString;
use std::path::PathBuf;
use std::sync::{Arc, Once};

use appine_kernel::{ExitStatus, HostRoute, Machine, MachineOptions, Stdio, Tier};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JObjectArray, JString};
use jni::sys::{jboolean, jint, jlong};

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

/// 句柄对应的 Machine，调用期间持有一个引用。
fn machine(handle: jlong) -> Arc<Machine> {
    let ptr = handle as *const Machine;
    // SAFETY: handle 来自 start 里的 Arc::into_raw，Kotlin 保证 stop 之后不再使用它；
    // stop 只释放它自己的那个引用
    unsafe {
        Arc::increment_strong_count(ptr);
        Arc::from_raw(ptr)
    }
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
    };
    match Machine::start(&options) {
        Ok(m) => Arc::into_raw(Arc::new(m)) as jlong,
        Err(e) => {
            throw(&mut env, format!("cannot start: {e}"));
            0
        }
    }
}

/// 结束所有 guest 进程，释放 Kotlin 持有的引用。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_stop<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    machine(handle).kill_all();
    // SAFETY: 释放 start 交出去的那个引用
    drop(unsafe { Arc::from_raw(handle as *const Machine) });
}

/// 宿主内存紧张：丢掉可以重建的缓存（池里保留的翻译等），提交推迟的写事务。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_trimMemory<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    machine(handle).trim_memory();
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
    machine(handle).set_routes(routes);
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
    match machine(handle).open_console(rows, cols) {
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
    let stdio = Stdio::Console(console as u32);
    match machine(handle).spawn(program.as_bytes(), &argv, &envp, stdio) {
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
    let mut buf = vec![0u8; len as usize];
    match machine(handle).read_console(console as u32, &mut buf) {
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
    let m = machine(handle);
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
    let _ = machine(handle).resize_console(
        console as u32,
        rows.clamp(1, 0xffff) as u16,
        cols.clamp(1, 0xffff) as u16,
    );
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_closeConsole<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
) {
    let _ = machine(handle).close_console(console as u32);
}

/// 等进程结束：返回退出码，被信号终止时返回负的信号编号（与 Termux 的约定相同）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_Native_waitFor<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    pid: jint,
) -> jint {
    match machine(handle).wait(pid as u32) {
        ExitStatus::Exited(code) => code,
        ExitStatus::Signaled { signal, .. } => -signal.number(),
    }
}
