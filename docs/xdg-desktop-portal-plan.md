# XDG Desktop Portal for DragOS — plan

## What the XDG Desktop Portal is

The XDG Desktop Portal is the **mediated interface between unprivileged
applications and the desktop**. It is a D-Bus service (or a set of them) that
sits between an app and the desktop-specific machinery the app is not allowed
to touch itself.

The canonical implementation is `xdg-desktop-portal` (a generic front end)
plus a *backend* (`xdg-desktop-portal-gtk`, `-hyprland`, `-kde`, …) that
provides the desktop-specific parts. Toolkits and applications only ever talk
to the front end, so the same app works on any desktop that ships one.

### Its core responsibilities

1. **Mediation and privilege separation.** An app asks the portal to do
   something on its behalf — open a file, show a screen, inject input, read
   the user's files. The portal runs outside the app's sandbox and with the
   desktop's authority. The app never receives a privileged handle: it gets a
   *response* (a URI, a stream fd, a device identifier) that is scoped to what
   the user consented to.

2. **User consent.** The portal is where "may this application use the
   camera / see my screen / open that file?" is asked and answered. The
   desktop's own UI shows the prompt; the app only sees the result.

3. **A stable, desktop-agnostic API surface.** The `org.freedesktop.portal.*`
   D-Bus interfaces are the contract. Code written against them (GTK's
   `GtkFileChooserNative`, a browser's screen-sharing button, flatpak's
   `--talk-name=org.freedesktop.portal.Desktop`) works on any desktop that
   implements the portals. This is what makes a "desktop" portable.

4. **A sandbox-friendly boundary.** Under flatpak/snap the app is confined;
   the portal is not. The app only ever holds a narrow, revocable handle
   (a document-portal path, a PipeWire fd, a settings read token), so the
   confinement stays meaningful.

5. **One-time tokens instead of credentials.** Where a portal hands out
   something that must not be reusable (a settings token, a device
   identifier), the portal issues a token that the app passes back to the
   *backend* service. The backend only accepts requests carrying a token it
   issued, so an app cannot talk to the backend directly and cannot replay
   the token for something else.

### The interfaces in the specification

From the upstream API reference: the `org.freedesktop.portal.*` interfaces an
app may call, and the `org.freedesktop.impl.portal.*` interfaces a *backend*
must implement.

| Portal (app side) | Backend (desktop side) | What it is for |
|---|---|---|
| `FileChooser` | `FileChooser` | open/save files, with the desktop's own dialog |
| `Documents` | - | the document portal's file access (see below) |
| `OpenURI` | - | open a URI with the desktop's default handler |
| `Screenshot` | `Screenshot` | capture the screen or a window |
| `ScreenCast` | `ScreenCast` | share a screen/window as a video stream |
| `RemoteDesktop` | `RemoteDesktop` | inject input and pick devices |
| `InputCapture` | `InputCapture` | capture input devices |
| `Notification` | `Notification` | show a notification, with actions |
| `Device` / `Camera` | - | camera / microphone / speaker access |
| `Secret` | `Secret` | store a secret (the keyring) |
| `Settings` | `Settings` | read/write desktop settings, with per-app consent |
| `GlobalShortcuts` | `GlobalShortcuts` | register global hotkeys |
| `Inhibit` | `Inhibit` | inhibit the screensaver or idle |
| `Trash` | - | move a file to the trash |
| `Location` | - | location access |
| `Account` | `Account` | the user's online accounts |
| `Clipboard` | `Clipboard` | read/write the clipboard |
| `Background` | `Background` | run a background application |
| `DynamicLauncher` | `DynamicLauncher` | register a .desktop entry |
| `FileTransfer` | - | transfer a file to a sandboxed app |
| `GameMode` | - | ask for a gaming mode |
| `MemoryMonitor`, `NetworkMonitor`, `PowerProfileMonitor`, `ProxyResolver`, `Realtime`, `Usb`, `Wallpaper`, `Print`, `Email` | several | the long tail |

Two conventions every portal shares:

* `org.freedesktop.portal.Request` -- the response signal, carrying either a
  result or an error, delivered on a per-request object path;
* `org.freedesktop.portal.Session` -- a group of related requests (a
  screen-cast session, a remote-desktop session) that is closed as a unit.

And the backend-only interfaces the *front end* uses, which are the token
mechanism the spec is built on:

* `org.freedesktop.impl.portal.Access` -- the front end asks the backend for
  access to a resource and gets back a **one-time token**; the app then
  passes the token to the *backend* service, never to the front end. The
  token is what makes the app unable to reach the privileged service
  directly, and unable to reuse the grant for anything else.
* `org.freedesktop.impl.portal.PermissionStore` -- the per-app consent
  database ("this app may read this key").
* `org.freedesktop.impl.portal.Lockdown` -- the desktop's lockdown state (no
  screen sharing, no location, ...), which the front end enforces.

Two things in the spec are *not* the portal service itself:

* the **document portal** (`xdg-document-portal`) -- a separate executable
  that implements a **FUSE** filesystem, mounted at `/run/user/<uid>/doc`
  (bind-mounted at `/run/flatpak/doc` inside a flatpak sandbox). It exposes
  *host* files that the user has granted to the app; the app writes into the
  document it was given (the grant is the write permission), and the file
  chooser is what produces the grant. The hierarchy is by *domain* (root,
  by-app, per-app, per-document), and each exposed file carries a
  `user.document-portal.host-path` xattr with the real path.
* the **backend** -- the privileged part that actually does the work and
  holds the one-time tokens.

## What DragOS has today, and what is missing

Measured against the tree:

| Prerequisite | State |
|---|---|
| D-Bus in the kernel or the guest | **absent** — no D-Bus anywhere |
| `AF_UNIX` `SOCK_STREAM` | present (`net.rs`), **path-based only** — the abstract namespace is not implemented, so the bus address must be `unix:path=/run/user/<uid>/bus` |
| `SO_PEERCRED` | present (`net.rs:1769`) — D-Bus's `EXTERNAL` authentication works |
| `flock` | present |
| `fcntl(F_SETLK/F_GETLK)` | **missing** — dbus-daemon uses it to lock the bus address |
| `pipe2` | **missing** — to be confirmed whether dbus-daemon needs it |
| `readv`/`writev`/`sendmsg`(`SCM_RIGHTS`) | present — the fd passing the portals rely on works |
| `mmap`, `prctl`, `setrlimit`, `clock_nanosleep` | present |
| a bus daemon in the guest | not staged — the host has `/usr/bin/dbus-daemon` and `scripts/build-fs.sh` already has `stage_bin_with_libs` for exactly this |
| a compositor that can be asked things | mywima exists, but there is no channel between a service and the compositor |

## The plan

### Phase 0 — the session bus

* **Kernel**: implement `fcntl(F_SETLK, F_GETLK)` on the bus socket (the
  advisory lock dbus-daemon takes on the bus address). Everything else
  dbus-daemon needs is already there.
* **Image**: stage `dbus-daemon` with `stage_bin_with_libs`; add a draginit
  unit that starts it with
  `--address=unix:path=/run/user/1000/bus --nofork`; export
  `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus` to the other units
  (the same `WAYLAND_DISPLAY=/run/wayland-1` pattern the compositor already
  uses, because the abstract namespace is not available).
* **Exit criterion**: `dbus-send --session --dest=org.freedesktop.DBus
  /org/freedesktop/DBus org.freedesktop.DBus.ListNames` answers from inside
  the guest.

### Phase 1 — a minimal portal service

A new crate, `userspace/xdg-portal`, in the same shape as `draginit`: a
supervised process that owns the bus name `org.freedesktop.portal.Desktop`.

* **D-Bus client**: hand-rolled, not `zbus`. `zbus` drags in tokio and the
  async runtime, which is a lot of binary for a guest that has to stay small
  (the mywima dependency-size invariant). What the portal needs is small and
  well-specified: connect, `Hello`, `RequestName`, method calls with
  `a{sv}`/`o`/`s`/`u`/`b` marshalling, and the `Response`/`Error` signal
  handshake. That is a few hundred lines, and it keeps the guest's footprint
  honest.
* **The cheap portals first** — the ones that need no compositor help:
  * `OpenURI` — hand the URI to the desktop's handler (mywima already knows
    how to spawn a terminal; the "open with" mapping can start as a small
    table).
  * `Inhibit` — a flag file the compositor reads (screensaver/idle).
  * `Settings` — a JSON file under `~/.config/mywima/portal-settings.json`,
    with the per-app consent the spec asks for (a "which apps may read this
    key" list).
  * `Trash` — move to `~/.local/share/Trash/files` + `.trashinfo`.
  * `Secret` — a file-based keyring under `~/.local/share/mywima/keyring`.
  * `Notification` — needs the compositor to draw it, so it waits for Phase 2.
* **The shape of every portal** is the same: validate the options, ask the
  user (through the compositor in Phase 2, or with a simple built-in prompt
  in Phase 1), then answer with the `Response` signal carrying the result and
  a token where the spec wants one.

### Phase 2 — the channel to the compositor

The portal has to ask mywima to do things that only the compositor can do
(ask the user, take a screenshot, inject input). The cheapest channel is a
second Unix socket next to the Wayland one, with a small length-prefixed
request/response protocol — the same "path-based socket" constraint as the
bus applies, and it avoids inventing a Wayland protocol for it.

The requests the compositor answers:

* `ask_user(prompt, options) -> id` — show a dialog, return the answer. This
  is the gate every other portal goes through, so it is the first thing to
  build. mywima would draw the dialog as an overlay (it already draws a
  status bar and a clock, so it has the text-to-texture path).
* `screenshot(output) -> pixels` — mywima re-renders its output into a
  buffer and returns it. It already has the GBM/GLES path, so this is a
  read-back of a frame it has just drawn.
* `inject_input(event)` — for `RemoteDesktop`; mywima already has the
  libinput plumbing, so this synthesises an event and pushes it through the
  same path a real device uses.

### Phase 3 — `Screenshot`, `Notification`, `GlobalShortcuts`

These need only the compositor channel:

* `Screenshot` — the portal asks for a screenshot, shows it to the user with
  a crop/confirm dialog, and answers with the image URI.
* `Notification` — the portal asks the compositor to show a toast, with the
  action buttons the spec requires.
* `GlobalShortcuts` — the portal registers a shortcut with the compositor,
  which then delivers it to the app.

### Phase 4 — `ScreenCast` and `RemoteDesktop` (the hard ones)

Both of these hand the app a **PipeWire** stream; that is the specification,
and an app (a browser's "share my screen") expects a PipeWire fd it can
hand to libpipewire.

* Porting PipeWire into the guest is a large piece of work: the daemon, the
  protocol, and a `libpipewire` the app can link against. It is the single
  biggest item in this plan and the reason `ScreenCast`/`RemoteDesktop` are
  last.
* Two ways out, to be decided:
  * **implement enough PipeWire** for the video path (the portal is the
    producer, the app the consumer) — a "PipeWire-compatible-enough" server
    plus a stub `libpipewire` in the guest; or
  * **ship a different mechanism** (a raw frame socket) and accept that the
    portal is not interoperable with the reference implementation for these
    two interfaces only.
* `RemoteDesktop` also needs the input injection of Phase 2, which is the
  smaller half.

### Phase 5 — the document portal

`FileChooser` under a sandbox needs the document portal: a FUSE filesystem
that gives the app a private directory, and the chooser hands one of its
files to the user.

* DragOS has no FUSE, so this means implementing a FUSE server in the kernel
  (or a `fusermount3` shim over a userspace daemon, which needs `/dev/fuse`
  — also not present).
* Until then, `FileChooser` can return plain `file://` URIs. That works for
  an unsandboxed app (the normal case in the guest today) and not for a
  flatpak app, which is exactly the gap the document portal fills.

## The order, and why

1. **The bus** — nothing else can be tested without it, and it is the
   smallest change (one kernel syscall plus a staged binary).
2. **The portal service and the cheap portals** — this is where the value
   is: `OpenURI`, `Settings`, `Trash`, `Secret` and `Inhibit` are what a
   normal desktop application actually asks for first, and none of them need
   the compositor.
3. **The compositor channel and the user-consent dialog** — the portal is
   only as good as its consent flow; without this, the portals that matter
   (`FileChooser`, `Notification`) cannot exist.
4. **`Screenshot` / `Notification` / `GlobalShortcuts`** — with the channel
   in place these are small.
5. **`ScreenCast` / `RemoteDesktop`** — gated on PipeWire.
6. **The document portal** — gated on FUSE.

## Comparison with the Linux implementation

Checked against the upstream documentation and the reference implementation
(`xdg-desktop-portal` plus a backend such as `-gtk`, `-hyprland` or
`-kde`):

| Aspect | Linux | This plan | Agrees? |
|---|---|---|---|
| Transport | D-Bus on the *session* bus, the front end owns `org.freedesktop.portal.Desktop` | the same | yes |
| Split | a generic front end plus a desktop-specific *backend*; the front end owns the user interaction, the backend owns the privileged work | mywima is both compositor and backend, so one service can be both -- but the two roles should stay separate in the code | with a caveat |
| Consent | the front end asks the user, then asks the backend for access and gets a one-time token; the app talks to the backend with the token | the same, with the token minted by the portal service | yes |
| Settings | `Settings` + `PermissionStore` + `Lockdown` for per-app consent and the desktop's lockdown state | `Settings` with a per-app consent list, a lock-down flag | yes |
| File access | `FileChooser` + the **document portal** (a FUSE filesystem at `/run/user/<uid>/doc`, host files granted by the user) | `FileChooser` returning `file://` URIs; the document portal needs FUSE, which DragOS does not have | **no** -- the documented gap |
| Screen sharing | `ScreenCast` hands the app a **PipeWire** fd | the same, which is why PipeWire is the schedule risk | yes (and the risk) |
| Remote control | `RemoteDesktop` (PipeWire video + an input fd) | the same, plus compositor-side input injection | yes (and the risk) |
| Notifications | `Notification` with actions, delivered by the desktop | the same, via the compositor channel | yes |
| The long tail | `Clipboard`, `DynamicLauncher`, `FileTransfer`, `GameMode`, `MemoryMonitor`, `NetworkMonitor`, `PowerProfileMonitor`, `ProxyResolver`, `Realtime`, `Usb`, `Wallpaper` | not planned; they need no compositor work, so they are "just D-Bus" once the service exists | deliberately out of scope |

**The one structural difference** is that on Linux the *front end* and the
*backend* are separate processes, and the backend is the one that holds the
privileged resources. On DragOS the compositor *is* the backend, so the portal
service can talk to it directly. The token discipline still has to be
implemented, because the *app* is the untrusted party: the app must never be
able to name the backend's service directly.

**The two things Linux has that DragOS does not** are the FUSE-based document
portal and PipeWire, and they are the two places where "implement the
portal" turns into "port a large project". Everything else is D-Bus plumbing
plus a bit of compositor work.

## Risks

* **PipeWire and FUSE are the schedule risks.** Both are large, and both are
  in the *spec* rather than in the code. If they are not wanted, the honest
  answer is that DragOS implements the portal *except* for `ScreenCast`,
  `RemoteDesktop` and the sandboxed `FileChooser` — and says so.
* **The bus needs `fcntl` locks** and the guest has no udev, so the bus
  address has to be a filesystem path everywhere. Every D-Bus-using app has
  to be told that (`DBUS_SESSION_BUS_ADDRESS`), which is one more environment
  variable for the guest's daemons to get right.
* **The D-Bus client should be hand-rolled** to keep the guest small, but
  that means owning the marshalling and the error handling. A bug there
  shows up as an app that silently gets no answer — so the test suite needs
  a portal client of its own to pin the protocol down.
