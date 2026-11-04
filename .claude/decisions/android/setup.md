# Rootfs, settings and login (android/setup) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/setup"`
>
> Last distilled: 2026-11-05 (HEAD = 173da76). D1–D8 were seeded by hand from the retired design doc
> §11.1 and §11.4 and the round records in `docs/rounds/`, so their sources cite rounds; later
> entries cite commits.

## Active

### D1: Alpine is bundled and imported on first launch; Debian is an optional download
- **What**: The first launch imports the Alpine minirootfs from the APK without asking; Settings can switch to Debian 13 (debuerreotype's `debian:trixie`, the build Docker's official image uses), downloaded only when chosen; Ubuntu is not offered.
- **Why**: The user decided that the app starts in Alpine with no distro dialog, that Ubuntu is dropped from the app, and that the Debian tarball comes from its official source rather than being self-hosted.
- **Tradeoffs**: Debian's 47 MB come from GitHub only, which is very slow from China without a proxy.
- **Source**: rounds 99 (`99-choose-rootfs`), 190 (`190-settings-default-alpine`)

### D2: Tarballs are trusted by pinned digests, whatever the source
- **What**: Version, length and sha256 are fixed in `scripts/fetch-rootfs` and `Rootfs.kt`; several sources are probed and tried in order; interrupted downloads resume with `Range`; a verified tarball is cached until the import succeeds.
- **Why**: With the digest fixed in the app, every source is equally trustworthy; a Debian signature would add no trust beyond the digest the app already carries.
- **Tradeoffs**: Updating a distro means changing the pinned values and shipping a new app.
- **Source**: round 99 (`99-choose-rootfs`); design doc §11.4

### D3: Switching or resetting downloads first and replaces the system only when ready
- **What**: The new tarball is downloaded and verified in the background while the old system keeps running (progress and cancel in Settings and the notification); only then are sessions ended, the store deleted and the new one imported; a failed download leaves the old system, a failed import falls back to the bundled Alpine; reset and switch stay available after a start failure.
- **Why**: The old reset deleted the store first, and a 47 MB download at 18 KB/s left the app unusable for over forty minutes; a broken store (an unreadable `meta.db`) could otherwise only be fixed by clearing app data.
- **Tradeoffs**: After a cancel the old download thread must finish (up to a 30 s timeout) before another switch; a download killed with the app restarts from zero.
- **Source**: round 190 (`190-settings-default-alpine`)

### D4: Mirrors are chosen by measured throughput, and only built-in mirror hosts are rewritten
- **What**: Built-in mirrors are probed concurrently (up to 512 KB or 5 s each) and the fastest is written unless the user picked one; writing replaces only the built-in mirror part of each line, keeping the scheme, other sources and commented lines.
- **Why**: Timing only the first 128 KB could not detect a mirror running at 14 KB/s through a proxy; rewriting the whole file dropped lines users had added (Alpine testing, Debian backports, third-party repositories).
- **Tradeoffs**: Probes use the app's HTTP stack and the system proxy, which guest package managers do not use, so the winner is not always fastest for the guest.
- **Source**: rounds 99 (`99-choose-rootfs`), 190 (`190-settings-default-alpine`), 203 (`203-mirror-keeps-lines`)

### D5: Tabs log in as the first regular user by default, who is set up to become root
- **What**: The login setting defaults to "auto" (the regular account with the lowest UID, else root); the first time a regular user becomes the login account it is added to wheel with doas (Alpine, installed offline from the bundled package) or to sudo (Debian, installed online), and NOPASSWD rules are written only while the account has no password.
- **Why**: The user wanted desktop-like behaviour where `adduser` makes new tabs log in as that user; Alpine's root is locked, so such a user could not `su` back or run `apk add`; the setup follows Alpine's own `setup-user -a`, and the user chose "ask only if a password is set".
- **Tradeoffs**: Only tabs opened by the app run the setup (`su -` and ssh logins do not); "no password" is judged from the shadow field alone.
- **Source**: rounds 193 (`193-login-account`), 206 (`206-debian-sudo-progress`); design doc §11.1

### D6: The login script keeps the start directory and the session's job control
- **What**: `login.sh` switches with `su <account> -c 'cd …; exec "$SHELL" -l'` instead of `su -l`, using `--session-command` when util-linux su offers it; Ctrl-C during the sudo install skips it until the next login; package commands read from `/dev/null`.
- **Why**: `su -l` always goes home, but restored tabs must start in their saved directory; util-linux `su -c` starts a new session, which left the shell without job control and made Ctrl-C close the tab; typing ahead must reach the login shell, not apt.
- **Source**: rounds 194 (`194-restore-tabs`), 206 (`206-debian-sudo-progress`)

### D7: OpenRC gets a loopback interfaces file and osclock, not `rc_provide`
- **What**: Before each start a loopback-only `/etc/network/interfaces` is written when missing; before boot osclock is added to the boot runlevel when OpenRC is installed and no clock service is configured.
- **Why**: Services needing `net` made OpenRC start networking, which failed without the file; `rc_provide` in rc.conf applies to every service, warned about `loopback` and created dependency cycles once `clock` was added; hwclock always fails because appine has no RTC and cannot set the host time.
- **Watch out**: Leave user-provided interfaces files and clock services alone.
- **Source**: round 187 (`187-app-init`)

### D8: One rootfs at a time, upgraded by the user
- **What**: App updates never touch an installed rootfs; upgrades within a branch are the user's `apk upgrade`/`apt upgrade`, across branches by editing the sources; switching distro discards the old files; rootfs export and import are planned for v1.x.
- **Why**: The design doc states the rule without a recorded reason.
- **Tradeoffs**: Two distros cannot be used side by side or share files (that would need mounts across stores).
- **Source**: round 99 (`99-choose-rootfs`); design doc §11.4

### D9: Distributions, first boot, logins and Settings are one android/setup module
- **What**: android/setup covers `Rootfs.kt`, `SettingsActivity.kt`, `login.sh` and `scripts/fetch-rootfs`, separate from android/service.
- **Why**: §11.1's login account and §11.4's first boot and settings needed a home when the design doc was retired.
- **Tradeoffs**: The service drives switches and imports, so those flows span two maps, and D8's reason was never recorded.
- **Watch out**: The map's open bugs include a password set in a running session keeping NOPASSWD until the next tab logs in and a switch interrupted by a kill silently becoming Alpine.
- **Source**: 12c4afa

### D10: The execution tier row exists only in debug builds
- **What**: Settings creates the tier row only when `BuildConfig.DEBUG`, with its strings left in `main`, which references them.
- **Why**: Tier 0 only tells engine bugs from kernel bugs, and in a release build choosing it made everything tens of times slower and could stay on unnoticed.
- **Tradeoffs**: Release users cannot be asked to retry on Tier 0 to locate a reported bug, and the release APK carries six strings it never shows.
- **Watch out**: Where Tier 1 cannot start, a release build has no fallback to Tier 0 (the row was already disabled while the kernel was down, so this is no regression).
- **Source**: 62a8c07

### D11: Filling two cells with CJK is an on-off switch, not a scale
- **What**: Settings offers a toggle row "CJK characters fill two cells" ("中日韩文字占满两格"), usable at any time, with summaries for on and off, rather than a choice of scales.
- **Why**: The user asked for a switch in Settings between filling two cells and the old centring.
- **Tradeoffs**: No intermediate scale.
- **Watch out**: The row speaks of CJK characters, but every wide glyph narrower than two cells is enlarged.
- **Source**: 0478dd5

### D12: Settings is a Material 3 page of grouped rows
- **What**: Rows inflated from `list_item.xml` into a `NestedScrollView` are grouped under System, Android, Terminal, Debugging (debug builds only) and About; toggles show a display-only `MaterialSwitch` presented to accessibility services as a switch, disabled rows are drawn at 38%, every dialog goes through `MaterialAlertDialogBuilder`, and state handling and the service calls are unchanged.
- **Why**: The user asked for the settings screen to be restyled first, and it was a flat list of nine hand-coloured rows with a text arrow for back.
- **Tradeoffs**: The groups and their names are the assistant's choice, not yet confirmed by the user, rows are plain views driven by the service's state rather than Preferences, and the login change stays a toast.
- **Watch out**: The first state jumps into place through `SwitchCompat.jumpDrawablesToCurrentState` ending its thumb animator (read from AppCompat 1.7.1's bytecode), so a later AppCompat could slide the switches on every open again.
- **Source**: 55be399

### D13: Login rules, admin setup, mirror choice and downloads fail closed
- **What**: NOPASSWD rules are refreshed at every login, root's included, and granted only when the account's shadow field after any leading `!` is empty or `*`; an admin is matched with `grep -F` and recorded only once `id -nG` shows the group (else retried at the next login); a failed mirror write leaves the sources and the record alone; and a download fails as soon as a source sends more than the pinned length.
- **Why**: Locked accounts with a password, missing shadow entries and a missing `/etc/shadow` counted as passwordless, root logins never rewrote the rules, failed group changes were recorded, names were matched as regexes, a mirror that failed to write was recorded and fetch had no cap.
- **Tradeoffs**: A password set inside a running session keeps NOPASSWD until the next tab logs in (map debt), and a locked account without a hash still counts as passwordless, as `adduser -D` accounts must.
- **Watch out**: Shadow formats other than Alpine's and Debian's (`!`, `!!`, `*`, `!*`) are judged by the same rule, and the Debian sudo path was not run in a container, only shared with the Alpine path.
- **Source**: 711b034

## Superseded

- ~~The execution tier is chosen in Settings in every build (5c34951, round 190)~~ → replaced by **D10** in 62a8c07 (2026-11-04)
