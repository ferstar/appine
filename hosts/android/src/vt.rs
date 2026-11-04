//! The experimental terminal view (the `vt` feature, android/terminal map): each tab of `VtView`
//! has a libghostty-vt terminal here, fed straight from its kernel console, so console output never
//! crosses JNI as bytes.
//!
//! The matching Kotlin declarations are in
//! `android/app/src/debug/java/io/github/mthli/appine/VtNative.kt`. Like [`Machine`]s in the crate
//! root, tabs are registered in a table and Kotlin holds a handle.
//!
//! Threads: Kotlin's pump thread runs [`pump`]'s loop (read the console, feed the terminal, write
//! the terminal's replies back) until the session ends; the main thread takes frames, resizes,
//! scrolls and encodes keys. The terminal is not thread-safe, so every call into vt.c holds the
//! tab's lock; the frame buffer it returns is only written by `frame`, which only the main thread
//! calls, so the main thread may read it after the lock is released.
//!
//! [`pump`]: Java_io_github_mthli_appine_VtNative_pump

use std::collections::BTreeMap;
use std::ffi::c_char;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use appine_kernel::Machine;
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
use jni::sys::{jboolean, jbyteArray, jint, jlong, jobject};

use super::{machine, string, throw};

#[repr(C)]
struct AppineVt {
    _opaque: [u8; 0],
}

// vt.c
unsafe extern "C" {
    fn appine_vt_new(cols: u16, rows: u16, cell_width: u32, cell_height: u32) -> *mut AppineVt;
    fn appine_vt_free(vt: *mut AppineVt);
    fn appine_vt_feed(vt: *mut AppineVt, data: *const u8, len: usize);
    fn appine_vt_take_replies(vt: *mut AppineVt, len: *mut usize) -> *const u8;
    fn appine_vt_resize(vt: *mut AppineVt, cols: u16, rows: u16, cell_width: u32, cell_height: u32);
    fn appine_vt_scroll(vt: *mut AppineVt, delta: i32);
    fn appine_vt_frame(vt: *mut AppineVt, full: bool, len: *mut usize) -> *const u8;
    #[allow(clippy::too_many_arguments)]
    fn appine_vt_key(
        vt: *mut AppineVt,
        key: i32,
        mods: u16,
        consumed: u16,
        action: i32,
        text: *const c_char,
        text_len: usize,
        unshifted: u32,
        out: *mut c_char,
        cap: usize,
    ) -> isize;
    fn appine_vt_bracketed_paste(vt: *mut AppineVt) -> bool;
}

/// Owns one vt.c terminal.
struct Term(*mut AppineVt);

// SAFETY: the terminal is only used under the tab's lock (module docs)
unsafe impl Send for Term {}

impl Term {
    /// Feeds bytes and returns the replies they produced.
    fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        unsafe { appine_vt_feed(self.0, data.as_ptr(), data.len()) };
        let mut len = 0;
        let p = unsafe { appine_vt_take_replies(self.0, &mut len) };
        if len == 0 {
            return Vec::new();
        }
        // SAFETY: vt.c returns len valid bytes, kept until the next feed
        unsafe { std::slice::from_raw_parts(p, len) }.to_vec()
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        unsafe { appine_vt_free(self.0) };
    }
}

struct Tab {
    machine: Arc<Machine>,
    console: u32,
    term: Mutex<Term>,
    /// Output changed the terminal since the last frame. The pump tells the view only when it sets
    /// this, so a burst of output asks for one frame; `frame` clears it before capturing.
    dirty: AtomicBool,
}

impl Tab {
    fn term(&self) -> MutexGuard<'_, Term> {
        self.term.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes input to the console until all of it is taken; false once the console is gone.
    fn write(&self, mut data: &[u8]) -> bool {
        while !data.is_empty() {
            match self.machine.write_console(self.console, data) {
                Ok(n) => data = &data[n..],
                Err(_) => return false,
            }
        }
        true
    }
}

static TABS: Mutex<BTreeMap<jlong, Arc<Tab>>> = Mutex::new(BTreeMap::new());
static NEXT_TAB: AtomicI64 = AtomicI64::new(1);

fn tab(handle: jlong) -> Option<Arc<Tab>> {
    TABS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&handle)
        .cloned()
}

fn size(cols: jint, rows: jint, cell_width: jint, cell_height: jint) -> (u16, u16, u32, u32) {
    (
        cols.clamp(1, 0xffff) as u16,
        rows.clamp(1, 0xffff) as u16,
        cell_width.max(1) as u32,
        cell_height.max(1) as u32,
    )
}

/// Creates the terminal of an open console; returns the tab handle.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_create<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    console: jint,
    cols: jint,
    rows: jint,
    cell_width: jint,
    cell_height: jint,
) -> jlong {
    let Some(m) = machine(handle) else {
        throw(&mut env, "the machine is stopped");
        return 0;
    };
    let (cols, rows, cw, ch) = size(cols, rows, cell_width, cell_height);
    let raw = unsafe { appine_vt_new(cols, rows, cw, ch) };
    if raw.is_null() {
        throw(&mut env, "cannot create a terminal");
        return 0;
    }
    let id = NEXT_TAB.fetch_add(1, Ordering::Relaxed);
    let tab = Tab {
        machine: m,
        console: console as u32,
        term: Mutex::new(Term(raw)),
        dirty: AtomicBool::new(false),
    };
    TABS.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, Arc::new(tab));
    id
}

/// Forgets a tab; its terminal is freed once a running pump has returned too.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_destroy<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    let removed = TABS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&handle);
    drop(removed);
}

/// Moves the console's output into the terminal until the session ends or the console is closed;
/// calls `view.onOutput()` whenever the screen changed after the view took its last frame.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_pump<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    view: JObject<'l>,
) {
    let Some(tab) = tab(handle) else { return };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match tab.machine.read_console(tab.console, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let replies = tab.term().feed(&buf[..n]);
        // A program waiting for an answer (DA, DSR) does not read further until it comes
        if !replies.is_empty() && !tab.write(&replies) {
            break;
        }
        if !tab.dirty.swap(true, Ordering::AcqRel)
            && env.call_method(&view, "onOutput", "()V", &[]).is_err()
        {
            let _ = env.exception_clear();
            break;
        }
    }
}

/// Captures a frame (vt.c's format) of the rows that changed, or of all rows with `full`. The
/// buffer stays valid until the next call, which only the main thread makes.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_frame<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    full: jboolean,
) -> jobject {
    let Some(tab) = tab(handle) else {
        return std::ptr::null_mut();
    };
    tab.dirty.store(false, Ordering::Release);
    let mut len = 0;
    let p = unsafe { appine_vt_frame(tab.term().0, full != 0, &mut len) };
    if p.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: vt.c keeps the frame until the next appine_vt_frame call (above)
    match unsafe { env.new_direct_byte_buffer(p as *mut u8, len) } {
        Ok(b) => b.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Resizes the terminal and then the console (whose foreground group gets SIGWINCH).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_resize<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    cols: jint,
    rows: jint,
    cell_width: jint,
    cell_height: jint,
) {
    let Some(tab) = tab(handle) else { return };
    let (cols, rows, cw, ch) = size(cols, rows, cell_width, cell_height);
    unsafe { appine_vt_resize(tab.term().0, cols, rows, cw, ch) };
    let _ = tab.machine.resize_console(tab.console, rows, cols);
}

/// Scrolls the viewport by `delta` rows (negative: back); 0 returns to the bottom.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_scroll<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    delta: jint,
) {
    if let Some(tab) = tab(handle) {
        unsafe { appine_vt_scroll(tab.term().0, delta) };
    }
}

/// Encodes a key for the program in the terminal's current modes (cursor keys, keypad, Kitty
/// keyboard protocol); null when it fails. `text` is what the key types without Ctrl and Alt.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_key<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    key: jint,
    mods: jint,
    consumed: jint,
    action: jint,
    text: JString<'l>,
    unshifted: jint,
) -> jbyteArray {
    let Some(tab) = tab(handle) else {
        return std::ptr::null_mut();
    };
    let text = if text.is_null() {
        String::new()
    } else {
        string(&mut env, &text).unwrap_or_default()
    };
    let mut out = [0 as c_char; 128];
    let n = unsafe {
        appine_vt_key(
            tab.term().0,
            key,
            mods as u16,
            consumed as u16,
            action,
            if text.is_empty() {
                std::ptr::null()
            } else {
                text.as_ptr().cast()
            },
            text.len(),
            unshifted as u32,
            out.as_mut_ptr(),
            out.len(),
        )
    };
    let Ok(n) = usize::try_from(n) else {
        return std::ptr::null_mut();
    };
    // SAFETY: c_char and u8 have the same layout
    let bytes = unsafe { std::slice::from_raw_parts(out.as_ptr().cast::<u8>(), n) };
    env.byte_array_from_slice(bytes)
        .map_or(std::ptr::null_mut(), JByteArray::into_raw)
}

/// Whether the program asked for bracketed paste (mode 2004).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_bracketedPaste<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jboolean {
    tab(handle).is_some_and(|tab| unsafe { appine_vt_bracketed_paste(tab.term().0) }) as jboolean
}

/// Shows bytes from the app itself (not the program) as if the console had printed them; replies
/// are dropped.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_print<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    data: JByteArray<'l>,
) {
    let Some(tab) = tab(handle) else { return };
    let Ok(data) = env.convert_byte_array(&data) else {
        return;
    };
    tab.term().feed(&data);
}
