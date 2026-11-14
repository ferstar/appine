# Rootfs, settings and login (android/setup) Decisions

> Snapshot of current consensus. Evolution: `git log --grep="MODULE: android/setup"`
>
> Last distilled: 2026-11-15 (HEAD = 221a89b1). D1–D8 were seeded by hand from the retired design doc
> §11.1 and §11.4 and the round records in `docs/rounds/`, so their sources cite rounds; later
> entries cite commits.

## Constraint index (compressed active decisions)

> Full text of D10, D11, D15, D23: `git show 221a89b12e1dffa7b1432b9076078c1bffde46cb:.agents/decisions/android/setup.md`

- **D10** — Every build exposes execution mode in Debugging with a distinct current-mode summary and restart confirmation, disables it without a running kernel and persists Tier 0 when chosen; switching ends sessions and device layout/switching remain unverified (62a8c07; cf4f5e20; 20137e4c)
- **D11** — CJK enlargement is an always-available on/off toggle with state summaries and no intermediate scale, affecting every narrow-rendered wide glyph rather than only CJK characters (0478dd5; 75123eef (shared labels and default))
- **D15** — Rootfs uses KTX edit with commit=true for synchronous import records, while standard Preferences replace handwritten toggle accessibility; TalkBack behavior remains unverified (4c1a3ce; f8822b8b; 20137e4c)
- **D23** — The always-available default-on volume-key tab-switch setting forwards to the service with English/Chinese summaries, has no modifier mode and falls back to system volume without another tab; device/localization coverage remains limited (61df6915; 20137e4c)

## Active (full detail)

### D1: Alpine is bundled and imported on first launch; Debian is an optional download
- **What**: The first launch imports the Alpine minirootfs from the APK without asking; Settings can switch to Debian 13 (debuerreotype's `debian:trixie`, the build Docker's official image uses), downloaded only when chosen; Ubuntu is not offered.
- **Why**: The user decided that the app starts in Alpine with no distro dialog, that Ubuntu is dropped from the app, and that the Debian tarball comes from its official source rather than being self-hosted.
- **Tradeoffs**: Debian's 47 MB come from GitHub only, which is very slow from China without a proxy.
- **Source**: rounds 99 (`99-choose-rootfs`), 190 (`190-settings-default-alpine`)

### D2: Tarballs are trusted by pinned digests, whatever the source
- **What**: Pinned version, length and sha256 validate tarballs from ordered sources; resume continues on 206, restarts on 200 and fails the source without resetting on other statuses, while a failed attempt already at the full length proceeds to verification.
- **Why**: With the digest fixed in the app, every source is equally trustworthy; a Debian signature would add no trust beyond the digest the app already carries.
- **Tradeoffs**: Distro updates require new pins, transient resume errors can exhaust Debian’s sole source, and partial files do not survive the download call.
- **Watch out**: A non-200 reply to an unsupported Range ends the source, and only the final hash checks the 206 payload’s correctness.
- **Source**: round 99 (`99-choose-rootfs`); design doc §11.4; 30d0fab3; 20137e4c

### D3: Switching or resetting downloads first and replaces the system only when ready
- **What**: The new tarball is downloaded and verified in the background while the old system keeps running (progress and cancel in Settings and the notification); only then are sessions ended, the store deleted and the new one imported; a failed download leaves the old system, a failed import falls back to the bundled Alpine; reset and switch stay available after a start failure.
- **Why**: The old reset deleted the store first, and a 47 MB download at 18 KB/s left the app unusable for over forty minutes; a broken store (an unreadable `meta.db`) could otherwise only be fixed by clearing app data.
- **Tradeoffs**: After a cancel the old download thread must finish (up to a 30 s timeout) before another switch; a download killed with the app restarts from zero.
- **Source**: round 190 (`190-settings-default-alpine`)

### D4: Mirrors are measured once after import and only an untouched template is replaced
- **What**: The post-import throughput probe selects a mirror and rewrites sources from the import template only if the file still exactly matches that template; manual mirror settings and records are no longer used.
- **Why**: Changing mirrors in a running system could overwrite user repositories and leave manual/measured state inconsistent.
- **Tradeoffs**: There is no later in-app mirror choice or retry, a failed first probe leaves official sources, and app HTTP/proxy measurements may differ from guest performance.
- **Watch out**: The compare and write are not atomic against a guest edit, old preference keys remain unread, and edited or deleted source files must remain untouched.
- **Source**: rounds 99 (`99-choose-rootfs`), 190 (`190-settings-default-alpine`), 203 (`203-mirror-keeps-lines`); a1367531; 20137e4c

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
- **Why**: Services needing `net` made OpenRC start networking, which failed without the file; `rc_provide` in rc.conf applies to every service, warned about `loopback` and created dependency cycles once `clock` was added; hwclock always fails because Appine has no RTC and cannot set the host time.
- **Watch out**: Leave user-provided interfaces files and clock services alone.
- **Source**: round 187 (`187-app-init`)

### D8: One rootfs at a time, upgraded by the user
- **What**: App updates do not upgrade installed distribution packages; Appine-owned per-start integration files may be refreshed (D20, D30); upgrades within a branch are the user's `apk upgrade`/`apt upgrade`, across branches by editing the sources; switching distro discards the old files; rootfs export and import are planned for v1.x.
- **Why**: The design doc states the rule without a recorded reason.
- **Tradeoffs**: Two distros cannot be used side by side or share files (that would need mounts across stores).
- **Source**: round 99 (`99-choose-rootfs`); design doc §11.4; 8e211142

### D9: Distributions, first boot, logins and Settings are one android/setup module
- **What**: android/setup covers Rootfs, SettingsActivity/SettingsFragment, login, shell-integration and browser-launcher assets, and fetch-rootfs, while service lifecycle stays in android/service.
- **Why**: §11.1's login account and §11.4's first boot and settings needed a home when the design doc was retired.
- **Tradeoffs**: The service drives imports, android/privacy owns authentication, android/licenses owns notices navigation, android/terminal owns the shared page frame, android/vtcore parses prompt marks and android/vt owns the login environment.
- **Watch out**: The 2026-11-12 full verification corrected import/login behavior and recorded mirror defects subsequently removed by import-only selection; in-place shadow edits can still delay NOPASSWD refresh, and shell-hook/device checks remain open.
- **Source**: 12c4afa (bug list since updated by 237d663, d03c9b6); 5d506bfb; a1367531; 7aa817af; f8822b8b; 20137e4c; 8e211142; 9c6c51a7; 81f60bef

### D12: Settings uses service-driven AndroidX Preferences in a Material page
- **What**: SettingsActivity supplies the frame and targets the preference RecyclerView for app-bar lift, while SettingsFragment builds stable-key nonpersistent categories, switches and lists driven by service setters, discards stale account loads and restores scrolling only after state and insets are ready.
- **Why**: Standard preferences replace custom row/accessibility code, and an explicit scrolling target prevents preference updates from resetting app-bar lift.
- **Tradeoffs**: Service rendering and destructive confirmations stay explicit, standard preference spacing replaces custom rows, and Material list dialogs retain AndroidX selection/recreation behavior.
- **Watch out**: Choice dialogs use AndroidX’s deprecated target-fragment contract, lift depends on its RecyclerView ID, restoration must await bottom padding and service state, and real-device/TalkBack behavior remains unverified.
- **Source**: 55be399; f8822b8b; dd200589; 20137e4c

### D13: Login rules, admin setup, mirror choice and downloads fail closed
- **What**: NOPASSWD rules are refreshed at every login, root's included, and granted only when the account's shadow field after any leading `!` is empty or `*`; an admin is matched with `grep -F` and recorded only once `id -nG` shows the group (else retried at the next login); an import-only mirror write leaves edited sources alone (D4); and a download fails as soon as a source sends more than the pinned length.
- **Why**: Locked accounts with a password, missing shadow entries and a missing `/etc/shadow` counted as passwordless, root logins never rewrote the rules, failed group changes were recorded, names were matched as regexes, a mirror that failed to write was recorded and fetch had no cap.
- **Tradeoffs**: A password set inside a running session keeps NOPASSWD until the refresh of D19 runs, or until the next login when nothing in `/etc` was renamed, and a locked account without a hash still counts as passwordless, as `adduser -D` accounts must.
- **Watch out**: Shadow formats other than Alpine's and Debian's (`!`, `!!`, `*`, `!*`) are judged by the same rule, and the Debian sudo path was not run in a container, only shared with the Alpine path.
- **Source**: 711b034; d03c9b6 (the refresh, D19); a1367531; 20137e4c

### D14: Rootfs and Settings read in the order a start uses them
- **What**: `Rootfs` runs types and constants, the store, the import, mirror probing and writing, the per-start files, boot, accounts, then downloads and reset; `SettingsActivity` now supplies the frame and `SettingsFragment` owns binding, preferences and actions (D12).
- **Why**: Rootfs interleaved import, mirror, login and download code, unlike the order a start uses them.
- **Tradeoffs**: The texts earlier profiles are matched against (`UNMARKED_PROFILE`, `OLD_PROFILE`, `REPLACED_PROFILES`) sit apart from the code that writes the profile, each next to its user.
- **Watch out**: `OLD_PROFILE` is built from `UNMARKED_PROFILE` and must stay after it.
- **Source**: a546fea; 6c5f921 (the profile moved to an asset); f8822b8b; 20137e4c

### D16: Guest messages name the app Appine; prefixes and paths stay lowercase
- **What**: The motd says "Welcome to Appine", and login.sh says "Appine tries again at the next login" and writes "# Written by Appine" into the NOPASSWD rules; the "appine:" prefixes, `/var/lib/appine`, `/var/cache/appine`, `30-appine.conf`, `sudoers.d/appine` and `profile.d/appine.sh` are unchanged.
- **Why**: The motd and the login script's messages named the app in lowercase.
- **Tradeoffs**: Messages read "appine: ... Appine tries again"; the rules file is rewritten at every login, so the new comment needs no migration.
- **Watch out**: The motd is written only at import, so an installed rootfs keeps "Welcome to appine" until it is reset or switched.
- **Source**: 3f04bf5 (with android/terminal D28)

### D17: An import records its distro first and keeps the tarball until the store is in place
- **What**: `prepare` commits `KEY_DISTRO` for the app's store before importing and deletes the tarball only after the rename and directory sync, and `Rootfs.importing` returns that record (Alpine when there is none) for a launch without a store (android/service D16).
- **Why**: The distro was recorded only just before the store was renamed into place and the tarball was deleted right after importing, so a kill midway left nothing saying which distro was being imported and could lose the download.
- **Tradeoffs**: A kill between deleting the old store and `prepare` starting still resumes the old distro; the cache holds the tarball a little longer during configure and sync.
- **Watch out**: Fallback prepare records Alpine before importing it, so a failed Debian download is not retried at every later storeless launch; the pre-prepare deletion window still resumes the previously recorded distro.
- **Source**: 237d663; 5d506bfb (recorded correction); a1367531; 20137e4c

### D18: Rootless podman uses the host network, and container storage keeps the packages' defaults
- **What**: Before every start (android/service D17) `Rootfs.ensureContainersConfig` writes `containers.rootless.conf.d/appine.conf` (`netns = "host"`) when missing and nothing else, so podman's overlay driver and docker's overlayfs snapshotter, the packages' defaults, run on the kernel's overlayfs rootful and rootless.
- **Why**: Without TUN, pasta and slirp4netns cannot start for an ordinary user, and users had to find and edit the right files themselves; once the kernel had overlayfs, forcing vfs and the native snapshotter only cost every container a full copy of its image.
- **Tradeoffs**: Rootless containers share the host's network; a store set up by an earlier build keeps its vfs `storage.conf` and native `daemon.json` (no migration, as the user asked for an unreleased app), and on Alpine the package's own `storage.conf` then sits in `storage.conf.apk-new`.
- **Watch out**: A distribution whose podman or docker package defaults to another driver, or a future default needing overlay features Appine lacks (redirects, metacopy, idmapped layers), would fail without the old fallback.
- **Source**: 0eb9e19, 869de68

### D19: A guest change to `/etc` rewrites the NOPASSWD rules without a login
- **What**: `login.sh --refresh-rules` only rewrites the rules, written beside the file and renamed into place so a login and a refresh at once never leave a half-written file, and `Rootfs.dirsChanged` spawns it as root, without a console and without waiting, whenever `/etc` is among the changed directories the service's poll passes on (android/service D20).
- **Why**: A password set in a running session did not stop NOPASSWD escalation until another tab logged in.
- **Tradeoffs**: Up to one poll (2 s) of delay, and any rename in `/etc`, not only of shadow and group, runs a refresh.
- **Watch out**: Changes that rename nothing in `/etc` (an in-place edit) or fall past the 256-directory limit between polls still wait for the next login (map debt), and changed directories are recorded only from the service's first take.
- **Source**: d03c9b6

### D20: Profile migration installs companion prompt hooks without overwriting user edits
- **What**: Recognized unmodified profiles migrate by text/hash, install bashrc/ashrc companions, integrate bash through the profile and Alpine’s system bashrc, expose ash hooks through an unset ENV, and wrap zsh prompts with precmd/preexec hooks.
- **Why**: Prompt features need marks after user startup files and in supported nested shells, while new Alpine logins use bash.
- **Tradeoffs**: Edited profiles and user ENV win, doas -s clears ash’s ENV, Debian nested bash and zsh remain unintegrated, and later PS1/PROMPT_COMMAND/theme changes can remove marks; fish 4 and nushell supply their own marks (android/vt D5).
- **Watch out**: Companion-only edits do not migrate existing stores without a profile change and recorded hash; zsh percent escapes/right or multiline prompts and broader device prompt gestures remain limited or unverified.
- **Source**: 6c5f921, 2b04a43; a7d8c9f5; a809c5a8; 7aa817af; 20137e4c

### D21: Settings' Terminal group chooses the cursor's shape and blinking
- **What**: The Terminal group gains "Cursor shape" (Block, Bar or Underline in a single-choice dialog, the summary naming the current one) and "Blinking cursor" (a toggle, on by default), both usable at any time and forwarding to the service (android/service D22), with English and Chinese strings.
- **Why**: The cursor setting needs rows in Settings.
- **Tradeoffs**: No hollow block (the view already draws a block hollow without the focus), and two more rows.
- **Watch out**: The Chinese strings were not seen on a device, and the groups' names are still the assistant's choice (D12).
- **Source**: 77d7f36

### D22: Settings is recreated on a display size change
- **What**: `SettingsActivity` leaves `density` out of `configChanges`, so a display size change recreates it as a dark-mode change does.
- **Why**: Settings handled `density` itself, so a display size change left its views at their old sizes, as it did the terminal's (android/terminal D50).
- **Tradeoffs**: Login/cursor ListPreference dialogs now survive recreation through AndroidX, while other dialogs still close.
- **Watch out**: None beyond the existing recreation path, which rebinds and renders again.
- **Source**: e12f560; f8822b8b; 20137e4c

### D24: New Alpine imports install bundled bash with serialized package setup

- **What**: Alpine imports cache four pinned bash packages, select bash for root and install offline under `/run/appine-install.lock`, preserving existing systems and shell fallback while serializing bash/doas and waiting for apk/apt locks.
- **Why**: New systems need full bash compatibility without network access or package contention breaking first login.
- **Tradeoffs**: The APK gains about 0.86 MB and first-login work, existing users keep their shells, and apt update may wait until interrupted.
- **Watch out**: Retain a usable fallback and retry cache on failure, refresh disappearing Alpine pins, and keep network sudo installation outside the bundled-install lock.
- **Source**: 7aa817af; 20137e4c

### D25: Settings summaries keep the selected value and move operational detail to confirmations

- **What**: Distribution choices omit sizes, automatic login shows the resolved username with root fallback, and reset summaries omit the distribution name.
- **Why**: Users asked for shorter descriptions while retaining meaningful current state.
- **Tradeoffs**: The initial chooser no longer previews download size or explains account selection.
- **Watch out**: Switch confirmations must retain download and deletion details.
- **Source**: b2f96a63; 20137e4c

### D26: Screen-off policy is a service-backed choice list

- **What**: A nonpersistent ListPreference replaces the keep-awake switch and uses the shared Material choice dialog with the service’s SLEEP, BUSY and ALWAYS policy descriptions.
- **Why**: Settings must expose three policies and stay synchronized with notification toggles.
- **Tradeoffs**: Changing policy opens a list, and BUSY needs its idle-timeout limitations explained.
- **Watch out**: Vendor presentation and accessibility remain unverified.
- **Source**: a3f7507e, 1e5de64e; 20137e4c

### D27: New Alpine imports stage an independent offline procps-ng bundle

- **What**: Imports cache pinned procps-ng dependencies separately from bash, install before account selection under the install lock using apk’s package database, re-add procps-ng by name and remove the cache only after success.
- **Why**: New systems need procps tools offline regardless of the selected shell.
- **Tradeoffs**: Existing stores are not migrated and first login performs another transaction.
- **Watch out**: Failed bundles must survive for retry, later user removal must stay respected, and disappearing upstream packages require refreshed pins.
- **Source**: 410ef192; 20137e4c

### D28: About actions work independently of service binding

- **What**: About opens the project HTTPS URL through unpinned Custom Tabs and shows BuildConfig.VERSION_NAME with a Google Play link that tries com.android.vending before an unrestricted HTTPS fallback.
- **Why**: Users need discoverable project and installed-version links without a running service.
- **Tradeoffs**: Routing depends on installed handlers and a Play listing may not exist before publication.
- **Watch out**: Keep the missing-handler toast on both actions; device navigation, fallback routing and return to Settings remain unverified.
- **Source**: d0a74e94, 0b3c5d8a

### D29: New Alpine imports stage an independent bubblewrap bundle

- **What**: Imports cache SHA-256-pinned bubblewrap 0.12.0-r0 and libcap2 2.78-r0, install before account selection under the shared lock, check apk state, re-add bubblewrap by name and delete the cache only on success.
- **Why**: New Alpine systems need offline sandbox tooling without depending on the bash or procps bundle.
- **Tradeoffs**: Existing stores receive no automatic installation and first login gains another package transaction.
- **Watch out**: Retain retry state without blocking usable login, respect later user removal and refresh pins when mirrors remove old builds.
- **Source**: 0815b3e4

### D30: Every kernel start refreshes the guest browser client and configuration

- **What**: Before each kernel start, setup installs the static client and current endpoint configuration, replacing only the known Appine OSC prototype while preserving custom launchers and symlinks.
- **Why**: Existing stores and long-lived shells need current browser connection information.
- **Tradeoffs**: Guest users share the capability and preserved custom launchers keep responsibility for their own behavior.
- **Watch out**: Future migrations must distinguish Appine-owned contents from user edits.
- **Source**: 8e211142

### D31: Enabling external file access always asks for confirmation

- **What**: The file-access switch renders service state, saves true only from a Material confirmation on every enable and disables immediately.
- **Why**: Users need an explicit explanation before home and system files become available to other apps.
- **Tradeoffs**: Every enable needs another tap and recreation discards an unaccepted confirmation.
- **Watch out**: Physical-device and accessibility behavior remain unverified; privacy mode disables this row (D32).
- **Source**: d0ddf170

### D32: Privacy changes require authentication and protected dialogs

- **What**: A nonpersistent privacy switch confirms enabling before authentication, authenticates disabling, protects Settings dialogs and replays deferred notices after unlock.
- **Why**: Users need an explicit control that cannot silently remove authentication or conflict with external file access.
- **Tradeoffs**: Both transitions require device authentication and disabling privacy does not restore file access.
- **Watch out**: Cancellation or unavailable authentication must leave the preference unchanged, and new dialogs must use the secure-window helpers.
- **Source**: f61072f3, 9c6c51a7

### D33: Log export prepares one authorized archive before the save picker

- **What**: Save to file and Share generate a ZIP after authorization, with a single-use pending save held in a fragment ViewModel and deleted on cancellation, abandonment or completion.
- **Why**: Users need direct log export without moving authentication after destination selection.
- **Tradeoffs**: A prepared ZIP stays in private cache during the picker, and process death discards authorization instead of resuming it.
- **Watch out**: Provider failures may leave partial destination files; cloud providers and slow transfers remain untested.
- **Source**: c7309c0c, 9c6c51a7

## Superseded

- ~~The execution tier is chosen in Settings in every build (5c34951, round 190)~~ → replaced by **D10** in 62a8c07 (2026-11-04)
- ~~`ensureContainersConfig` also chooses vfs in `/etc/containers/storage.conf` (rewriting an active `driver = "overlay"` line) and the native snapshotter in `/etc/docker/daemon.json` (0eb9e19)~~ → replaced by **D18** in 869de68 (2026-11-08)
- ~~A password set inside a running session keeps NOPASSWD until the next tab logs in (D13's tradeoff, 711b034, and D9's watch-out, 12c4afa)~~ → replaced by **D19** in d03c9b6 (2026-11-09)
- ~~`OLD_PROFILE` is built from `PROFILE`, the profile kept as a Kotlin string (D14's watch-out, a546fea)~~ → replaced by **D20** in 6c5f921 (2026-11-10)
- ~~D4’s manual mirror selection and rewriting of built-in hosts in existing sources~~ → replaced by **D4** in a1367531 (2026-11-12)
- ~~D10’s debug-only execution-tier row~~ → replaced by **D10** in cf4f5e20 (2026-11-13)
- ~~D12’s inflated custom Settings rows and hand-written switch accessibility~~ → replaced by **D12** in f8822b8b (2026-11-13)
- ~~D15’s custom toggle accessibility compat wrapper~~ → replaced by **D12** in f8822b8b (2026-11-13, implicit refinement)
- ~~D20’s ash marks written only by the login profile~~ → replaced by **D20** in a809c5a8 (2026-11-13, existing-entry match; the cited a7d8c9f5 Decision describes zsh)
- ~~D20’s bash hooks kept inline in the login profile~~ → replaced by **D20** in 7aa817af (2026-11-13, implicit refinement)
- ~~The keep-awake switch introduced by a3f7507e~~ → replaced by **D26** in 1e5de64e (2026-11-13, predecessor predates structured Decision records)
- ~~The immediate system-directory toggle summarized under D12~~ → replaced by **D31** in d0ddf170 (2026-11-14)
