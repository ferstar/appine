# Rootfs, settings and login (android/setup) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/setup"`
>
> Last distilled: 2026-11-03 (HEAD = c3df5d3) — seeded by hand from the retired design doc §11.1 and
> §11.4 and the round records in `docs/rounds/`; no `MODULE: android/setup` Decision blocks exist
> yet, so sources cite rounds instead of commits.

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
