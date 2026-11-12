# android/vt Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/vt.md` for the paired decision history (decisions from before the split are D2, D5, D6, D8, D16, D17, D32, D33, D35–D39 and D42–D49 in `.claude/decisions/android/terminal.md`).
>
> Verified: 2026-11-11 (split out of the android/terminal full mapping of the same day: 5 research concerns, 18 claims checked by 3 independent verifiers; the boundary was checked against the source at the split)
>
> Maintained: 2026-11-11 (targeted verification only since the split: the transcript kept for saves, then images cut under memory pressure last)

## Responsibilities

`android/vt` is the tabs' terminal sessions: libghostty-vt terminals in the native library, the
sessions that feed them from the kernel's consoles, and what programs ask of the app, handed to
the service:

- `VtSession` (a tab: its shell, the terminal, pump, waiter and writer threads, what its programs
  report about their progress and status, whether its shell waits at its prompt and in which
  directory, its terminal's snapshots and coming back from one, clipboard reads waiting for an
  answer, the commands its shell's prompt marks show finishing, clearing and resetting, the
  cursor's default from Settings, tmux's control mode ending) and `ConsolePty` (the tab's kernel
  console, `/dev/pts/N`, and login shell);
- `VtNative` and the native library's JNI (`vt.rs`) and emulator shim (`vt.c`) around Ghostty's
  libghostty-vt, with the `vt-compress` thread that compresses idle tabs' scrollback.

Root: `VtSession.kt`, `ConsolePty.kt` and `VtNative.kt` in
`android/app/src/main/java/io/github/mthli/appine/`, `hosts/android/src/vt.rs`, and
`hosts/android/src/vt.c` but its Frames, Input, Paste, Mouse and focus, Selection, Links, Taps on
the prompt and Search sections.

Outside the module: drawing, input, selection, search and the view's uses of the prompt marks,
with those sections of vt.c (`android/vtview`); the screen around the view (`android/terminal`);
session ownership, saving, kill reports and where bells, notifications, program statuses,
clipboard writes and clipboard reads go (`android/service`); the host console in the kernel
(`kernel/tty`); the JNI crate, `guarded!` and the build, including compiling libghostty-vt with Zig
(`android/native`); the login shells' prompt marks (`android/setup`).

## Key types

- `VtSession` — a tab owned by the service: its `id` (from the service, naming the tab in a
  notification's intent), its `ConsolePty`, the terminal handle (0 until the shell starts, never
  reset), the view showing it (`view`, read on the pump thread), a restored tab's snapshot (a
  loader the service gives) and transcript, the pending notice and messages, its login and the
  account an `auto` login resolved to (`resolvedLogin`, written by the service), the file of its
  last saved snapshot (`savedSnapshot`, used by the service's worker only), the last transcript
  (its last 256 K characters, also returned while the screen is unchanged), the last progress
  report (`progress`, a
  `Progress`), the program status records by id (`Status`, least recently updated first, at most
  64) and the one most in need of the user (`status`), the clipboard reads waiting for an answer
  (the clipboard each named), a `vt-pump` thread, a `vt-waiter` thread and its own single-thread
  `vt-writer` executor, the cursor's default shape and blinking (`setCursor`, from the service);
  `Events` (title changed, bell, clipboard write, notification, clipboard read, progress or status
  changed, a command finished, tmux's control mode detached, the shell finished) go
  to the service on the main thread; `busy` (closing it would end something: the shell runs, has
  started and is not at its prompt) and `cwd()` (its foreground group's directory, from the
  kernel). Its companion holds
  libghostty-vt's progress states, status states and kinds, and `describe` (a status in words).
- `VtNative` — 48 `@JvmStatic external` functions on the native library, and libghostty-vt's
  cursor styles (`CURSOR_BAR`, `CURSOR_BLOCK`, `CURSOR_UNDERLINE`, and for frames `CURSOR_HOLLOW`
  and vt.c's `CURSOR_LOCK`), selection gesture behaviours
  (`GESTURE_WORD`, `GESTURE_LINE`) and selection adjustments (`ADJUST_*`), passed to vt.c as raw
  values, so they must match libghostty-vt's headers.
- `ConsolePty` — `start` (open a console, spawn `login.sh` through `sh -c` with the tab's login
  and cwd and a fixed environment, `TERM=xterm-256color` among it), `waitFor`, `close`; `console`
  is −1 until started and keeps its number after `close` (`VtSession.consoleOpen` tells whether it
  is still open, for the routing of kill reports in android/service).
- In `vt.rs`: `Tab` (its own handle, the kernel's handle, console number, the terminal under a
  mutex, a `dirty` flag) in a table `TABS` keyed by handles from 1, never reused; `TermGuard` (a
  tab's locked terminal, which schedules a compression pass when it is released); `Term` (owns the
  `AppineVt`, freed on drop); `RawEffects`, laid out as vt.c's `AppineVtEffects`, turned into
  `Effects` (with the `StatusReport`s decoded from vt.c's queue, the clipboard reads, the last
  command finished: its exit code when reported and how long it ran, and whether tmux's control
  mode, which vt.c detached, ended); the
  compression schedule `COMPRESS` (a `Due` per tab: when to look at it next and when it was
  scheduled), served by the `vt-compress` thread (`compressor`).
- In `vt.c`: `AppineVt`, one per terminal: the terminal, its render state, key and mouse encoders,
  the grid and cell pixel sizes; reply, frame and scratch buffers and one text buffer shared by
  title, copy, link and transcript; the title-changed flag, the synchronized-output hold and
  `password_input` (the cursor then a lock); the selection gesture, shared by touch and mouse (with
  reusable events: an untimed press and a drag for a finger, a timed press and a release for a
  mouse), and the tracked anchor of a handle's drag; the last reported mouse cell; the effects
  since they were last taken (bells, the last clipboard write, notification and progress report,
  every program status report queued in a byte layout vt.rs reads, the clipboard reads with the
  clipboard last named, the last command finished, `tmux_detached`), handed out as
  `AppineVtEffects`; command timing (`command_start`, `was_at_prompt`); the OSC string the output
  is in (its first 64 bytes and length) and the DCS string (first 5 bytes, header, length, and
  `tmux_control`); `prompt_click` (how a tap on the input moves the shell's cursor, as the primary
  screen's last prompt start declared); the search (a `GhosttySearch`, scratch for the viewport's
  and all matches and their rows, each viewport cell's mark this frame and as last sent); the cell
  of an output a long press found, tracked for Select output; the compression state (a pass
  scheduled, the activity token last seen, `compress_touched`); whether the last transcript
  (`transcribed`) and snapshot are still current; and the render state's image list
  (`Placement`) with its generation. Process-wide, installed into libghostty-vt's sys interface
  with the first terminal (`install_sys`): the PNG decoder and the log callback (`on_log`), which
  with `on_unknown_sequence` shares a limit of 20 lines a second across all tabs (`log_allowed`).

## Public entry points

- `VtNative`: `create`, `restore`, `setCursor`, `print`, `pump`, `title`, `atPrompt`, `frame`,
  `image`, `passwordInput`, `resize`, `key`, `paste`, `mouse`, `focus`, `scroll`, `jumpPrompt`,
  `gesturePress`, `gestureDrag`, `gestureEnd`, `mousePress`, `mouseDrag`, `mouseRelease`,
  `selectContext`, `selectGrab`, `selectTo`, `selectAdjust`, `selectRectangle`, `selectAll`,
  `outputAt`, `selectOutput`, `selectClear`, `copy`, `link`, `promptClick`, `search`,
  `searchSelect`, `searchRows`, `searchEnd`, `clear`, `reset`, `message`, `transcript`, `snapshot`,
  `version`, `compressAll`, `dropImages`, `destroy`; `pump` runs on `vt-pump` and calls
  `VtSession.onOutput`, `onTitle`, `onBell`, `onClipboard`, `onNotification`, `onProgress`,
  `onStatus`, `onClipboardRead`, `onCommandFinished`, `onTmuxDetached` and `onForeground` back;
  `snapshot`, `compressAll` and `dropImages` run on the service's worker, `version` wherever `Tabs`
  needs it, the rest on the main thread.

## Data flow / lifecycle

### Start and end

- A `VtSession` starts its shell the first time a view showing it knows the grid (`VtView.attach`
  and every regrid call `Host.onGrid`, which starts the current tab once): `ConsolePty.start` opens
  a console of the grid's size and spawns the login; then the terminal is made: a restored tab's
  from its saved snapshot when it has one that decodes (`VtNative.restore`, see Snapshots and
  restoring), else a new one (`VtNative.create`), both with the same options (scrollback limited to
  10,000 lines or 10 MiB, whichever is reached first: the lines up to about 120 columns, the bytes
  beyond; default colours white on black and no default cursor colour, so the cursor follows the
  foreground as in Ghostty; the cursor's default from Settings, a blinking block unless changed
  (`wrap` only turns blinking on over libghostty-vt's steady block, and the session sets the user's
  choice right after, `VtNative.setCursor`, before the pending text; android/vtview's Frames and
  drawing); grapheme clustering, mode 2027, on by default as in Ghostty; the render state asks for
  one overscan row below the viewport; the OSC 133 callback (command timing) and unimplemented OSC
  and APC strings reported with 16 bytes kept, of which only an OSC's leading number or an APC's
  first letter is logged (a malformed string may start with the program's data); Kitty graphics on,
  with 32 MiB of decoded images per screen (Ghostty's default is 320 MB) and only data sent in the
  escape sequences (the file, temporary file and shared memory media, which would name the host's
  files and objects, answer `EINVAL: unsupported medium`, so programs fall back to sending the
  data); the Glyph Protocol off, so it is not advertised and programs that probe fall back to text;
  the terminfo name `xterm-256color`, the `TERM` the console's login gets, which XTGETTCAP's `TN`
  reports; continuation tracking of up to 64 KiB, so a snapshot can be taken while output stops
  inside a sequence); a restored tab whose snapshot did not come back has its transcript printed
  into the new terminal, then the restore notice and queued messages are; then the writer, the pump
  and the waiter start.
- If opening the console or spawning fails, the reason is printed, the console is closed and the
  tab counts as ended (127), with no "Process completed" line and no writer. If the terminal cannot
  be made (the kernel is gone, libghostty-vt fails, a panic), the tab ends (127) without a word,
  and later messages for it are refused.
- A restore replaces whatever was pending with its notice; a message shown before the terminal
  exists is queued after it as a plain yellow line, not inserted as below.
- The pump reads the console into a 64 KB buffer, asks the kernel whether the console now reads a
  password (`Machine::console_password_input`: lines without echo, which programs set before
  printing their prompt) and notes it in vt.c, feeds the terminal under the tab's lock (in parts
  that end at the start or end of an OSC or DCS string, `ghostty_terminal_vt_write_until`, where
  vt.c keeps an OSC's first 64 bytes to find clipboard reads and prompt starts and a DCS's first 5,
  its header, to find tmux's control mode; no slower than one write), writes
  the terminal's replies (DA, DSR, XTVERSION, size reports, pastes' bytes, the line that detaches
  tmux's control mode) back to the console
  before reading again, calls `VtSession.onOutput` only when it sets `dirty` (so a burst asks for
  one frame, and nothing is asked while no view takes frames) and `onTitle` after OSC 0/2. It runs
  whether or not a view shows the tab, and ends when the kernel is gone, the console reads end or
  fail, a reply cannot be written, a call into Java throws, or it panics.
- What programs ask of the app is taken after each read (`appine_vt_take_effects`) and handed to
  the session, which posts it to the service unless released: the bell (any number of BELs in a
  read counts once); a clipboard write (OSC 52, OSC 1337, OSC 5522, to any location, as Ghostty's
  `clipboard-write = allow`): its first `text/plain…` representation up to 256 KiB (larger is
  denied, other types unsupported; those replies reach only OSC 5522 programs), or a request to
  clear (an OSC 5522 write of more than 1 MiB in all its representations fails with EFBIG inside
  libghostty-vt, which buffers a write whole before handing it over); a notification (OSC 9's
  body, OSC 777's title and body, up to 4 KiB each; only the last of a read); a progress report
  (OSC 9;4, only the last of a read); program status reports (OSC 7501, every one in order, which
  libghostty-vt has already checked against the specification and whose title and message it has
  decoded; with the callback set it also answers the protocol's `?` query); clipboard reads
  (OSC 52 with `?` for data: an OSC string of `52;`, the clipboards named (`c`, `p`, `q`, `s`,
  `0`–`7`, maybe none) and `;?`, ended and not cancelled, however the reads split it), counted
  with the first clipboard the last one named (`p` or `s`, else `c`), which every read of the
  batch is answered for; the last command the prompt marks saw finish (Shell integration); the end
  of tmux's control mode, which vt.c detached (below), delivered last (`onTmuxDetached`).
  libghostty-vt's clipboard-read
  callback is left unset: it must answer before returning, under the tab's lock, which rules out
  asking the user; so OSC 5522 reads are refused and Kitty paste events never sent. What the app
  prints itself (`VtNative.print`) asks nothing: its replies and effects are dropped. Where they go
  is android/service's choice; the bell of the tab in front comes back as `VtView.bell`.
- Clipboard reads, as Ghostty's default `clipboard-read = ask`: the session queues them (the
  clipboard each named) and tells the service once for the reads that come before an answer; the
  user is asked in the tab in front (android/terminal), and `VtSession.answerClipboardRead` writes
  one answer per read through the tab's writer, in Ghostty's form `ESC ] 52 ; <clipboard> ; <base64>
  ESC \` (ST, whatever ended the request): the text when allowed, nothing (an empty clipboard, as
  libghostty-vt answers a read it refuses, so the program does not wait for a timeout) when denied,
  dismissed or asked from a tab not in front.
- tmux's control mode (`tmux -CC`), which libghostty-vt built for apps does not parse (Confirmed
  bugs / technical debt), would leave the tab showing nothing while keys reach tmux as commands.
  vt.c finds a DCS whose header is literally `1000p`, however the reads split it, and, as Enter
  would, puts a carriage return among the replies the pump writes before reading again: tmux takes
  the empty line as a detach, leaving its session running, and ends the string with ST. That end
  is reported (only an ESC ending it, which libghostty-vt reports at ST's first byte; 0x9C is
  payload once the header is complete, and a string a program cancels with CAN or SUB, or the app's
  own CAN cuts, reports nothing), and the service shows a yellow `[appine]` line in the tab saying
  so and that `tmux attach` brings the session back (android/service; nothing on the alternate
  screen). Other DCS strings (DECRQSS, XTGETTCAP, tmux passthrough) only cost two extra stops each. Nothing
  retries when tmux does not detach.
- Progress and status (`VtSession`, main thread): a progress report replaces the last one (a
  removal clears it) and lasts 15 s without another, as in Ghostty. Status reports of a read are
  applied together, as the specification keeps them: one record per id, a report replacing its
  record whole, a clear removing the record and those beneath it (`build` takes `build/test`; an
  empty id, as after a full reset, takes all), at most 64 records, the one updated longest ago
  dropped; text direction controls are removed from titles and messages. `status` is the record
  most in need of the user: blocked, then error, done, working, idle, the most recently updated
  among equals. A change of either (by value) is reported to the service (`onStatusChanged`).
- The pump stands in for shell integration's prompt mark, which only some shells send: after a
  read that carried a progress or status report it notes the console's foreground process group
  (`Machine::console_foreground`; a failed lookup notes none, and that report then waits for its
  timeout, being seen or the shell's end), and on a later read where another group (or none) has
  the foreground it calls `onForeground`,
  which drops the progress and the working, blocked and idle records, keeping done and error ones
  until seen. Seeing the tab (`statusSeen`, from the service) drops done and error records; the
  shell's end drops everything.
- The waiter waits for the shell (−9, "signal 9", once the kernel is gone), gives the pump up to
  0.5 s to take the last output, then on the main thread closes the console (once), stops the
  writer and, unless the tab was released, drops the tab's progress and status records, prints
  "[Process completed (code N | signal N) - press Enter]" and reports the end.
- `release` (the tab closed, or its kernel going away) hangs up the console once, stops the writer
  (writes already queued still run) and destroys the terminal handle; a running pump keeps the
  terminal until it returns.
- Saving: `transcriptText` formats the primary screen with its scrollback as plain text (soft
  wraps joined, trailing blanks trimmed) and returns its last 256 K characters
  (`Tabs.MAX_TRANSCRIPT`, all that saving keeps), which it also keeps; on the alternate screen,
  when nothing changed the screen since the last transcript (no output, message, resize, clearing
  or reset), or when formatting fails, it returns the last transcript taken on the primary one (""
  when none was). The service also takes the terminal's snapshot (Snapshots and restoring).
- Kill reports go through `VtNative.message` (a yellow line inserted above the cursor line: a CAN
  first when the parser is not at ground (`VT_GROUND`, which includes the UTF-8 decoder), so a
  program that died inside an OSC, DCS or APC string does not swallow it; then as many line feeds
  as the text needs rows, up again, save the cursor, insert lines, write, restore and move down,
  dropping any replies; skipped on the alternate screen) or are queued until the terminal exists.

### Scrollback compression

- libghostty-vt compresses the primary screen's cold pages (wholly in the scrollback and outside
  the viewport; the alternate screen keeps no scrollback) to about a fifth of their 400 KiB, and
  restores a page when something reads it: drawing a viewport scrolled back over it, the formatter
  (transcripts, copies), select all (which reads the oldest and newest pages), a mouse's
  Ctrl+triple-click that selected a command's output and a tapped link's line; searching restores
  nothing. The scrollback limits count pages uncompressed, so the same
  history is kept in less memory.
- A pass is scheduled when a call into vt.c left work for one, which vt.c tells as the
  `TermGuard` is released: libghostty-vt's activity token changed (rows moving into the
  scrollback, a resize, scrolling), or one of the reads above restored pages, which vt.c marks
  itself since the token does not change for them. The `vt-compress` thread looks at the tab
  250 ms later and, while the terminal keeps changing, waits another 250 ms each time, but not
  beyond 5 s from scheduling, so output that never pauses is compressed too. It then runs
  libghostty-vt's incremental steps 1 ms apart, each under the tab's lock, until a pass finds
  nothing left; the last step frees vt.c's text buffer if a transcript or copy grew it past
  64 KiB.
- Memory trimming (`VtNative.compressAll`, from android/service) compresses every tab's cold pages
  at once, idle or not, one tab's lock at a time.
- Under memory pressure the service also cuts the images of the tabs not shown
  (`VtNative.dropImages`): vt.c lowers the tab's Kitty storage limit to 4 MiB and puts it back to
  32 MiB, the only way libghostty-vt gives image memory back. On each screen it deletes whole
  images with their placements, those no placement shows first, then the oldest sent, until the
  rest fit, so images in the scrollback or on the screen go too. The last snapshot is left as it
  was, images included: taking a new one would decompress every page of the scrollback.
- Saving does not format a transcript again while nothing changed the screen, since formatting
  restores every compressed page; a tab whose text changed is formatted at each save and
  compressed again afterwards. A snapshot, which reads every page, marks the terminal touched too.

### Snapshots and restoring

- A snapshot (`VtSession.takeSnapshot`, `VtNative.snapshot`, on the service's worker) is
  libghostty-vt's binary record of the whole terminal: both screens, the scrollback, images,
  colours, modes, the title and the unfinished sequence the parser is in. One is encoded only
  after the terminal changed since the last (output, a message, a resize, clearing or resetting;
  null otherwise, and the service keeps the last file); none can be taken (empty) when encoding
  fails, output having stopped inside a sequence longer than the 64 KiB tracked, or the snapshot
  is larger than 16 MiB (mostly images), and none is tried again until the terminal changes. A full
  scrollback of coloured output takes about 1.5 MB, encoded in about 1 ms and decoded in about
  3 ms on a Mac; the service stores it deflated (android/service).
- Restoring (`VtNative.restore`) decodes a snapshot into a new terminal, compressing its
  scrollback as it comes and keeping its images within the 32 MiB per screen. Before any callback
  is installed (nothing is reported, replies go nowhere), vt.c undoes what the programs that ran
  in it set up for themselves: an unfinished sequence is cancelled (CAN), the alternate screen
  left for the primary one, a soft reset (DECSTR) brings back the cursor, margins, attributes,
  cursor and keypad modes, the palette and the cursor style, reverse video, mouse tracking and its
  formats, focus events, bracketed paste, synchronized output, colour scheme and in-band size
  reports and Kitty paste events are reset, alternate scroll set, the Kitty keyboard flags popped,
  an open hyperlink closed, the title cleared and the mouse pointer's shape (OSC 22, which the
  snapshot carries and a full reset keeps) back to text; what follows is marked as output
  (OSC 133;C), so
  the prompt the snapshot was taken at is not cleared by a resize as a live one is, the cursor
  starts a line of its own when it is not at its start, and the rest of the screen below it is
  erased; the selection goes and the viewport returns to the bottom. The terminal then gets the
  options a new one gets and is resized to the grid. A snapshot that does not decode leaves the
  tab to its transcript (logged).

### Clearing and resetting

- ⋮'s Clear screen (`VtSession.clear`, `VtNative.clear`) is Ghostty's clear_screen, and does
  nothing on the alternate screen (the item is disabled there): the selection goes and the
  scrollback is erased (ED 3). At the shell's prompt (prompt marks) the whole screen goes too (ED 2,
  which libghostty-vt turns into a move into the scrollback when the last non-blank row is a
  prompt, hence the ED 3 after it) and the shell gets a form feed (^L) to draw its prompt again;
  elsewhere (a command running, or a shell without marks) the rows above the cursor's are deleted
  from the top (DL), so the cursor's row moves up to it with the rows below, and the screen's
  Kitty placements are deleted with the images no other one shows. The viewport returns to the
  bottom.
- Reset terminal (`VtSession.reset`, `VtNative.reset`) is Ghostty's reset: libghostty-vt's full
  reset (RIS) of both screens, the scrollback, modes, the palette, the title and images, keeping
  the app's options and the colours set with OSC 10, 11 and 12; the selection, a
  synchronized-output hold and the prompt's way of moving its cursor end, a shell that was at its
  prompt gets a form feed (^L) to draw it
  on the empty screen, and the tab bar hears that the title went. Unlike a program's RIS, it
  reports no removal of the progress and no clear of the status records (libghostty-vt reports
  them only from the stream), so the tab keeps them (the progress still lapses 15 s after its last
  report).
- Clearing writes its sequences as the app's own (`write_own`, as kill reports do): after a CAN
  when the parser is inside a sequence a program left unfinished, keeping the console's replies
  as they were; resetting calls libghostty-vt's reset directly.

### Shell integration

- The guest's login shells mark their prompts with OSC 133 (android/setup's profile: bash marks
  the prompt, the typed command and the output's start; busybox ash the prompt, and the command
  ending with its line). libghostty-vt keeps the marks in the rows and cells it writes (prompt,
  input, output) and the cursor's place among them, which closing, jumping, Select output and taps
  on the input read through vt.c's shell integration functions; a mouse's Ctrl+triple-click
  selects a command's output through libghostty-vt's selection gesture, and a right-click looks
  for one for the menu's Select output. A shell without the marks (zsh,
  fish, dash, a nested non-login shell, a remote one over ssh that sends none) leaves every cell
  output: it is never at a prompt, there is nothing to jump to and no output to select, and no
  command is timed.
- Commands are timed through libghostty-vt's OSC 133 callback (`on_semantic_prompt`): a command
  starts at the output's mark (bash's PS0, `C`) or, as busybox ash marks none, when a feed ends
  with the cursor no longer at the prompt where the one before left it there (the line feed that
  ends the typed input, but also output of a background job at the prompt); it finishes at the
  command end (`D` with the exit code) the next prompt sends, and a primary prompt's start
  without one forgets it. A command end with nothing timed (a prompt drawn again after a resize,
  Ctrl-C at the prompt) reports nothing. The pump hands the last finished command of a read to
  `VtSession.onCommandFinished`, and the service notifies for one that ran at least 10 s in a tab
  not in front (android/service).
- At the prompt (`VtNative.atPrompt`, libghostty-vt's cursor-at-prompt: the cursor in a prompt or
  the input after it, on the primary screen): `VtSession.busy` is false and closing asks nothing.
  A command typed and entered ends the input at its line feed, so a running command, even a silent
  one, counts as busy until the next prompt; jobs left in the background do not.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` (creates and holds the `VtSession`s: `setRestored`, `showMessage`,
  `transcriptText`, `takeSnapshot` and `savedSnapshot`, `title`, `isRunning`, `console`, `login` and
  `resolvedLogin`, `id`, `status`, `statusSeen`, `describe`, `setCursor`, `answerClipboardRead`,
  `release`), its memory trimming (`VtNative.compressAll`, `dropImages`) and `Tabs`
  (`VtNative.version`).
- `MainActivity` (android/terminal): `VtSession`'s `busy`, `cwd()`, `progress`, `status`, `clear`
  and `reset`.
- android/vtview: `VtNative`'s functions for frames, input, selection, links, taps on the prompt
  and search, on the attached session's terminal (`VtSession.tab`), and `VtSession.resize`.

### Outbound

- android/service through `VtSession`'s `Events`, posted on the main thread; android/vtview through
  `VtSession.view` (an invalidate on output, `onStarted`); `Native` (`openConsole`, `spawn`,
  `write`, `closeConsole`, `waitFor`, `consoleCwd`), hence the kernel's host console; `login.sh`,
  and the prompt marks of android/setup's profile (`profile.sh`).
- libghostty-vt 1.0.0, Ghostty's terminal library released apart from the app (the terminal
  state and its full reset, the default cursor's shape and blinking, writes that stop at OSC and
  DCS strings' starts and ends, render state with one overscan row, key and mouse encoders, paste,
  focus encoding, selection gestures (a finger's untimed word and line presses and drags; a
  mouse's timed clicks with their count, cell, word, line and output behaviours, rectangle drags,
  releases, dragged flag and anchor), word selection and selection containment (right-click),
  output derivation and keyboard adjustment, search with the list of all its matches, plain-text
  formatting, title and width,
  progress and program status reports, semantic
  prompts (cursor at a prompt, scrolling by prompts, the OSC 133 callback, rows' and cells' marks
  read through grid references), the full reset callback, unimplemented OSC and
  APC strings, its log, scrollback compression, snapshots with continuation tracking, Kitty
  graphics with their render-state image list and animation), and the Wuffs PNG decoder it
  carries, linked into
  `libappine.so` (`android/native`); the system's libjnigraphics (bitmap locking for images'
  pixels, android/vtview's); `Machine::read_console`,
  `write_console`, `resize_console`, `console_foreground` and `console_password_input`
  (kernel/tty) and the
  bridge's `guarded!`, handle table and string helpers, used from `vt.rs` directly.
- Of libghostty-vt's C API, the terminal has no use for: the parsers that work without a terminal
  (OSC, SGR); key and mouse events' getters; `wasm_*`, `type_json`, `ghostty_version` and
  `build_info` but its version string; the formatter's extras; the cursor's SGR style; colour
  string parsing and the X11 names; iterating Kitty placements directly; decoding a snapshot in
  steps and exporting its unfinished sequence; key events' composing flag and the macOS
  Option-as-Alt option, the one `set_from_terminal` does not copy; the standalone encoders the
  terminal's own handling replaces (`paste_encode`, `paste_is_safe`, `size_report_encode`,
  `mode_report_encode`, `color_scheme_report_encode`); the hollow block cursor style; setting the
  title or directory by hand. Defaults that suit it stay unset: resizing pulls rows back from the
  scrollback (right for a console that keeps no screen of its own), APC strings' size limits and
  getrandom. OSC 7 (and OSC 9 or 1337 directories) is left unwired: Ghostty takes a directory only
  from the local host, and for a local shell the kernel's foreground directory, which new tabs use,
  says as much.

## Invariants and gotchas

- A kill report opens its room with line feeds before moving back up, so the cursor's line moves
  down with the cursor; this needs the parser at ground, which the CAN ensures. A program that died
  in the middle of a UTF-8 sequence leaves a U+FFFD at its last output position: the CAN ends the
  sequence and the decoder replaces it. The report's cursor save overwrites one the program had
  saved.
- The console opens lazily, so restored tabs that were never shown have no console and no shell.
- `openConsole`, `spawn` and `closeConsole` run on the main thread and take the kernel's BKL, as
  does a resize (`VtNative.resize`, which resizes the kernel console from `vt.rs`; `Native.resize`
  is no longer used by the app); input is written on each tab's own thread, so a program that stops
  reading blocks only its tab.
- Console numbers are freed at close and reused lowest-first by the kernel, and a close carries no
  generation, so closing a stale number closes whichever console now has it.
- A tab refers to its kernel by handle and looks it up for each call: the pump once per read,
  holding it through the blocking read and the reply write and dropping it before calling into
  Java; each resize; like the `Native` functions. `Native.stop` and `shutdown` remove the handle
  and end the processes, and the store is closed when the last `Arc<Machine>` goes, so a tab
  holding one would keep the old store open after a reboot, tier change or distro switch, and the
  next kernel would start without the boot cleanup. The service releases its tabs (closing their
  consoles) before shutting the kernel down, so the pump's read returns first.
- A `VtSession` closes its console exactly once (`hangUp`), so a later close cannot hang up a tab
  that reused the number.
- The pump stays in native code for the tab's whole life, so JNI local references it makes for
  each event (the clipboard text, a notification's strings) must be deleted after the call, or
  they pile up in its frame.
- `AppineVtEffects` (vt.c) and `RawEffects` (vt.rs) are one layout and change together, as the
  frame format does (vt.c's comments name vt.rs's `Effects` for it).
- A restored terminal is a new one (the decoder makes it): the userdata and callbacks are not in a
  snapshot, nor are the Kitty storage limit (a decoder option), the media, the Glyph Protocol,
  the continuation limit (the unfinished sequence's bytes are), the terminfo name and the
  clipboard-write and unknown-sequence limits;
  the snapshot carries the scrollback limits, default colours, default modes (2027's) and cursor
  defaults and whether the cursor follows them (older snapshots a steady cursor). vt.c sets every
  option on it as on a new terminal, after `settle` (whose DECSTR makes the cursor follow its
  defaults again), and the session then sets the user's cursor, so the app's values win.
- Snapshots are recorded with the libghostty-vt version that wrote them (`1.0.0`; the copy in
  Ghostty 1.4.0 recorded `ghostty-1.4.0/0.1.0-dev`). One from another version is not decoded,
  since the snapshot format is versioned apart from the library and outside its ABI promise, and
  an app update is the commonest way a run ends with tabs to restore; those tabs come back from
  their transcripts.
- libghostty-vt's clipboard-read callback must answer before it returns, which happens on the pump
  thread under the tab's lock: asking the user there would block every frame (the main thread
  takes the same lock). OSC 52 reads are therefore found by vt.c and answered later through the
  tab's writer, and OSC 5522 reads, which only the callback can serve, stay refused.
- vt.c's OSC and DCS tracking (`osc_open`, `dcs_open` and `tmux_control`) follows only what goes
  through `appine_vt_feed`. App writes that cancel a program's string with CAN (`write_own`) clear
  it (a tmux control string so cut is never reported as detached); any other write into the
  terminal that could end or start such a string must too, or a later string's start is misread.
  Text the app prints (`VtNative.print`) goes through the tracking, but its replies are dropped,
  so it cannot detach tmux.
- The tmux detach rests on tmux taking a lone carriage return in control mode as an empty line,
  which detaches it (tmux 3.5a and 3.7c do); the reply goes out once per string. Should a program
  ever start and end such a string within one read, the carriage return still reaches whatever
  reads the console next (an empty line at a shell's prompt).
- Continuation tracking keeps at most 64 KiB of an unfinished sequence: a snapshot taken while
  output stops inside a longer one (an OSC 52 write, a large Kitty transmission) fails, and that
  save leaves the tab without a snapshot until the terminal changes.
- libghostty-vt is not thread-safe: every call into `vt.c` holds the tab's lock (recovered when
  poisoned); the frame buffer is written only by `frame`, which only the main thread calls, so the
  main thread reads it after the lock is released. vt.c's text buffer is shared by title, copy,
  link and transcript, so each result is turned into a Java string under the lock (the end of a
  compression pass frees it when large).
- Compression is scheduled only when a `TermGuard` is released, so every call into vt.c that can
  make work must lock through `Tab::term`; the compressor and `compressAll` lock the mutex
  directly because they make none (through the guard, a step would schedule its own pass). A tab's
  lock is taken before the schedule's, never after: the compressor lets the schedule's lock go
  before a step.
- libghostty-vt's compression activity token does not change when a read restores compressed
  pages, so a vt.c function that reads beyond the viewport (the formatter, select all, grid
  references above or below it) must set `compress_touched`, or what it restored stays
  uncompressed until other activity. A vt.c function that writes into the terminal must clear
  `transcribed`, or saves keep the old transcript.
- Compression gives page memory back with `MADV_DONTNEED` on ranges aligned to Zig's minimum page
  size (4 KiB), which a 16 KB-page kernel rounds up. That stays harmless because every terminal
  page, pooled or not, is a mapping of its own from Zig's page allocator, so the rounding never
  reaches another page; a libghostty-vt that packed pages into shared mappings would break it.
- Since Ghostty 1.4's libghostty-vt, `GHOSTTY_TERMINAL_OPT_USERDATA` takes the pointer itself, not
  its address (older code passing `&ptr` crashes in the reply callback).
- The `get` and `set` functions take `void *` values, so a value of the wrong type compiles: 1.0
  changed some (a cell's graphemes come in a `GhosttyCodepointBuffer`, a Kitty image's pixels in a
  `GhosttyString`), and an upgrade must check every key's documented type, not only what fails to
  compile.
- Without a DA reply, programs that probe for it (vim, tmux) wait for a timeout, so `vt.c` answers
  DA1 (132 columns, selective erase, ANSI colour, and clipboard access, 52, which Ghostty lists
  whenever clipboard writes are allowed and programs check before using OSC 52), DA2 (VT220), DA3
  and XTVERSION ("appine") and reports a dark colour scheme. XTGETTCAP answers `TN` with the name
  vt.c sets, and every other capability from libghostty-vt's built-in table of Ghostty's own entry,
  not from `xterm-256color`'s.
- A progress or status report from a command that exits at once (a `printf` at the prompt) is
  usually dropped as soon as it shows: the read after it finds the shell back in the foreground. A
  report made after the reporting group has already given the terminal back is kept until the next
  command takes the foreground. A program in the background reporting is tracked against whatever
  group was in the foreground then.
- The status records live on the main thread only; the pump hands reports over through a locked
  list drained by one post, so the reports of one read are usually applied together and a passing
  state between two of them notifies no one. A program's full reset (RIS) arrives as a clear of
  every record and a removal of the progress, before libghostty-vt's reset callback (which only
  forgets how the prompt moves its cursor); the app's Reset terminal sends neither, nor calls the
  callback, so vt.c forgets that itself there (Clearing and resetting).
- Command timing's at-prompt state (`was_at_prompt`) changes only in `appine_vt_feed`, which the
  app's own printed text also goes through; clearing, resetting, messages and resizes leave it.
  For busybox ash, which marks no output start, output of a background job while the shell waits
  at its prompt starts the timing, so the next command's time is counted from then; bash's output
  mark replaces such a start. libghostty-vt is built ReleaseFast, so its log emits info and
  above, of which `on_log` passes warnings and errors; the log is installed when the first
  terminal is wrapped, after it was created or restored, so what that first decode and settle
  would log goes nowhere.
- Without default foreground and background colours libghostty-vt treats them as unset: OSC 10
  and 11 queries go unanswered, reverse video (DECSCNM) swaps nothing, and a colour set with OSC 10
  or 11 alone does not reach the render state. With them, an OSC 12 query without a cursor colour
  answers the foreground. A full reset (RIS) resets the palette but keeps colours set with OSC 10,
  11 and 12, as Ghostty does.
- libghostty-vt leaves the Glyph Protocol off (since 1.0; the copy in Ghostty 1.4.0 turned it on
  and answered `fmt=glyf` to its query), and the C API gives no access to registered glyphs; vt.c
  turns it off at create all the same. A full reset
  keeps that setting, the Kitty storage limit and the allowed media (the alternate screen takes
  the primary's), so they stay as set. Its Kitty media default is data in the escape sequences
  only; vt.c sets it again so a library change cannot open the file media, whose paths would be
  the app's own files rather than the guest's.
- As built for an app (`-Demit-lib-vt`), libghostty-vt has no PNG decoder unless one is installed
  (`ghostty_sys_set`, process-wide), though libghostty-vt.a contains Wuffs: vt.c's decoder calls
  Wuffs's C functions in the archive, compiling against the header from the release's Zig
  packages.
  Without it, PNG images (`f=100`, what most programs send) fail with `EINVAL`.
- Erasing below the cursor (ED 0, what busybox's `clear` sends) leaves images in place, as in
  Ghostty and kitty; ED 2 and 3 delete the visible ones (and the scrollback's).
- With grapheme clustering on, programs that count widths per codepoint (`wcwidth`) disagree on
  emoji sequences; they can turn mode 2027 off.
- libghostty-vt's C terminal starts with a scrollback byte limit of 10,000 bytes (the default of
  its `Terminal.Options`, not Ghostty's 50 MB `scrollback-limit-bytes`), raised to its minimum of
  the active area's pages plus one, so a line limit alone keeps about two pages. The byte and line
  limits both apply, the first reached pruning whole 400 KiB pages; a page holds fewer rows the
  wider the terminal (about 1,000 at 45 columns, 580 at 80, 190 at 250).
- With prompt marks, every resize while the cursor is at a prompt (the keyboard showing or hiding
  is one) clears the prompt's rows, from its first row down, for the shell to redraw them on
  SIGWINCH (bash's marks ask for the last row only, `redraw=last`, as readline redraws). busybox
  ash and bash redraw on every resize, rows only included; a shell given the marks that did not
  would lose its prompt and typed input until the next one.
- Shell integration lives in the guest (`/etc/profile.d/appine.sh`): a tab's behaviour (closing
  without asking, jumps, Select output) follows whatever the shell in it sends, so a user's own
  prompt or shell changes it, and a remote shell over ssh can send marks too.
- Text the app prints does not consume the title-changed flag, so a title it sets is reported at
  the pump's next read; a notification cut at 4 KiB can split a UTF-8 sequence, which vt.rs decodes
  lossily.

## Confirmed bugs / technical debt

- **Shell integration is partial**: only bash and busybox ash get prompt marks (not zsh, fish or
  nested non-login shells); ash's mark the typed command as ending with its line, so the rows of a
  command longer than the screen's width after the first count as output (ash breaks such lines
  itself), and with the cursor on one of them a tap moves nothing, ash marks no output start, so a
  background job's output at its prompt starts the next command's timing early, and a PS1 set
  after the profile (ash's `~/.profile`) has none; and the progress and status records still go by
  the foreground process group rather than the next prompt, though the OSC 133 callback is now set
  (for command timing only).
- **libghostty-vt features not wired**: ENQ, the title report (CSI 21 t),
  XTCHECKSUM and window resizing (CSI 8 t) are left unanswered, as Ghostty's defaults leave them
  (`enquiry-response` empty, `title-report`, `vt-xt-checksum-report` and
  `vt-window-resize-allowed` false); OSC 5522 clipboard reads are refused and Kitty
  paste events never sent, since only the read callback, which cannot wait for the user, serves
  them (wiring it would also need the IME's multi-line commits, pasted as
  `GHOSTTY_PASTE_SOURCE_CLIPBOARD`, marked `GHOSTTY_PASTE_SOURCE_TEXT`, or they would become paste
  events).
- **More of libghostty-vt left unused**:
  - the rest: VT or HTML formatting with colours and OSC 8 links (transcripts are plain text); the
    memory usage and the VT processing-error flag; the OSC 133 event's command line and error text;
    KAM (mode 2), ignored as Ghostty ignores it by default;
  - out of reach in the C API: the terminal's visibility (mode 2033 reports every tab as visible)
    and the password-input flag (vt.c keeps its own); Kitty drag and drop (OSC 72, a callback only
    in the Zig API); OSC 66, OSC 99 and hierarchical contexts, parsed but not acted on; tmux control
    mode, left out with Oniguruma when libghostty-vt is built for apps, so `ESC P 1000 p` would be
    swallowed as an unknown DCS: vt.c detaches tmux as it starts (Start and end), and `tmux -CC`
    gets no windows or tabs as iTerm2 gives it.

## Open questions

- Should a status notification name the record's `app` when it has no title, as a key for grouping
  (the specification says it is not a label), rather than the tab's title?
- Should zsh and fish get prompt marks too (Ghostty's bash script, derived from Kitty's, is
  GPLv3; its zsh and fish scripts' licences were not checked), and should a background tab's
  progress and status be dropped at the next prompt mark rather than when the foreground group
  changes?
- Should the menu's Reset terminal also drop the tab's progress and status records, as a program's
  RIS does?

## To verify

- Tried only on a Pixel 4a API 34 emulator (4 KB pages) and through vt.c's harnesses on a macOS
  build, where the figures for scrollback rows and memory also come from; not on real or 16 KB-page
  devices. Unverified beyond that:
  - shell integration: bash's marks in the app (seen only in a container); the console's password
    state at sudo, passwd, ssh or doas prompts;
  - output: progress and status from real programs (cargo, systemd, AI coding CLIs); the copy toast
    on Android 12 and earlier; tmux -CC with versions other than 3.5a and 3.7c; how programs that
    probe for the Glyph Protocol fall back;
  - memory: scrollback compression on 16 KB-page devices and its battery cost; snapshots with
    images, bash's marks, a program on the alternate screen at saving, or across a library version
    change.
