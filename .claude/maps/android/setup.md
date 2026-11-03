# android/setup Map

> Static understanding snapshot, not a decision history.
> See `.claude/decisions/android/setup.md` for the paired decision history.
>
> Verified: 2026-11-03 (full mapping: 1 research concern; 5 claims checked by 1 independent verifier — 4 confirmed and 1 partial; bugs and debts recorded)

## Responsibilities

`android/setup` provides the guest system and its account setup:

- the distributions (Alpine from the APK, Debian downloaded), downloading and verifying tarballs,
  importing them into a new store, and writing first-boot configuration;
- files refreshed before every kernel start (network interfaces, the bundled doas package, the
  profile script) and the osclock script run before boot;
- package mirrors: built-in lists, speed probes, rewriting the sources file;
- resolving the login account and the per-tab login script (`login.sh`), including the first-login
  admin setup;
- the settings screen, which only displays the service's state and forwards choices;
- the store lock shared by the kernel start, store deletion and the Files provider.

Root: `android/app/src/main/java/io/github/mthli/appine/Rootfs.kt`, `SettingsActivity.kt`,
`android/app/src/main/assets/login.sh`, `scripts/fetch-rootfs`.

Outside the module: when imports, switches and logins happen (`android/service`); the import and
`write_file`/`read_file` implementations (`kernel/store`, `kernel/vfs`); bundling the assets
(`android/native`).

## Key types

- `Rootfs` — object: `Distro` (`ALPINE`, `DEBIAN`: label, download, mirrors, probe), `Download`
  (urls, sha256, size), `Cancelled`, `Account`, `access` (a `ReentrantReadWriteLock`).
- Preferences: file `rootfs` (`distro`, `mirror`, `mirror_manual`); file `settings` (`tier`,
  `login`, `keep_awake`, `system_documents`).
- `SettingsActivity` — rows for distro, mirror, reset, login account, tier, keep-awake, system
  documents and licenses.
- `login.sh` — `sh -c <script> appine-login <login> <cwd>`, run as root on each tab's console.

## Public entry points

- `Rootfs.prepare(distro)`, `download`, `fetch`, `isReady`, `installed`, `reset`, `configure`,
  `ensureNetworkConfig`, `ensureLoginTools`, `updateProfile`, `PREFER_OSCLOCK`, `failedServices`,
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
  up to 5 times when an attempt made progress (restarting on a non-206 reply), checks length and
  sha256, and renames; a verified cached tarball is reused; cancellation is checked before each
  read.

### Import

- `prepare`: delete `rootfs.tmp`, import into it, delete the tarball, `configure`, record the
  distro and its first mirror in preferences (`commit`), sync the store, rename to the real store
  and fsync `files/`. A store without a distro record counts as Alpine.
- `configure` writes: package sources (apk `main` and `community`; Debian deb822 over http for
  trixie, -updates, -security), `hostname`, `hosts` (Alpine only when absent, Debian always),
  Debian's `resolv.conf` pointing at the DNS stub, `motd`, and `/etc/profile.d/appine.sh` (motd,
  `TERM`, `LANG`, `NMAP_UNPRIVILEGED`, `PS1`).
- Before every start: a loopback-only `/etc/network/interfaces` when absent; on Alpine the bundled
  `doas.apk` into `/var/cache/appine/`; an unmodified old profile (with `cd "$HOME"`) replaced.
- Before boot (`PREFER_OSCLOCK`): with OpenRC installed and no clock service in any runlevel, add
  osclock to boot.

### Mirrors

- Each distro has built-in mirrors (official, Tsinghua, USTC, Aliyun). After an import the service
  probes them concurrently (up to 512 KB or 5 s each, by throughput) and writes the fastest unless
  the user chose one; manual choice and automatic writes are serialised by one lock.
- `retarget` rewrites only built-in mirror hosts (http or https, commented lines too), keeps the
  scheme and the rest of each line, and leaves other sources alone; a missing sources file is
  written from the template; a file with no built-in mirror is left alone with a hint.

### Accounts and login

- `loginAccount`: `auto` is the regular account (UID 1000–65533, a real login shell) with the
  lowest UID, else root; `root`; or a named passwd entry, falling back to root.
- `login.sh` traps SIGINT for itself, picks the account the same way, and:
  - root: `cd` to the directory and `exec` root's passwd shell as a login shell;
  - a regular account the first time (not in `/var/lib/appine/admins`): add it to wheel (Alpine,
    with `permit persist :wheel`) or sudo (Debian), record it, install doas from the bundled package
    and re-add it by name, or install sudo from the network showing apt's progress (Ctrl-C skips
    until the next login); marker files stop reinstalls;
  - every regular login: rewrite `/etc/doas.d/30-appine.conf` or `/etc/sudoers.d/appine` with
    NOPASSWD lines for recorded admins still in the group whose shadow password is unset;
  - switch with `su <account> -c '<cd dir or home>; exec "$SHELL" -l'` (`--session-command` when
    util-linux su offers it); package commands read from `/dev/null` so early typing reaches the
    shell.

### Settings

- Every row reads the service's state and calls it; rows that need a running kernel (mirror, tier)
  are disabled otherwise; distro and reset stay usable after a start failure; a running download
  turns the distro row into "cancel".

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
- An import interrupted by a kill is redone as Alpine on the next launch (no store means
  `start(ALPINE)`), without telling the user; a verified Debian tarball stays in the cache.
- Preferences are written before the rename, so an existing store always has its distro record.
- `writeFile` while the kernel runs is seen at once (the store's commit counter invalidates kernel
  caches).

## Confirmed bugs / technical debt

- **The mirror setting can disagree with the sources.** `chooseMirror` treats a failed write as
  success (`getOrDefault(true)`), records the mirror and shows it in Settings while the sources
  file still names the old one.
- **The NOPASSWD rules fail open.** A shadow field starting with `!` counts as "no password", so a
  `passwd -l` account gets NOPASSWD; a missing `/etc/shadow` makes every admin passwordless; rules
  are refreshed only at regular-user logins, so a newly set password still has a NOPASSWD line
  until then.
- **The admin setup ignores failures and matches loosely.** Failed `addgroup`/`usermod` still
  records the user as set up (never retried); the admins file is matched with a regex
  (`grep -qsx` without `-F`).
- **A switch interrupted by a kill silently becomes Alpine** (see above).
- `fetch` reads until EOF with no cap at the expected size, so a misbehaving server can fill the
  cache until the read timeout or a full disk.

## Open questions

- Should a locked account (`!<hash>`) count as "no password" for the NOPASSWD rule?
- Should root logins also refresh or remove the NOPASSWD rules?
- Should an interrupted Debian import resume as Debian?

## To verify

- That Alpine's doas reads `/etc/doas.d/*.conf` in lexical order (the NOPASSWD file relies on coming
  after `20-wheel.conf`).
