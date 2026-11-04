//! The libghostty-vt terminals of the app's tabs (the `vt` feature, android/terminal map): each
//! `VtSession` has a terminal here, fed straight from its kernel console, so console output never
//! crosses JNI as bytes.
//!
//! The matching Kotlin declarations are in
//! `android/app/src/main/java/io/github/mthli/appine/VtNative.kt`. Like [`Machine`]s in the crate
//! root, tabs are registered in a table and Kotlin holds a handle.
//!
//! A tab names its kernel by the crate root's handle too and looks it up for each call, as the
//! `Native` functions do, instead of holding an `Arc<Machine>`: `Native.stop` and `shutdown` only
//! remove the handle and end the processes, and the store is closed when the last reference is
//! dropped. A tab outliving its kernel would keep the old store open after a reboot or a distro
//! switch, and the next kernel would then start without the boot cleanup.
//!
//! Threads: Kotlin's pump thread runs [`pump`]'s loop (read the console, feed the terminal, write
//! the terminal's replies back) until the session ends; the main thread takes frames, resizes,
//! scrolls, encodes keys, mouse and focus events and pastes, selects and copies, and reads the title
//! and transcript. The terminal is not thread-safe, so every call into vt.c holds the
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
use jni::sys::{jboolean, jbyteArray, jfloat, jint, jlong, jobject, jstring};

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
    fn appine_vt_paste(vt: *mut AppineVt, data: *const u8, len: usize, allow_unsafe: bool) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn appine_vt_mouse(
        vt: *mut AppineVt,
        action: i32,
        button: i32,
        mods: u16,
        x: f32,
        y: f32,
        pressed: bool,
        out: *mut c_char,
        cap: usize,
    ) -> isize;
    fn appine_vt_focus(vt: *mut AppineVt, gained: bool, out: *mut c_char, cap: usize) -> isize;
    fn appine_vt_select_word(vt: *mut AppineVt, x: u16, y: u16) -> bool;
    fn appine_vt_select_grab(vt: *mut AppineVt, end: bool) -> bool;
    fn appine_vt_select_to(vt: *mut AppineVt, x: u16, y: u16) -> bool;
    fn appine_vt_select_all(vt: *mut AppineVt) -> bool;
    fn appine_vt_select_clear(vt: *mut AppineVt);
    fn appine_vt_copy(vt: *mut AppineVt, len: *mut usize) -> *const u8;
    fn appine_vt_take_title_changed(vt: *mut AppineVt) -> bool;
    fn appine_vt_title(vt: *mut AppineVt, len: *mut usize) -> *const u8;
    fn appine_vt_message(vt: *mut AppineVt, text: *const u8, len: usize) -> bool;
    fn appine_vt_transcript(vt: *mut AppineVt, len: *mut usize) -> *const u8;
}

/// Owns one vt.c terminal.
struct Term(*mut AppineVt);

// SAFETY: the terminal is only used under the tab's lock (module docs)
unsafe impl Send for Term {}

impl Term {
    /// Feeds bytes and returns the replies they produced.
    fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        unsafe { appine_vt_feed(self.0, data.as_ptr(), data.len()) };
        self.take_replies()
    }

    /// Text vt.c returned (`len` bytes at `p`, kept until its next text call) as a Java string;
    /// null for a null `p`. Called with the lock held.
    fn text(&self, env: &mut JNIEnv<'_>, p: *const u8, len: usize) -> jstring {
        if p.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: vt.c keeps len bytes at p until its next text call, which needs the lock
        let text = String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(p, len) });
        env.new_string(text)
            .map_or(std::ptr::null_mut(), JString::into_raw)
    }

    /// The bytes the terminal wrote for the console since the last call (replies, a paste).
    fn take_replies(&mut self) -> Vec<u8> {
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
    /// The kernel's handle in the crate root's table ([`machine`]).
    machine: jlong,
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

    /// The kernel, for the duration of one call; None once it is stopped.
    fn machine(&self) -> Option<Arc<Machine>> {
        machine(self.machine)
    }

    /// Writes input to the console of `m` (this tab's kernel) until all of it is taken; false once
    /// the console is gone.
    fn write(&self, m: &Machine, mut data: &[u8]) -> bool {
        while !data.is_empty() {
            match m.write_console(self.console, data) {
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

/// A viewport coordinate from Kotlin, clamped to the grid's range (vt.c clamps to the grid).
fn cell(v: jint) -> u16 {
    v.clamp(0, 0xffff) as u16
}

fn bytes_array(env: &mut JNIEnv<'_>, bytes: &[u8]) -> jbyteArray {
    env.byte_array_from_slice(bytes)
        .map_or(std::ptr::null_mut(), JByteArray::into_raw)
}

/// The first `n` bytes of an encoder's output, null when it failed (`n` < 0).
fn encoded(env: &mut JNIEnv<'_>, out: &[c_char], n: isize) -> jbyteArray {
    let Ok(n) = usize::try_from(n) else {
        return std::ptr::null_mut();
    };
    // SAFETY: c_char and u8 have the same layout, and the encoder wrote n <= out.len() bytes
    bytes_array(env, unsafe {
        std::slice::from_raw_parts(out.as_ptr().cast::<u8>(), n)
    })
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
    if machine(handle).is_none() {
        throw(&mut env, "the machine is stopped");
        return 0;
    }
    let (cols, rows, cw, ch) = size(cols, rows, cell_width, cell_height);
    let raw = unsafe { appine_vt_new(cols, rows, cw, ch) };
    if raw.is_null() {
        throw(&mut env, "cannot create a terminal");
        return 0;
    }
    let id = NEXT_TAB.fetch_add(1, Ordering::Relaxed);
    let tab = Tab {
        machine: handle,
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
/// calls `session.onOutput()` whenever the screen changed after the last frame was taken, and
/// `session.onTitle()` when a program set the title.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_pump<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    session: JObject<'l>,
) {
    let Some(tab) = tab(handle) else { return };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // Looked up again for every read, so the kernel is not kept once the service stops it
        let Some(m) = tab.machine() else { break };
        let n = match m.read_console(tab.console, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let (replies, titled) = {
            let mut term = tab.term();
            let replies = term.feed(&buf[..n]);
            (replies, unsafe { appine_vt_take_title_changed(term.0) })
        };
        // A program waiting for an answer (DA, DSR) does not read further until it comes
        if !replies.is_empty() && !tab.write(&m, &replies) {
            break;
        }
        // Not kept across the calls into Java either
        drop(m);
        let output = !tab.dirty.swap(true, Ordering::AcqRel);
        if (output && env.call_method(&session, "onOutput", "()V", &[]).is_err())
            || (titled && env.call_method(&session, "onTitle", "()V", &[]).is_err())
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
    if let Some(m) = tab.machine() {
        let _ = m.resize_console(tab.console, rows, cols);
    }
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
    encoded(&mut env, &out, n)
}

/// Pastes clipboard text as the terminal's modes ask (bracketed paste, line breaks, unsafe
/// control bytes); returns the bytes for the console, or null when the text could run commands
/// and `allow_unsafe` is false (the view asks first and tries again). An empty array when it
/// failed.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_paste<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    text: JString<'l>,
    allow_unsafe: jboolean,
) -> jbyteArray {
    let Some(tab) = tab(handle) else {
        return std::ptr::null_mut();
    };
    let Some(text) = string(&mut env, &text) else {
        return std::ptr::null_mut();
    };
    let bytes = {
        let mut term = tab.term();
        match unsafe { appine_vt_paste(term.0, text.as_ptr(), text.len(), allow_unsafe != 0) } {
            0 => term.take_replies(),
            1 => return std::ptr::null_mut(),
            _ => Vec::new(),
        }
    };
    bytes_array(&mut env, &bytes)
}

/// Encodes a mouse event at pixel (`x`, `y`) of the grid; empty when the program does not take
/// it, null when encoding failed. `action`: 0 press, 1 release, 2 motion; `button`: 0 none,
/// 1 left, 4 and 5 the wheel.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_mouse<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    action: jint,
    button: jint,
    mods: jint,
    x: jfloat,
    y: jfloat,
    pressed: jboolean,
) -> jbyteArray {
    let Some(tab) = tab(handle) else {
        return std::ptr::null_mut();
    };
    let mut out = [0 as c_char; 64];
    let n = unsafe {
        appine_vt_mouse(
            tab.term().0,
            action,
            button,
            mods as u16,
            x,
            y,
            pressed != 0,
            out.as_mut_ptr(),
            out.len(),
        )
    };
    encoded(&mut env, &out, n)
}

/// Encodes focus gained or lost; empty when the program did not ask for focus events.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_focus<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    gained: jboolean,
) -> jbyteArray {
    let Some(tab) = tab(handle) else {
        return std::ptr::null_mut();
    };
    let mut out = [0 as c_char; 16];
    let n = unsafe { appine_vt_focus(tab.term().0, gained != 0, out.as_mut_ptr(), out.len()) };
    encoded(&mut env, &out, n)
}

/// Selects the word at viewport cell (`x`, `y`) and anchors drags to it; false when there is no
/// word there (the selection is then cleared).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_selectWord<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
) -> jboolean {
    tab(handle).is_some_and(|tab| unsafe { appine_vt_select_word(tab.term().0, cell(x), cell(y)) })
        as jboolean
}

/// Starts dragging the handle of the selection's last cell (`end`) or first cell: the other
/// end becomes the anchor. False when there is no selection.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_selectGrab<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    end: jboolean,
) -> jboolean {
    tab(handle).is_some_and(|tab| unsafe { appine_vt_select_grab(tab.term().0, end != 0) })
        as jboolean
}

/// Extends the selection from its anchor to viewport cell (`x`, `y`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_selectTo<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    x: jint,
    y: jint,
) -> jboolean {
    tab(handle).is_some_and(|tab| unsafe { appine_vt_select_to(tab.term().0, cell(x), cell(y)) })
        as jboolean
}

/// Selects everything, scrollback included.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_selectAll<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jboolean {
    tab(handle).is_some_and(|tab| unsafe { appine_vt_select_all(tab.term().0) }) as jboolean
}

/// Clears the selection.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_selectClear<'l>(
    _: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) {
    if let Some(tab) = tab(handle) {
        unsafe { appine_vt_select_clear(tab.term().0) };
    }
}

/// The selection as plain text (soft wraps joined, trailing blanks trimmed); null when there is
/// no selection or it is empty.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_copy<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jstring {
    let Some(tab) = tab(handle) else {
        return std::ptr::null_mut();
    };
    let term = tab.term();
    let mut len = 0;
    let p = unsafe { appine_vt_copy(term.0, &mut len) };
    term.text(&mut env, p, len)
}

/// The title a program set (OSC 0/2); null when none.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_title<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jstring {
    let Some(tab) = tab(handle) else {
        return std::ptr::null_mut();
    };
    let term = tab.term();
    let mut len = 0;
    let p = unsafe { appine_vt_title(term.0, &mut len) };
    term.text(&mut env, p, len)
}

/// Shows a yellow line of app notice above the cursor's line; false on the alternate screen.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_message<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
    text: JString<'l>,
) -> jboolean {
    let (Some(tab), Some(text)) = (tab(handle), string(&mut env, &text)) else {
        return 0;
    };
    (unsafe { appine_vt_message(tab.term().0, text.as_ptr(), text.len()) }) as jboolean
}

/// The primary screen's text with its scrollback (soft wraps joined, trailing blanks trimmed);
/// null on the alternate screen or on failure.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_github_mthli_appine_VtNative_transcript<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    handle: jlong,
) -> jstring {
    let Some(tab) = tab(handle) else {
        return std::ptr::null_mut();
    };
    let term = tab.term();
    let mut len = 0;
    let p = unsafe { appine_vt_transcript(term.0, &mut len) };
    term.text(&mut env, p, len)
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
