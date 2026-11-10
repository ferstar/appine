# android/setup Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/setup.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 5 claims checked by 1 independent verifier — 4 confirmed and 1 partial; bugs and debts recorded)
>
> Maintained: 2026-11-11 (targeted verification only since the full mapping: NOPASSWD rules and admins; mirrors; downloads and resumed imports; podman setup; the profile's prompt marks and replaced versions last)

## Responsibilities

`android/setup` provides the guest system and its account setup:

- the distributions (Alpine from the APK, Debian downloaded), downloading and verifying tarballs,
  importing them into a new store, and writing first-boot configuration;
- files refreshed before every kernel start (network interfaces, the bundled doas package, the
  profile script with its prompt marks for the terminal's shell integration) and the osclock
  script run before boot;
- package mirrors: built-in lists, speed probes, rewriting the sources file;
- resolving the login account and the per-tab login script (`login.sh`), including the first-login
  admin setup;
- the settings screen, which only displays the service's state and forwards choices;
- the store lock shared by the kernel start, store deletion and the Files provider.

Root: `android/app/src/main/java/io/github/mthli/appine/Rootfs.kt`, `SettingsActivity.kt`,
`android/app/src/main/assets/login.sh`, `android/app/src/main/assets/profile.sh`,
`scripts/fetch-rootfs`.

Outside the module: when imports, switches and logins happen (`android/service`); the import and
`write_file`/`read_file` implementations (`kernel/store`, `kernel/vfs`); bundling the assets
(`android/native`).

## Key types

- `Rootfs` — object: `Distro` (`ALPINE`, `DEBIAN`: label, download, mirrors, probe), `Download`
  (urls, sha256, size), `Cancelled`, `Account`, `access` (a `ReentrantReadWriteLock`).
- Preferences: file `rootfs` (`distro`, `mirror`, `mirror_manual`); file `settings` (`tier`, read
  by debug builds only; `login`, `keep_awake`, `system_documents`, `fill_wide`).
- `SettingsActivity` — an `AppCompatActivity` on the Material 3 page that it shares with the
  licenses (`android/terminal`'s `page.xml`, `list_item.xml`, `list_header.xml`); rows in groups
  under headings: System (distro, mirror, login account, reset), Android (keep-awake, system
  documents), Terminal (CJK characters filling two cells, `android/terminal`), Debugging (tier,
  debug builds only) and About (licenses).
- `login.sh` — `sh -c <script> appine-login <login> <cwd>`, run as root on each tab's console.
- `profile.sh` — copied verbatim, comments included, into the guest as
  `/etc/profile.d/appine.sh`; `Rootfs` keeps the texts of the versions without prompt marks
  (`OLD_PROFILE`, `UNMARKED_PROFILE`) and the sha256s of the later ones (`REPLACED_PROFILES`) to
  recognise them.

## Public entry points

- `Rootfs.prepare(distro)`, `download`, `fetch`, `isReady`, `installed`, `reset`, `configure`,
  `ensureNetworkConfig`, `ensureContainersConfig`, `ensureLoginTools`, `updateProfile`,
  `PREFER_OSCLOCK`, `failedServices`,
  `chooseMirror`, `setMirror`, `retarget`, `loginAccount`, `regularAccounts`.
- `scripts/fetch-rootfs` (build-time download of the Alpine minirootfs and the doas package).
- `login.sh` arguments: login setting (`auto`, `root` or an account name) and an optional start
  directory.

## Data flow / lifecycle

### Tarballs

- Alpine: `scripts/fetch-rootfs` downloads the minirootfs and the doas package at build time (pinned
  version and sha256; the GPG signature is checked when gpg is available); Gradle bundles them as
  `rootfs.tgz` and `doas.apk`; `prepare` copies the asset to the cache.
- Debian: one pinned GitHub URL (debuerreotype `debian:trixie`) with fixed length and sha256.
  `fetch` probes sources when there are several, writes `.part` while hashing, resumes with `Range`
  up to 5 times when an attempt made progress (restarting on a non-206 reply), stops at once (no
  resume) when a source sends more than the pinned length, checks length and sha256, and renames;
  a verified cached tarball is reused; cancellation is checked before each read.

### Import

- `prepare`: record the distro it imports (`commit`), delete `rootfs.tmp`, import into it,
  `configure`, record the distro and its first mirror in preferences (`commit`), sync the store,
  rename to the real store, fsync `files/` and only then delete the tarball. A store without a
  distro record counts as Alpine. While there is no store, the record names the distro an import
  was last started for (`importing`; Alpine when there is none), which the service imports at the
  next launch.
- `configure` writes: package sources (apk `main` and `community`; Debian deb822 over http for
  trixie, -updates, -security), `hostname`, `hosts` (Alpine only when absent, Debian always),
  Debian's `resolv.conf` pointing at the DNS stub, `motd`, and `/etc/profile.d/appine.sh` (the
  assets' `profile.sh`: motd, `TERM`, `LANG`, `NMAP_UNPRIVILEGED`, and for bash and busybox ash
  only a coloured prompt with prompt marks, below; other shells keep the distro's prompt).
- Before every start: a loopback-only `/etc/network/interfaces` when absent;
  `/etc/containers/containers.rootless.conf.d/appine.conf` setting `netns = "host"` when absent
  (rootless podman's pasta and slirp4netns need a TUN device Appine lacks; only rootless podman
  reads that directory, and a user's own `containers.conf` overrides it); on Alpine the bundled
  `doas.apk` into `/var/cache/appine/`; a profile left exactly as an earlier version wrote it
  (with `cd "$HOME"`, without prompt marks, or with marks that do not declare `cl=line`) replaced
  by the current one, an edited one kept.
  Storage is left to the packages: podman's overlay driver and docker's (containerd's) overlayfs
  snapshotter run on the kernel's overlayfs, rootful and rootless. A `storage.conf` choosing vfs or
  a `daemon.json` choosing native that an earlier build wrote stays until the user removes it.
- Before boot (`PREFER_OSCLOCK`): with OpenRC installed and no clock service in any runlevel, add
  osclock to boot.

### Prompt marks

- For the terminal's shell integration (android/terminal) the profile has login shells mark their
  prompts with OSC 133:
  - bash: a `__appine_prompt` function put first in `PROMPT_COMMAND` (once) prints, before each
    prompt, the last command's end with its status (D) and the prompt's start on a fresh line (A,
    `redraw=last;cl=line`), and wraps PS1 (P with `k=i` … B, the input starting) and PS2 (`k=s`),
    and appends the output's start (C) to PS0, each again whenever it is missing, so a PS1 that
    `~/.bashrc` or anything later sets is wrapped from the next prompt on;
  - busybox ash, which has no hook before a command or a prompt: PS1 itself carries D with `$?`
    (expanded at each prompt and redraw), A with `cl=line`, and after the prompt I (the input ends
    with its line); PS2 is P with `k=s` and I. A PS1 set after the profile goes without them.
  - `cl=line` tells the terminal that both line editors move the cursor with the left and right
    arrows, so a tap on the typed command moves the cursor there.
- Every login shell of a tab reads it (root's shell `-l`, and `su <account> -c … exec "$SHELL" -l`
  for others); nested non-login shells and other shells get no marks.

### Mirrors

- Each distro has built-in mirrors (official, Tsinghua, USTC, Aliyun). After an import the service
  probes them concurrently (up to 512 KB or 5 s each, by throughput) and writes the fastest unless
  the user chose one; manual choice and automatic writes are serialised by one lock. The recorded
  mirror changes only after the sources were written (a write that fails leaves both alone).
- `retarget` rewrites only built-in mirror hosts (http or https, commented lines too), keeps the
  scheme and the rest of each line, and leaves other sources alone; a missing sources file is
  written from the template; a file with no built-in mirror is left alone with a hint.

### Accounts and login

- `loginAccount`: `auto` is the regular account (UID 1000–65533, a real login shell) with the
  lowest UID, else root; `root`; or a named passwd entry, falling back to root.
- `login.sh` traps SIGINT for itself, picks the account the same way, and:
  - every login, root's included: rewrite `/etc/doas.d/30-appine.conf` or `/etc/sudoers.d/appine`
    with NOPASSWD lines for recorded admins still in the group that have no password: a shadow
    field that, after any leading `!`s, is empty or `*` (`adduser -D` writes `!`); `passwd -l`'s `!`
    before a hash still counts as a password, and so do a missing shadow entry and an unreadable
    `/etc/shadow`; the file is written beside it and renamed into place;
  - `--refresh-rules` as the first argument only rewrites the rules: `Rootfs.dirsChanged` runs
    it as root, without a console and without waiting, when the service's poll finds `/etc`
    among the directories the guest changed (`passwd`, `chpasswd` and `adduser` replace
    `/etc/shadow`; group changes replace `/etc/group`), so a password set in a running session
    makes escalating ask for it within about one poll (2 s);
  - root: `cd` to the directory and `exec` root's passwd shell as a login shell;
  - a regular account the first time (not in `/var/lib/appine/admins`, matched literally): add it
    to wheel (Alpine, with `permit persist :wheel`) or sudo (Debian) and record it once it really is
    in the group (otherwise say so and retry at the next login), install doas from the bundled
    package and re-add it by name, or install sudo from the network showing apt's progress (Ctrl-C
    skips until the next login); marker files stop reinstalls;
  - switch with `su <account> -c '<cd dir or home>; exec "$SHELL" -l'` (`--session-command` when
    util-linux su offers it); package commands read from `/dev/null` so early typing reaches the
    shell.

### Settings

- Every row reads the service's state and calls it; rows that need a running kernel (mirror, tier)
  are disabled otherwise; distro and reset stay usable after a start failure; a running download
  turns the distro row into "cancel".
- The tier row exists only in a debug build (`BuildConfig.DEBUG`); a release build has no way to
  choose and always runs Tier 1.
- Display: a row is a title over its value or description; a toggle row adds a `MaterialSwitch`
  that only shows the state (tapping the row toggles, and accessibility services see the row as a
  switch), whose first state jumps into place instead of sliding; a disabled row is drawn at 38%
  opacity. Choices and confirmations are `MaterialAlertDialogBuilder` dialogs; the login change is
  a toast.
- Look: Material 3 light or dark with the system, with the wallpaper's colours from Android 12 on
  (`DynamicColors`), edge to edge. A dark-mode change recreates the screen (it does not handle
  `uiMode`), which rebinds and renders again and drops an open dialog.

## Dependencies (inbound / outbound)

### Inbound

- `TerminalService` (start, switch, mirrors, login, tier), `Documents` (`loginAccount`, `installed`,
  `access`), `ConsolePty` (runs `login.sh`), `MainActivity` (opens Settings).

### Outbound

- `Native.importRootfs`, `syncRootfs`, `writeFile` (creates parent directories, does not follow
  symlinks), `readFile` (null when missing); `HttpURLConnection` (system proxy); `BuildConfig`
  (Alpine branch); the assets; `LicensesActivity`.

## Invariants and gotchas

- `Rootfs.access`: provider calls hold the read lock; starting the kernel and deleting the store
  hold the write lock, so the kernel's exclusive boot open never overlaps a provider open. The
  mirror-probing thread reads and writes the sources without the lock; it writes only after its
  probes finish (seconds later, never offline) and only after a fresh import, so an overlap with the
  kernel's open is unlikely and would skip a cleanup that has nothing to clean.
- An import interrupted by a kill (a first run, a distro switch, a reset) is redone with the same
  distro on the next launch, from the verified tarball still in the cache or by downloading it
  again with the usual progress; if that fails, the service falls back to Alpine with its notice.
- Preferences are written before the rename, so an existing store always has its distro record.
- `writeFile` while the kernel runs is seen at once (the store's commit counter invalidates kernel
  caches).
- `updateProfile` replaces only the versions it knows, compared whole (`OLD_PROFILE` and
  `UNMARKED_PROFILE` by text, later ones by sha256 in `REPLACED_PROFILES`): changing `profile.sh`
  must add the sha256 of the text it replaces (the committed asset, byte for byte), or stores keep
  the version they have; and the motd is written only at import, so it is never updated.
- The marks rely on the shell redrawing its prompt on SIGWINCH (the terminal clears the prompt's
  rows on a resize; android/terminal): bash and busybox ash do.

## Confirmed bugs / technical debt

- **The rules miss changes that rename nothing in `/etc`.** A shadow or group file edited in place
  (an append, a text editor writing the file itself) changes no entry of `/etc`, and a burst of
  more than 256 changed directories between two polls stops recording the rest, so such a change
  waits for the next login; while the app is not running there is nothing to refresh.

## Open questions

none

## To verify

- That Alpine's doas reads `/etc/doas.d/*.conf` in lexical order (the NOPASSWD file relies on coming
  after `20-wheel.conf`).
- The profile's bash marks in the app on Debian (tried only in a Docker container with Ubuntu
  26.04's bash 5.3; busybox ash's in the app and an instrumentation test), and with a
  `PROMPT_COMMAND` array or a prompt tool that rewrites PS1 after it each time.
