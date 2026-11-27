# android/vtcore Map

> Static understanding snapshot, not a decision history.
> See `.agents/decisions/android/vtcore.md` for the paired decision history (pre-split decisions D1, D3, D4 and the parsing side of D5–D7 stay in `.agents/decisions/android/vt.md`; earlier terminal implementation decisions D2, D5, D6, D8, D16, D17, D32, D33, D35–D39 and D42–D49 stay in `.agents/decisions/android/terminal.md`).
>
> Verified: 2026-11-26 (full refresh: 18 adversarial claims checked by independent verifier — 13 confirmed, 5 partial corrected below, 0 refuted; confirmed bugs and debts recorded)
>
> Maintained: 2026-11-27 (targeted verification only since the full mapping; latest change: freeze render state on mode 2026 hold, collapse vt-pump console reads via `read_console_run_state` with cached foreground lookup, and bound worker-side `appine_vt_transcript`)

## Responsibilities

The native per-tab terminal owns handles/locks, console pumping, protocol effects, shell marks
and timing, terminal options, PNG decoding, messages, save/restore, clear/reset, compression
and image trimming. It spans `android/app/src/main/java/io/github/mthli/appine/VtNative.kt`,
all JNI in `hosts/android/src/vt.rs`, and `hosts/android/src/vt.c` with three exclusions:

- `android/vtrender`: frame-format specification and Frames.
- `android/vtview`: Input, Paste, `appine_vt_focus` in Mouse and focus, and Search.
- `android/vtgestures`: `appine_vt_mouse` in Mouse and focus, Selection, Links and Taps on the prompt.

Kotlin session/login lifecycle belongs to `android/vt`, service policy to `android/service`
and the activity to `android/terminal`.

## Key types

| Owner | State |
| --- | --- |
| `TABS`, `Tab` | Mutex-protected `BTreeMap<jlong, Arc<Tab>>`; each tab stores its ID, kernel handle, console number, separate terminal/render mutexes and atomic frame-notification flag `dirty`. |
| `Term`, `TermGuard` | Opaque `AppineVt` pointer and optional Rust fallback title. `Tab::term()` returns a guard that checks for compression work on drop. |
| `AppineVt` | Ghostty terminal, geometry, encoders/events, reply/text buffers, search scratch, tracked gesture/selection/output references, hold/password/resize state, OSC/DCS scanners, clipboard and other effects, prompt/timing, compression and save flags. |
| `Render`, `AppineVtRender` | Separate Ghostty render state, row/cell/image iterators, frame/extras/grapheme buffers, current/sent search marks, placement list with generation/validity and the locked header sample `FrameSnap`. |
| `AppineVtEffects`, `RawEffects`, `Effects` | C structure and its Rust C-layout mirror borrow payload buffers. `Term::take_effects` copies them into owned Rust values, including ordered `StatusReport` entries. |
| `PromptState`, `PumpRead`, `FallbackTracker` | Locked prompt sample; data or timeout with effects, title and newline flags; pump-local PGID/timing, explicit-title flag and probe index. |
| `COMPRESS`, `Due` | Handle-keyed schedule with next step and original scheduling times, served by one lazy worker and condition variable. |

## Public entry points

### JNI surface and callers

All 49 `@JvmStatic external` declarations have matching `Java_io_github_mthli_appine_VtNative_*`
exports.

| Caller or role | Functions |
| --- | --- |
| Session lifetime and app output | `create`, `restore`, `setCursor`, `print`, `pump`, `destroy` |
| Terminal state and save/trim | `title`, `atPrompt`, `busy`, `clear`, `reset`, `message`, `transcript`, `snapshot`, `version`, `compressAll`, `dropImages` |
| Display and input | `frame`, `image`, `passwordInput`, `resize`, `key`, `paste`, `mouse`, `focus`, `scroll`, `jumpPrompt` |
| Touch and pointer selection | `gesturePress`, `gestureDrag`, `gestureEnd`, `mousePress`, `mouseDrag`, `mouseRelease`, `selectContext`, `selectGrab`, `selectTo`, `selectAdjust`, `selectRectangle`, `selectAll` |
| Output, links and search | `outputAt`, `selectOutput`, `selectClear`, `copy`, `link`, `promptClick`, `search`, `searchSelect`, `searchRows`, `searchEnd` |

`pump` runs on `VtSession`'s `vt-pump`. Production session controls, input,
frames and bitmap reads run on main. The service worker calls `transcript` (any-thread), `snapshot`
(any-thread), `dropImages` and `compressAll`. `Tabs` reads `version` lazily for its index.
Instrumentation uses `atPrompt`; production close checks use `busy`.

### Constants and JNI values

The 15 integer constants are `CURSOR_BAR=0`, `CURSOR_BLOCK=1`, `CURSOR_UNDERLINE=2`,
`CURSOR_HOLLOW=3`, `CURSOR_LOCK=4`; `GESTURE_WORD=1`, `GESTURE_LINE=2`; and
`ADJUST_LEFT=0`, `ADJUST_RIGHT=1`, `ADJUST_UP=2`, `ADJUST_DOWN=3`, `ADJUST_HOME=4`,
`ADJUST_END=5`, `ADJUST_PAGE_UP=6`, `ADJUST_PAGE_DOWN=7`. Appine's settings offer bar, block
and underline; frames can also report hollow or Appine's password lock. Ghostty accepts hollow
as a cursor default through JNI, whereas lock is an Appine-only frame value. The other cursor
styles, gestures and adjustments use Ghostty enum values.

For paste, null can request consent and empty means encoding failed. For prompt taps, null
means inapplicable and empty means no movement. Key/mouse/focus use 128/64/16-byte buffers.
Empty mouse/focus can mean no report. Search rows begin with the selected row or −1, then
distinct match rows newest first, copied before unlock.

## Data flow / lifecycle

### Opening and releasing a tab

`VtNative` initializes `Native` to load the shared library. `ConsolePty` opens the console
and spawns the login script. Both constructors check the kernel handle, clamp grid dimensions
to 1–65,535 and cell pixel dimensions to at least 1. Stopped kernels raise an I/O exception.
`create` throws on allocation failure; `restore` returns 0 on conversion/decode/allocation
failure. `register` owns the terminal before allocating render state, so failure frees it.
IDs increase atomically from 1 without a free list. A new tab starts clean.

Calls clone the tab's `Arc` under the table lock. Kernel operations look up the handle for a
temporary `Arc<Machine>`; holding one in the tab would pin the previous store across reboot,
tier or distro changes. The service releases sessions before shutdown. Closing the console
wakes a blocked read. `destroy` removes the handle, then drops its reference outside the table
lock. In-flight calls, including the pump, keep both C objects alive until the last owner drops.

### Terminal options and system callbacks

`appine_vt_new` creates a Ghostty terminal; restore decodes and settles one. Both use `wrap`:
allocate encoders/events, run `pthread_once(&sys_once, install_sys)`, resize to supply pixel
dimensions, install userdata, callbacks and options. If the first terminal is restored, decode and
settle precede system log installation. Kotlin applies the user's cursor before pending messages
and pump startup. Transcript fallback replay inside `createTerminal` precedes that cursor call.

| Setting | Contract on create and restore |
| --- | --- |
| Scrollback | 10,000 lines or 10 MiB, pruning oldest whole pages (~400 KiB). The byte cap dominates above roughly 120 columns. A page holds ~1,000/580/190 rows at 45/80/250 columns. The C default is only 10,000 bytes, raised to fit active pages plus one. |
| Colours/cursor | White foreground, black background enable OSC 10/11 queries, reverse video and rendered colour overrides. OSC 12 defaults to foreground. Enable blinking over Ghostty's default block. `setCursor` changes defaults immediately unless a program chose its style. CSI 0 q, DECSTR and RIS bring defaults back. |
| Grid/modes | Enable mode 2027 (grapheme clustering) now and as default. Emoji widths may disagree with per-codepoint `wcwidth`; programs can disable it. All new render states request one overscan row below, zero above. |
| Device replies | Stored grid/cell size, dark colour scheme, DA1 VT220 with 132 columns/selective erase/ANSI colour/clipboard 52, DA2 type 1/firmware 1/ROM 0, DA3 unit 0, XTVERSION `appine`. XTGETTCAP's `TN` matches login `TERM=xterm-256color`; other capabilities use Ghostty's built-in entry. |
| Graphics | 32 MiB decoded Kitty data per screen (Ghostty app default 320 MB). Direct data only: file/shared-memory disabled, temp-file handler null; these host media answer `EINVAL: unsupported medium`. Alternate inherits the policy; RIS retains it. Glyph Protocol off: the C API cannot retrieve registered glyphs to draw. |
| Parser/effects | 64 KiB continuation tracking for snapshots, 1 MiB aggregate clipboard write, 16 bytes unknown OSC/APC. Hooks cover PTY replies, size/colour/DA/version, hold, title, bell, clipboard write, notification, progress, status, semantic prompt, reset and unknown sequences. |

`install_sys` registers Wuffs PNG decoding and logging via `ghostty_sys_set`. Kitty PNG
(`f=100`) fails with `EINVAL` without a decoder. `decode_png` rejects zero dimensions or
over 32 MiB of RGBA before allocating pixels via Ghostty's allocator. Wuffs returns straight
alpha; bitmap delivery uses premultiplied pixels. Error paths release allocated pixels and scratch.

Library warnings/errors go to stderr/logcat, including unimplemented CSI/ESC. The unknown-string
hook logs leading decimal OSC digits, the first alphabetic APC byte or “(unnamed)”, limiting
payload exposure for malformed strings. Both hooks share 20 lines per monotonic second across
tabs/threads, with a possible race at the second boundary.

### Console output and JNI delivery

1. `vt-pump` reads one run of foreground or background output into a reusable 64 KiB buffer with
   `Machine::read_console_run_state`, which samples `password_input` (ICANON on, ECHO off),
   `fg_pgrp`, and a lightweight `ConsoleForegroundKey` under the same single BKL acquisition. The
   view still polls `passwordInput` while showing the lock cursor in case echo returns without
   output.
2. Under one terminal guard it sets the password flag, feeds bytes, takes replies, title-change
   flag, effects and prompt state. `ghostty_terminal_vt_write_until` stops at OSC/DCS starts
   and ends. Scanners retain 64/5 prefix bytes across reads and count the entire content,
   excluding its terminator. Overlong OSC skips manual recognition. On a boundary-API error,
   bulk-write the remainder and clear OSC/DCS/tmux tracking. Armed ash LFs are handled below.
3. After unlock `Tab::write` loops over partial writes until all replies (e.g. DA, DSR, size,
   tmux CR) reach the console or a write fails. Paste and prompt-click adapters drain the same C
   buffer for Kotlin. A timeout samples prompt state without feeding. After data or timeout,
   `console_foreground_process` is skipped when `at_prompt` or `PROMPT_FULL` (using `fg_pgrp` for
   report expiry), reused from a pump-local cache while `ConsoleForegroundKey` matches, and
   queried fresh only on key changes or probe timeouts; the pump then drops the machine and updates
   fallback.
4. Only a data run swaps `dirty` to true. Only the false→true transition calls `onOutput`,
   posting a frame invalidation if a view is attached. Hidden tabs keep pumping. Then come
   `onTitle` for explicit/fallback changes, batch effects, fallback completion and finally
   `onForeground`. Timeouts cannot dirty a frame.
5. Missing handle/kernel, EOF, console read/write failure, failed JNI delivery or guarded panic
   ends the pump. A timeout keeps it running. JNI string refs for clipboard, notification and
   each status are deleted after their calls. Failed delivery short-circuits later callbacks
   and clears a pending Java exception before exit.

### Effects and clipboard generations

`appine_vt_take_effects` captures a batch under the lock and drains pending counts, flags
and queue length. Rust owns all payloads before calling Java in this order:

| Callback | Contents per read |
| --- | --- |
| `onBell` | One for any number of BELs. |
| `onClipboard` | Last accepted OSC 52/1337/5522 write to any location. First MIME type beginning `text/plain`, up to 256 KiB. Zero representations clears. Unsupported MIME, oversized text or copy failure returns unsupported, denied or I/O error; only OSC 5522 exposes those replies. Its aggregate buffered write fails above 1 MiB with EFBIG. |
| `onNotification` | Last OSC 9 body or OSC 777 title/body. Each string capped at 4 KiB, possibly splitting UTF-8; Rust decodes lossily. |
| `onProgress`, `onStatus` | Last OSC 9;4 state/percentage (−1 if absent); every OSC 7501 status in order. Ghostty validates/decodes statuses and answers the protocol's capability query with the callback installed. |
| `onClipboardReset`, `onClipboardRead` | Reset indication first, then kind/count for all subsequent reads, both using the same captured generation. |
| `onCommandFinished`, `onTmuxDetached` | Last timed command end with optional exit code, then normal end of a recognized tmux control string. |

The seven-byte status header carries state, kind, signed progress, byte-sized ID/title lengths
and little-endian 16-bit message length. Then come the strings (protocol maxima 128/192/2,048
bytes). Rust stops at a truncated entry. State 255 with zero lengths expires transient records
in queue order.

`osc_ended` recognizes a complete, uncancelled `52;<locations>;?` within the prefix limit.
Locations may be empty or any sequence of `c,p,q,s,0–7` without separators. All reads use
the first location of the last accepted query: `p` or `s`, otherwise `c`. RIS increments
`clipboard_generation`, discards earlier reads and sets `clipboard_reset`. Taking effects
consumes the flag and count but preserves the generation. Kotlin observes resets on the pump
thread before posting cleanup, rejects stale reads on enqueue and checks reply tokens on the
writer. A write past that final check can complete across reset. Ghostty's synchronous read
callback stays unset because user approval must happen outside the terminal lock.

`print` feeds app bytes as foreground and drains replies and effects. A RIS still advances the
generation for subsequent reads, and `title_changed` remains for the next pump data run.
Printing a tmux header therefore cannot send the detach reply.

### Tmux and report expiry

The first write chunk completing a DCS prefix of exactly `1000p` arms `tmux_control` and
appends one CR if reply storage is available, even if the string ends in the same read.
`tmux -CC` accepts the empty command as detach, leaving the session running. Only an ST end
sets `tmux_detached`. C accepts ST_ESC/ST_C1 end reasons, but after this header 0x9C is
payload; the parser reports normal ST at its ESC. CAN/SUB or app cancellation suppress the
notice. Other DCS strings (DECRQSS, XTGETTCAP, tmux passthrough) cost two stops. The service
shows a yellow `[appine]` notice about `tmux attach` unless on alternate screen. No retry.

Progress/status sets `reported_since_prompt`. At command end or a primary-kind semantic
prompt start, `expire_reports` sets progress to removal and attempts to append transient
expiry, even with no timer; if allocation fails, expiry remains pending for a later boundary.
Later reports in that read replace progress or follow the marker. On data
with progress/queued statuses the pump records the sampled foreground PGID. A later report-free
**data** read that sees a different or missing group fires `onForeground` once. Timeouts do
not compare the reporter. Kotlin expires progress and working/blocked/idle statuses; done/error
survive until seen. Shell exit or explicit reset clears all records.

### Shell marks and line feeds

OSC 133 assigns prompt/input/output semantics to rows and cells. Bash's PS0 and zsh's preexec
mark output start. The setup hooks cover multiline continuations and nested shells, bootstrap
Debian login zsh via `/etc/appine/zsh/.zshenv` and nested zsh via `/etc/zsh/zshrc`, and wrap
RPROMPT/RPS2 to exclude right prompts from input. BusyBox ash marks prompts/input and delegates
line-feed recognition. Packaged Alpine `doas -s` preserves its `ENV` hook. Fish 4 and nushell
can supply all four marks themselves. Fish requests click events; nushell need not declare a
tap action. Marks support prompt jumps, command-output selection (including Ctrl+triple-click
and the context menu), taps and strict prompt detection.

On the primary screen the manual A/P scanner reads the first pair for each option key.
On A, `click_events=1` requests absolute mouse coordinates and 2 prompt-relative. Otherwise,
`cl` values `line`, `m`, `v` and `w` request arrows. Unrecognized or missing options retain the previous click
mode. A or P with `appine_lf=output` selects ASH and arms LF handling. A without it selects
FULL and disarms; P without it preserves the setting. Reset clears both modes and the resumed row.

- While armed outside OSC/DCS, a foreground LF after ≥2 CRs means ash broke a long line (CR LF
  plus ONLCR). The next row continues the input. Count up to two trailing foreground CRs across
  reads, ignoring other writers. Any other LF disarms; if at a prompt, write LF then synthetic
  OSC 133 C to mark the row as output. A completion list or Ctrl-C echo can also take this path.
- A nonempty background run starting at an armed prompt outside open OSC/DCS gets a tracked
  cursor reference and C before the bytes. If an OSC/DCS remains open, skip resumption.
  Otherwise compare page node and row: same row gets B alone; a different/indeterminate row
  gets P with `k=s` then B, tracking column zero there as `lf_resumed` if possible. Typed
  text is input again; while the resumed row is trackable and the cursor is at or below it,
  taps above that row are rejected, and that row is excluded from ash's line-break count.
  Rearming, a new primary prompt, an ending LF, real C/D or reset can drop it.

### Timing, busy state and fallback titles

A real C sets FULL and starts/restarts monotonic timing. `lf_marking` suppresses this for
injected C. At **feed end**, a transition from `was_at_prompt` to away starts timing if no
timer exists. D expires reports, drops the resumed row and records elapsed whole milliseconds
and optional exit code only with a start time. A primary-kind prompt-start event (A/N/P
according to `k`, primary by default) expires reports and abandons any timer. A redraw can
therefore produce D without completion. Only the last marked completion per read is delivered.
Resize, clear, reset and message do not update `was_at_prompt`. Old edited ash profiles using
OSC 133 I without the LF option can mistake background output for departure from a prompt.

`atPrompt` queries Ghostty for a cursor in a marked prompt or its input on primary. For a
live tab `busy` returns false there, then true on alternate or in FULL away from a prompt.
In NONE/ASH it releases the guard before querying the kernel. An interactive foreground shell
on primary counts idle in NONE; ASH also requires noncanonical input. Ash's raw line editor
can recover idle with overwritten PS1 while a canonical builtin like `read` remains busy.
Missing sample means busy, missing tab means false. Kotlin also requires a running session.
Background jobs alone do not make a marked prompt busy.

`FallbackTracker` clears tracking and title immediately at a marked prompt or in FULL.
Otherwise it times a sampled non-shell PGID from discovery. Without an explicit OSC 0/2 change
for this command it uses the trimmed cmdline (`comm` if absent/multiline), skips login wrappers
and empty text, replaces controls with `?` and caps at 256 UTF-8 bytes on a character boundary.
`title()` prefers the stored fallback; an explicit change suppresses it, revealing Ghostty's
title. Changes to either source can notify `onTitle`.

A foreground run containing LF while the shell still holds foreground can arm **successive**
15/35/75 ms waits to catch fork/handoff/exec, including silent commands. While a tracked
command returns to a still-canonical ash shell, any new output can rearm; the final timeout
there clears stale tracking/title. Otherwise idle reads block indefinitely once probes run
out. A sampled transition back to an idle interactive shell away from a marked prompt delivers
`onCommandFinished(false, 0, millis)` unless the batch already contains a marked completion.
A return directly to a marked prompt just clears the tracker. The notification threshold is
10 s for a tab outside the foreground.

### Resize, frame capture and synchronized output

`resize` updates the emulator and drains replies under the guard, then calls `resize_console`
(SIGWINCH). Kotlin writes any mode 2048 size report immediately. A restarted search catches up
and re-selects the old index from newest, scrolling only on the last step. A resize at a marked
prompt can erase prompt rows for shell redraw (only the last with bash's `redraw=last`).
Shells that fail to redraw can lose visible input until the next prompt.

Only a changed grid sets `resize_pending` and zeros `resize_output_at`. Each nonempty feed
while pending refreshes the timestamp. Frame begin clears pending only after such a feed,
≥32 ms quiet, successful `VT_GROUND` (including UTF-8 state) and a clear host hold flag. Frame bit 7 is pending **or** held. The height-shrink preview has its own 200 ms bound
even if output never comes. Later redraws can follow a quiet gap; resize and SIGWINCH proceed
without waiting.

`frame` clears `dirty` before capture. Under render then terminal lock, `appine_vt_frame_begin`
samples state, images, animation, search and header fields. `appine_vt_frame_end` finishes
deferred style work and encodes rows under render alone. JNI releases render before wrapping
the direct buffer; main must consume it before the next frame on the live tab. Mode 2026's
`on_render_hold` only records `held` and `held_since` without touching `GhosttyRenderState`, so
terminal dirty flags stay intact; while held, `appine_vt_frame_begin` freezes `render->state` in
place (suppressing row encoding via `skip_rows` only if `render->state` dimensions do not yet match
the terminal) so the first frame after `CSI ? 2026 l` updates incrementally. Begin forces a hold
older than one second off.

### App messages, clear and reset

`cancel_sequence` writes CAN if a successful `VT_GROUND` query finds incomplete input,
then clears `osc_open`, `dcs_open` and `tmux_control` unconditionally. This prevents an open
OSC/DCS/APC swallowing app text or reporting detach. Partial UTF-8 can leave U+FFFD.
`write_own` cancels first, preserves the previous reply length across its write and invalidates
both saves. Explicit reset also cancels because Ghostty's direct reset preserves its parser.

| Action | Screen and caller contract |
| --- | --- |
| `message` | The session can queue a kill report before startup. On primary, compute rows from per-codepoint display width and columns (minimum one, maximum height−1 on multirow grids). Write LFs for room, move up, save cursor, CR, insert lines, write yellow text, reset attributes, restore cursor and move down. Prompt and half-typed input move with it, though a one-row grid or restrictive scrolling margins (where Ghostty ignores `IL`) can prevent that placement even when the call returns true. The program's saved cursor is overwritten. Alternate returns false before cancellation or writing. |
| `clear` | −1 on alternate or missing tab. On primary clear selection. At a marked prompt ED 2 then ED 3 removes display and history, including rows ED 2 can push into history; return 1 for Kotlin to send form feed. Else ED 3, then DL from the top for rows above the cursor, restoring its column on the top row. Quiet Kitty deletion clears screen placements and images with no remaining placement. Return 0. Both primary paths scroll to bottom. |
| `reset` | On either screen sample prompt, clear selection, cancel and call `ghostty_terminal_reset`. Reset screens, history, modes, palette, title and images; retain app options and OSC 10/11/12 dynamic colours. Drop hold state, prompt/LF tracking and Rust fallback title. Increment generation directly, zero buffered reads and clear the pending reset flag. |
| Reset result | JNI returns zero for a missing tab. Otherwise mask generation to 63 bits, minimum magnitude 1; positive iff previously at prompt. Kotlin invalidates local clipboard work before waiting, adopts the absolute result, sends form feed only if positive, clears progress/status and signals title change. Stream RIS uses progress removal and status clear callbacks and increments generation in `on_reset`. |

### Scrollback compression and image trimming

Cold primary pages lie wholly in history outside the viewport. Alternate has no scrollback.
Earlier host measurements put compressed size near one fifth of a ~400 KiB page; history limits
charge the uncompressed size. Drawing a scrolled viewport and formatting text can restore
pages. Search preserves compressed storage. Android decommits page-allocator mappings using
`MADV_DONTNEED`; 4 KiB minimum alignment can meet a 16 KiB kernel that rounds the advised
length up. Separate page mappings matter.

`compression_activity` tracks history growth, resize and scroll, but reads can restore pages
without changing it. Set `compress_touched` for prompt jumps/taps, select-all, output lookup
and selection (including Ctrl+triple-click), selection adjustment, copy, text-link lookup,
transcript and snapshot. The snapshot mark is conservative: pinned Ghostty decodes temporary
copies of cold pages without changing their storage.

On guard drop `appine_vt_compress_wanted` compares activity/touched, marking work scheduled
once per tab. `compress_later` queues the ID 250 ms out. The `vt-compress` worker removes the earliest due
entry and releases `COMPRESS` before tab lookup and raw terminal locking. Changed activity
can defer another 250 ms while age is <5 s. After that age it cannot postpone a step. Pending
incremental work requests the next step 1 ms later, releasing the terminal between steps.
Neither interval bounds total runtime. Completion or error clears scheduled/touched and frees
the entire text buffer if capacity >64 KiB. Requeue preserves the original scheduling time.
A closed handle drops out at lookup.

`compressAll` clones tabs under `TABS`, then fully compresses each under its own mutex and
releases oversized text buffers. An existing scheduled pass survives; otherwise it updates the
token and clears touched. `dropImages` lowers each screen's Kitty limit to 4 MiB then restores
32 MiB. Eviction takes unplaced before placed, transient before durable within each class,
then oldest transmission. Placements go with evicted images, even visible/history ones.
The saved snapshot may retain evicted images; trimming alone avoids a new full history traversal.

The service trims background images after 60 s without UI or on memory pressure. While
switching it keeps at most two recently left tabs for 60 s, trims older tabs sooner and
re-trims previously trimmed unseen tabs at periodic saves. Current is excluded from image
trim but included in full scrollback compression. The policy also serves Android 14+ where
foreground running-low callbacks are absent.

### Transcripts, snapshots and restore

`changed` invalidates both `transcribed` and `snapshot` on feed, resize, own writes and
explicit reset, independently of frame `dirty`. `transcript` formats the primary screen's trailing
plain text (capped to `TRANSCRIPT_MAX_UTF16` = 256 Ki UTF-16 code units in C before JNI, trying a
tail selection first on deep scrollback so older cold pages remain compressed when the tail fills
the cap, and falling back to full history for sparse lines) with soft wraps joined and trailing
blanks trimmed. Success on an empty screen returns an empty string. Alternate, unchanged or failed
returns null. Kotlin caches the result on the service worker. Skipping formatting avoids making
compressed pages resident again.

Binary snapshots capture both screens, history, images, colours, modes, title and parser
continuation. From `SNAPSHOT_STALE`, mark touched and set `NONE` before encoding. Success
at **≤16 MiB** sets `TAKEN`. Rust copies the owned allocation to Java after unlocking and
frees it. Subsequent unchanged calls return null (keep previous file), as do missing tabs.
Failure, including an unfinished sequence beyond the **64 KiB** continuation cap, or encoded
size >16 MiB returns empty (drop saved reference). `NONE` returns empty on subsequent saves
without retry until invalidation.

`Tabs` saves content-addressed, deflated bytes and gates loading on the library version.
A historical full coloured history sample on Mac was ~1.5 MB, ~1 ms to encode, ~3 ms to
decode. The snapshot format has no cross-version ABI promise. Kotlin limits inflation to
16 MiB. Native restore enables compressed history and sets a 32 MiB per-screen image limit
**on the decoder before decode**, then frees it. On native failure Kotlin logs and can
create a fresh terminal replaying its saved transcript.

`settle` prepares decoded state for a new shell with no per-terminal callbacks installed.
Writes produce no app effects or console replies. In order:

1. Prepend CAN if off ground and DECRST 1049 if on alternate. Issue DECSTR for cursor,
   margins, attributes, cursor/keypad modes and palette.
2. Reset private modes `5,9,1000,1002,1003,1004,1005,1006,1015,1016,2004,2026,2031,2048,5522`
   (reverse video, mouse tracking/formats, focus, bracketed paste, sync output, colour/size
   reports, Kitty paste). Omit unsupported 1001. Set alternate scroll 1007, pop Kitty keyboard
   flags (CSI <99u), close OSC 8, empty OSC 0 title, reset OSC 22 pointer to text.
   A full reset alone keeps the pointer.
3. Write OSC 133 C separately to mark what follows as output, protecting restored prompt rows
   from resize. Query cursor X; CR LF only if X > 0, then ED 0. Clear selection and scroll to bottom.
4. `wrap` resizes and reapplies creation options/callbacks. Kotlin applies the user's cursor
   defaults. History survives for the new shell.

## Dependencies (inbound / outbound)

### Inbound

- `VtSession`/`ConsolePty` supply the console, manage terminal lifetime, cache transcript text
  and send form feeds, resize replies and asynchronous clipboard answers.
- `TerminalService` schedules save/trim; `Tabs` owns files and version checks.
- `VtView` calls JNI for frames, input, password, search and gestures. `VtImages` requests a
  frame's ID/generation. JNI locks RGBA_8888 bitmap pixels and passes dimensions and stride
  to the copy under `Tab.term`.

### Outbound

- `android/native` supplies `machine`, JNI string/exception helpers and `guarded!` panic
  containment. `kernel/tty` provides console I/O, resize, writer classification, password and
  foreground-process queries. `android/setup` supplies guest prompt hooks.
- `android/build` pins and links libghostty-vt 1.0.0 with Wuffs into `libappine.so`. JNI
  uses NDK `libjnigraphics`. Native rendering/interaction use the sibling-owned C sections.
- `VtSession.on*` runs on the pump thread. Kotlin posts UI/service work to main, coalesces
  reports, handles clipboard approval and checks session lifetime. Service policy routes effects,
  including `VtView.bell` for the foreground tab.

## Invariants and gotchas

- Nested lock order is `Tab.render` → `Tab.term` → `COMPRESS`. `TermGuard::drop` tests and
  enqueues work **before** its terminal mutex unlocks. The worker and `compressAll` lock
  directly to avoid rescheduling themselves. Poisoned locks/condition-variable guards recover
  with `into_inner`. Release the terminal lock before kernel locking or session callbacks.
- Keep `AppineVtEffects`/`RawEffects` field order, widths, pointer/length pairs, booleans and
  padding in sync. The status wire header and frame format are distinct contracts. JNI copies
  title/copy/link/transcript under the terminal guard before text can be reused or freed.
  Pump and reset must agree on clipboard generation conversion.
- Ghostty get/set keys use untyped pointers. Recheck each C value type on upgrades;
  `GHOSTTY_TERMINAL_OPT_USERDATA` takes the pointer itself. Reinstall non-serialized options
  and callbacks after decode.
- Keep writes bypassing `appine_vt_feed` consistent with its OSC/DCS/tmux scanners. New
  text-changing and history-reading paths need the appropriate save/compression flags.
- ED 0 leaves images. ED 2 removes placements intersecting the active screen; ED 3 removes
  those in erased history. Trim can evict a visible image while text and the saved file survive.

## Confirmed bugs / technical debt

- Unmarked shells (dash, mksh, tcsh, elvish or ash with overwritten PS1) lack prompt jumps, output
  selection and tap targets. Sampling can miss short commands and cannot recover exit codes
  or a remote shell's boundaries behind ssh without marks.
- ENQ, CSI 21 t title report, CSI 8 t window resize and DECRQCRA checksum request go
  unanswered, matching Ghostty defaults. XTCHECKSUM selects a calculation and is ignored
  with reporting off. OSC 5522 reads are refused and Kitty paste events unavailable. Wiring
  the synchronous read API would also require classifying multiline IME commits as text
  instead of clipboard-source pastes.
- Unused library surfaces include VT/HTML formatting with colours/OSC 8 links, memory usage
  and processing-error queries, OSC 133 command/error text, KAM (mode 2), standalone parsers
  and encoders, WASM/JSON helpers, X11 colour names, stepped snapshot decode, macOS Option-as-Alt
  and manual title/directory setters. Local cwd comes from the kernel. OSC 7 hooks, resize
  pulling scrollback rows, APC limits and the `getrandom` callback remain unset.
- The embedding lacks visibility control (mode 2033 reports all tabs visible), the native
  password flag, Kitty drag-and-drop (OSC 72), OSC 66/99/hierarchical contexts and full
  tmux control-mode integration. Without Oniguruma it can detach but cannot show tmux windows/tabs.
- If the first compressor spawn fails, the error is logged and `Once` completes. Queued idle
  passes then have no worker; `compressAll` remains available.
- A successful encode marks the snapshot taken before JNI allocates the Java `byte[]` or
  `Tabs.writeSnapshot` writes the file. If array allocation or writing fails, unchanged saves
  cannot retry until another terminal change, so restore falls back to the transcript.
- Both `on_reset` (RIS) and `appine_vt_reset` clear prompt tracking without clearing
  `command_start`, and explicit reset also leaves `was_at_prompt` intact; a later `D` can
  report duration spanning a reset, and the first feed after explicit reset can infer
  departure from a pre-reset prompt.
- `tmux_started` arms `tmux_control` before reserving the CR reply, so a reply-buffer
  reservation failure can leave detach tracking armed without queuing CR.
- `appine_vt_message` returns true on primary even on a one-row grid or when the cursor lies
  outside scrolling margins (where Ghostty ignores `IL`).
- Ash's background-output wrapper checks only `osc_open` and `dcs_open`, so unfinished CSI,
  APC or UTF-8 sequences can be interrupted by synthetic prompt marks.

## Open questions

None currently.

## To verify

- Historical coverage includes a Pixel 4a API 34 emulator (4 KiB pages), prompt expiry on
  an API 37 emulator (16 KiB pages) and macOS C harnesses. Exercise interactive zsh, Debian
  bash and the marks fish 4.6/nushell 0.104 emit in the app; host/container runs and synthetic
  marks exercise different paths. Check real sudo, passwd, ssh and doas password prompts.
- Check tmux detach beyond versions 3.5a/3.7c and application fallback after Glyph Protocol probes.
- Measure reclamation and battery cost on physical 16 KiB-page devices. Exercise restore with
  images, bash marks and a saved alternate screen, plus transcript fallback across a library upgrade.
