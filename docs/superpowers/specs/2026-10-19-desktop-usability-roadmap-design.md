# DragOS Desktop Usability — Program Roadmap

**Status:** design (awaiting review)
**Date:** 2026-10-19
**Companions:** per-subsystem specs (this program), starting with
`2026-10-19-mount-subsystem-design.md`.

---

## Goal

Make DragOS usable for *normal desktop use* — sit down, browse the web, edit
documents, view media, manage files, install software — **inside the QEMU/KVM
guest**.

## Target & constraints (decided)

- **Environment:** the QEMU/KVM VM only. This program does *not* target real
  hardware. Wifi, physical GPU drivers, suspend/battery, and firmware loading
  for real chips are explicitly a **separate program** (see *Out of scope*).
- **Networking:** wired VirtIO-net + SLIRP only. No wireless.
- **Graphics:** virtio-gpu / Venus with software GL (llvmpipe); `--gl` optional.
  Performance, not correctness, is the limit.
- **Vendoring strategy:** *hybrid*. The kernel mechanism, session/auth layer,
  package manager, and glue are written **in-tree**. Large third-party apps
  (`dbus-daemon`, PipeWire, GTK, a browser) are **staged as host-built binaries**
  with their runtime `.so` deps — the pattern `scripts/build-fs.sh` already uses
  for `bash`, `sshd`, `curl`, and `mywima`.

## Global constraints (apply to every sub-project)

- Every sub-project ends **green** on `cargo run -p tools -- test`
  (currently 19 pass / 0 fail / 5 skip).
- All `AGENTS.md` invariants hold: lock ordering (§1), interrupt masking (§2),
  one-shot timer rearm (§3), context frame layout (§4), `current_thread()`
  clone-write-back (§5), CR3 discipline (§9), and the userspace ABI rules (§0).
- Host-staged binaries must have their full runtime dependency closure staged
  (use `stage_bin_with_libs`) and must pass the build-fs completeness check at
  the end of `scripts/build-fs.sh`. A binary whose unit exists but whose file is
  missing is a silent boot failure (see the `ptyrun` post-mortem in that script).
- No new runtime network dependency beyond SLIRP; package/browser content is
  staged at build time from the host or a local mirror.
- New kernel code must not add hardcoded path/special-case arms to
  `fs/mod.rs::lookup` — sub-project #1 exists to remove that pattern.
- Test-mode output must reach `COM1` exactly as `AGENTS.md` §0a describes; a new
  program whose output the harness reads needs a `/dev/ttyS0` redirection.

---

## The gap, decomposed

Thirteen sub-projects. Each gets its own spec → plan → implementation cycle and
must ship working, testable software on its own.

| # | Sub-project | Kind | Depends on | Unlocks |
|---|---|---|---|---|
| 1 | `mount(2)`/`umount2` + real `/proc`, `/sys`, `/dev` | kernel | — | removable/secondary storage, installer, app probes |
| 2 | D-Bus bus + kernel IPC (`SO_PEERCRED`, `SO_PASSCRED`, `SCM_RIGHTS`) | kernel + userland | — | nearly every modern desktop app; portals |
| 3 | Session & auth (login/greeter, minimal auth, `sudo`/`polkit`) | userland | 2 (partial) | real user sessions, privilege separation |
| 4 | Terminal/job-control hardening (tmux, zellij) | kernel/userland fix | — | a shell you can live in |
| 5 | CLI toolset + editor (`grep/sed/awk/tar/vi/ps/top`) | build-script | — | terminal productivity |
| 6 | Audio server (PipeWire or ALSA + mixer) + volume | userland | — | media playback |
| 7 | `xdg-desktop-portal` + GTK toolkit | userland | 2 | file choosers, notifications, screenshots |
| 8 | Network userland (DHCP client, config UI) | userland | 2 | user-managed networking |
| 9 | Time (NTP client, `hwclock`, timezone) | userland | — | a correct clock |
| 10 | GUI apps (file manager, text editor, settings) | userland | 7 | a desktop, not just a shell |
| 11 | Package management | userland | 1, 8 | install software at all |
| 12 | Web browser (WebKitGTK/Epiphany) | userland | 2, 6, 7, 8 | the core daily-driver use case |
| 13 | Polish: logging/journal, printing, Bluetooth, real GPU | mixed | — | daily-driver feel |

**Build order:** 1 → 4 → 5 → 2 → 6 → 3 → 7 → 8 → 9 → 10 → 11 → 12 → 13.

Rationale: #1 removes a structural blocker; #4/#5 are cheap wins that make the
system feel usable immediately; #2 is the keystone the desktop stack sits on;
#12 is the capstone and only tractable after its dependencies.

---

## Per-sub-project scope

Each entry states goal, deliverables, success criteria, and the principal risk.
The owning spec expands this; the roadmap fixes the boundaries.

### 1. Mount subsystem — `mount(2)`, `umount2(2)`, a mount table

- **Goal:** a real mount table and the `mount`/`umount2` syscalls, so `/proc`,
  `/sys`, `/dev` are genuinely mounted and a second disk / disk image can be
  mounted.
- **Deliverables:** `Mount` + `MOUNTS`, fstype registry, block-device VNode
  layer, `sys_mount`/`sys_umount2`, `statfs` per mount.
- **Success:** `mount("proc","/proc","proc",0,NULL)` → 0 and `/proc/self`
  resolves; a secondary ext4 image mounts and a sentinel file reads back;
  `umount2` succeeds; busy unmount → `EBUSY`; `mount -t vfat /dev/vda1 /efi` in
  `init.sh` stops being an expected failure.
- **Risk:** touching `fs/mod.rs::lookup`, which every syscall depends on.
  Mitigated by the two-phase plan in the companion spec.
- **Spec:** `2026-10-19-mount-subsystem-design.md`.

### 2. D-Bus + kernel credential passing

- **Goal:** a running session (and system) bus, and the kernel IPC needed to
  support it.
- **Deliverables:** `dbus-daemon` staged with its `.so` closure + a draginit
  unit; kernel support for `SO_PEERCRED`, `SO_PASSCRED`, and `SCM_RIGHTS`
  ancillary fd passing over `AF_UNIX` (currently `net.rs` handles sockets but
  credential/fd-passing ancillary data is unverified); `DBUS_SESSION_BUS_ADDRESS`
  wired into every session.
- **Success:** `dbus-send`/`busctl` round-trips a method call; a client receives
  a `NameOwnerChanged` signal.
- **Risk:** `SCM_RIGHTS` is the hard part — fd passing requires a new
  `sendmsg`/`recvmsg` control-message path. If the current socket layer cannot
  express it, this becomes a larger kernel task.

### 3. Session & auth

- **Goal:** a real login/session instead of a hardcoded daemon set.
- **Deliverables:** a login program (or a light greeter) that authenticates
  against `/etc/shadow` (or a minimal PAM-alike), establishes a session
  (groups, `XDG_RUNTIME_DIR`, seat), and launches the compositor as the user;
  privilege separation via a small `sudo`/`doas` and a `polkit`-equivalent the
  portals can call.
- **Success:** log in as `user`, get a compositor session owned by that user;
  `sudo` re-authenticates; root-owned daemons cannot be signalled by the user.
- **Risk:** inventing a bespoke auth mechanism that later third-party tools
  assume is PAM. Keep the interface PAM-shaped even if the backend is custom.

### 4. Terminal/job-control hardening

- **Goal:** `tmux` and `zellij` run interactive sessions.
- **Deliverables:** fix the SIGTTOU/`TOSTOP` foreground-write path, verify
  signals to a process blocked in a pty `read`, add the missing
  "SIGTERM a blocked reader" test, and add the "interactive tmux/zellij" test.
- **Success:** `tmux new-session` shows a live pane; a background writer gets
  `SIGTTOU`.
- **Risk:** already partially fixed; the residual is the fd-identity/`TOSTOP`
  question in `TODO.md` P0.

### 5. CLI toolset + editor

- **Goal:** a terminal you can work in.
- **Deliverables:** `grep`, `sed`, `awk`, `tar`/`gzip`, `vi` (or `vim`/`nano`),
  `ps`, `top`, `kill`/`killall`, `dmesg` staged into the image (host-built,
  static where possible). A `ps`/`top` backend means a real `/proc` — depends on
  #1.
- **Success:** `grep -r`, `tar xf`, and `vi` all work in-guest.
- **Risk:** `ps`/`top` semantics require `/proc` to expose real per-pid data;
  scope them to what `/proc` can honestly provide.

### 6. Audio server + volume

- **Goal:** media playback with volume control. (The kernel driver and `/dev/snd`
  already exist — this is userland.)
- **Deliverables:** PipeWire (preferred) or a minimal ALSA-only mixer staged;
  a volume control reachable from the session.
- **Success:** `mpv clip.mp4` produces sound and responds to a volume change.
- **Risk:** PipeWire pulls in D-Bus (#2) and a lot of `.so` deps; ALSA-only is
  the fallback if PipeWire proves too heavy on software.

### 7. xdg-desktop-portal + GTK

- **Goal:** the desktop integration every GTK app assumes.
- **Deliverables:** `xdg-desktop-portal` (+ `-gtk` or `-wlr`) staged, a
  file-chooser backend, notification, screenshot, and clipboard portals;
  `GTK`/`GDK` staged for Wayland.
- **Success:** a GTK app opens a native file chooser; a notification appears.
- **Risk:** portals need a session bus (#2) and a seat/session (#3); the
  clipboard portal needs compositor support in `mywima` (external repo).

### 8. Network userland

- **Goal:** user-managed networking rather than a build-time static
  `resolv.conf`.
- **Deliverables:** a DHCP client (or SLIRP-configured static), an interface
  manager usable by the session, and a minimal network config UI.
- **Success:** an interface comes up with a lease; `/etc/resolv.conf` is written
  at runtime.
- **Risk:** NetworkManager is heavy and D-Bus-dependent; a small custom client
  may be more in keeping with the hybrid strategy.

### 9. Time

- **Goal:** a correct clock.
- **Deliverables:** an NTP client (or SLIRP-provided time), `hwclock`, timezone
  data, and a settable system clock from the RTC driver (`pit.rs`/`rtc.rs`
  exist).
- **Success:** time matches the host after boot; `date` reports the right zone.
- **Risk:** low.

### 10. GUI apps

- **Goal:** a desktop, not a shell. File manager, text editor, settings.
- **Deliverables:** a GTK file manager, a GTK text editor, and a settings app,
  all staged, all launched from the compositor; plus a desktop shell/launcher in
  `mywima`.
- **Success:** rename a file in the file manager; edit and save in the editor;
  change the wallpaper from settings.
- **Risk:** depends on #7 and on `mywima` growing launcher/window-management
  affordances.

### 11. Package management

- **Goal:** install software without rebuilding the image.
- **Deliverables:** either a ported `apk` against a local repo, or a signed
  tarball installer written in-tree; a package index; a source of packages
  (in-VM local repo staged at build time).
- **Success:** `apk add <pkg>` (or equivalent) installs and the program runs.
- **Risk:** signature verification needs a working crypto stack (OpenSSL is
  staged for `curl`); dependency resolution is the bulk of the work. YAGNI: a
  signed-tarball installer may be enough for a hobby OS.

### 12. Web browser

- **Goal:** browse the web — the core daily-driver use case.
- **Deliverables:** WebKitGTK + Epiphany (or a minimal WebKitGTK shell) staged
  with its dependency closure, sandbox helpers, certificate store, and fonts.
- **Success:** load a real page over SLIRP; follow a link; type into a field.
- **Risk:** WebKitGTK is the heaviest staged component (D-Bus, portals, GTK,
  GL, sandboxing, network #8). It depends on 2, 6, 7, 8. Performance on llvmpipe
  may be poor but should be functional.

### 13. Polish

- **Goal:** daily-driver feel.
- **Deliverables:** a logging/journal story, printing, Bluetooth (likely N/A in
  the VM), and a real GPU path (deferred; see *Out of scope*).
- **Success:** per item; none blocks "normal usage."
- **Risk:** scope creep. Do this last.

---

## Dependencies (graph)

```
1 (mount) ────────────► 10 (proc-based ps/top in #5)
4 (tmux/zellij)  independent
5 (cli)          independent
2 (dbus) ──► 3 (session) ──► 7 (portals) ──► 10 (gui apps) ──► 12 (browser)
2 (dbus) ─────────────► 6 (audio, if PipeWire)
2 (dbus) ──► 8 (network) ─────────────────► 12 (browser)
6 ─────────────────────────────────────► 12 (browser)
1 (mount), 8 (net) ──► 11 (packages)
9 (time)         independent
```

---

## Cross-cutting decisions

- **Session model.** One interactive user (`root` today; `user` as #3 lands).
  The compositor is a per-session process launched by the session layer, not by
  `draginit` directly. `seatd` remains the seat broker.
- **D-Bus is load-bearing.** #2 is a prerequisite for portals, NetworkManager-
  style networking, PipeWire, and every modern GUI app. It is deliberately
  placed early.
- **Package format.** Prefer in-tree signed tarballs over porting a distro
  package manager until proven otherwise (YAGNI).
- **`mywima` is external.** Desktop-shell features (#10) may require changes in
  `/home/jyash8/Projects/mywima`; that repository is not managed here. Each such
  sub-project must state which side its changes live on.

---

## Out of scope (separate program: "DragOS on real hardware")

- Wifi / `wpa_supplicant`.
- Real GPU drivers (amdgpu/i915) and firmware loading for physical chips.
- Suspend/resume, battery, backlight, thermal.
- Secure Boot / real EFI disk installation.

---

## Review focus

Inputs and conditions this program will meet which no single task's tests cover:

1. **A user program that mounts over an existing mount point** (e.g. mounting
   `/dev` while `/dev` already exists) — Linux stacks it; the incremental table
   must not silently drop the old mount.
2. **`..` traversal across a mount point** — must stay within the mount on
   which the path began, not escape to the parent filesystem.
3. **A filesystem whose backing device disappears** (removed disk) — reads must
   return `EIO`, not fault.
4. **D-Bus client disconnecting mid-fd-pass** — the kernel must not leak the
   passed fd.
5. **A GUI app launched before the session bus exists** — must fail cleanly, not
   hang the session.

Each line gets a test in the task that owns the corresponding code.

---

## Self-review

- **Placeholder scan:** no TBD/TODO; every sub-project has scope + success.
- **Consistency:** the order in *Build order* matches the dependency graph.
- **Scope:** 13 independent subsystems — decomposed with one spec/plan each, as
  the process requires.
- **Ambiguity:** "normal usage" is fixed to the QEMU desktop scenario; real
  hardware is explicitly out of scope.
