# P1 — Dynamic Input Device Model Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the static two-node input model with per-physical-device evdev nodes created/destroyed at runtime, plus a netlink uevent broadcast so userspace (libinput) hotplugs instead of one-shot coldplug.

**Architecture:** Keep the existing per-open FIFO fan-out in `kernel/src/drivers/keyboard.rs` (`INPUT_DEV`, `input_push`, `input_register`). Replace the fixed `INPUT_DEV_KEYBOARD/INPUT_DEV_MOUSE` pair with a dynamic table keyed by minor number. Sysfs moves from `const DEVICES` to a runtime registry fed by driver attach/detach. A tiny `KOBJECT_UEVENT` broadcast (NETLINK protocol 15) wakes a new `udevd`-equivalent — which, per YAGNI, is NOT a daemon: libinput re-scans on a `EPOLL` wakeup from a uevent listener socket opened by mywima itself.

**Tech Stack:** Rust `no_std` kernel, existing `SpinLock`, `AF_NETLINK` socket infra in `kernel/src/arch/x86_64/syscalls/net.rs`, sysfs in `kernel/src/fs/sysfs.rs`, mywima `/home/jyash8/Projects/mywima/src/backends/kms.rs`.

---

### File map

- Modify: `kernel/src/drivers/keyboard.rs` — dynamic device table (`INPUT_DEV: SpinLock<BTreeMap<usize, InputDev>>`), minor allocator, keep `input_push(ev, dev)` signature.
- Modify: `kernel/src/fs/sysfs.rs` — replace `const DEVICES: &[CharDev]` with `SYSFS_REGISTRY: SpinLock<Vec<CharDev>>` + `sysfs_register(dev) / sysfs_unregister(major, minor)`; `root()` and `dev_dir()` unchanged otherwise.
- Modify: `kernel/src/fs/mod.rs:156-161` — path matcher currently hardcodes `event0/event1`; resolve any `dev/input/eventN` to minor `64+N`.
- Modify: `kernel/src/arch/x86_64/syscalls/fsio.rs:518-612` (`input_evdev_ioctl`) — capability bitmaps must come from the device record, not from `dev == INPUT_DEV_MOUSE` branches. Store `ev_bits/key_bits/rel_bits/abs_bits` + name in the registry entry at register time.
- Create: `kernel/src/drivers/uevent.rs` — `broadcast(action: &str, devpath: &str, env: &[(&str,&str)])`; netlink group buffer + wake waiters on port 15.
- Modify: `kernel/src/arch/x86_64/syscalls/net.rs` — verify `bind()` + `recvfrom()` on protocol 15 (`NETLINK_KOBJECT_UEVENT`); add a kernel-side sender queue if `sendto` to kernel-originated multicast isn't already routed. Check `net.rs:528` (protocol-15 branch) and `net.rs:1645` (`is_netlink` recv path) first.
- Modify: `kernel/src/drivers/usb/xhci.rs` — on `enumerate_port` success call `input_alloc_node(name, caps)` instead of assuming dev 0/1; on hotplug disconnect call `input_free_node`. Same for PS/2 in `kernel/src/drivers/ps2_mouse.rs:init` and `kernel/src/interrupts.rs` keyboard init path.
- Modify: `/home/jyash8/Projects/mywima/src/backends/kms.rs` — open a `AF_NETLINK/SOCK_DGRAM/15` socket, add to the event loop; on datagram, call `libinput_context.resume()` + force re-scan (libinput picks up new nodes on resume) instead of relying on boot-time coldplug.

---

### Task 1: Dynamic device table in keyboard.rs

**Files:**
- Modify: `kernel/src/drivers/keyboard.rs:20-54`

- [ ] **Step 1: Replace the fixed array with a map**

```rust
// before:
pub const INPUT_DEV_KEYBOARD: usize = 0;
pub const INPUT_DEV_MOUSE: usize = 1;
pub const INPUT_DEV_COUNT: usize = 2;
pub static INPUT_DEV: [SpinLock<InputDev>; INPUT_DEV_COUNT] = [...];

// after:
pub static INPUT_DEV: SpinLock<alloc::collections::BTreeMap<usize, InputDev>> =
    SpinLock::new(alloc::collections::BTreeMap::new());
static NEXT_MINOR: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

pub struct InputCaps {
    pub name: &'static str,
    pub ev: u64,        // bitmask: bit0=SYN, bit1=KEY, bit2=REL, bit3=ABS
    pub key: [u8; 96],  // 768-bit KEY bitmap (covers BTN_* at 272+)
    pub rel: u8,        // REL_X|REL_Y|REL_WHEEL bits
}

pub fn input_alloc_node(name: &'static str, caps: InputCaps) -> usize {
    let minor = NEXT_MINOR.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    INPUT_DEV.lock().insert(minor, InputDev::new());
    INPUT_CAPS.lock().insert(minor, caps);
    minor
}
```

Keep `INPUT_DEV_KEYBOARD`/`INPUT_DEV_MOUSE` as constants during migration (shim: allocate them first at boot in the same order so minor 0=kbd aggregate, 1=mouse aggregate), delete them in Task 4.

- [ ] **Step 2: Update `input_push/input_pop/input_pending/input_register/input_unregister`** to `.get(dev)` on the map (most already go through `INPUT_DEV.get(dev)` — verify each; `time.rs` stats loop must iterate the map instead of indexing `[0]/[1]`).
- [ ] **Step 3: Build**

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -2`
Expected: `Kernel deployed`, no errors.

- [ ] **Step 4: Commit**

```bash
git add kernel/src/drivers/keyboard.rs
git commit -m "feat(input): dynamic evdev device table with minor allocator"
```

---

### Task 2: Runtime sysfs registry

**Files:**
- Modify: `kernel/src/fs/sysfs.rs:24-74, 258-304`

- [ ] **Step 1: Replace `const DEVICES` with a registry**

```rust
pub static SYSFS_REGISTRY: SpinLock<Vec<CharDev>> = SpinLock::new(Vec::new());

pub fn sysfs_register(dev: CharDev) { SYSFS_REGISTRY.lock().push(dev); }
pub fn sysfs_unregister(major: u32, minor: u32) {
    SYSFS_REGISTRY.lock().retain(|d| !(d.major == major && d.minor == minor));
}
```

Seed the registry at boot with the current six entries (move the `DEVICES` initializers into a `sysfs_init()` called from `kernel/src/lib.rs` device init). `root()` iterates `SYSFS_REGISTRY` instead of `DEVICES`. The `input0/input1` parent dirs and `dev_dir()` stay as-is.

- [ ] **Step 2: Generalize `/dev` path matching in `kernel/src/fs/mod.rs:156-161`**

```rust
// before: hardcoded event0/event1/event aliases
// after:
if let Some(n) = clean.strip_prefix("dev/input/event") {
    if let Ok(minor) = n.parse::<u32>() {
        return input node for major 13, minor 64 + minor;
    }
}
```

- [ ] **Step 3: Build + boot smoke**

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1 && cargo run -p tools -- build-fs 2>&1 | tail -1`
Expected: both succeed.
Run headless 100s, check serial: `tr -d '\0' < /tmp/opencode/smoke.log | grep -a "input-stats" | head -3`
Expected: `kbd(readers=1)` present (no behavior change yet).

- [ ] **Step 4: Commit**

```bash
git add kernel/src/fs/sysfs.rs kernel/src/fs/mod.rs
git commit -m "feat(input): runtime sysfs device registry, generic eventN matching"
```

---

### Task 3: Uevent broadcast (netlink protocol 15)

**Files:**
- Create: `kernel/src/drivers/uevent.rs`
- Modify: `kernel/src/arch/x86_64/syscalls/net.rs` (protocol-15 paths)

- [ ] **Step 1: Inspect the existing netlink recv path** at `net.rs:528` and `net.rs:1645`. Confirm what `bind(fd, AF_NETLINK, protocol=15)` + `recvfrom` currently do. If protocol-15 sockets can already receive from a kernel queue, skip to Step 3.
- [ ] **Step 2: Minimal kernel sender**

```rust
// kernel/src/drivers/uevent.rs
static UEVENT_QUEUE: SpinLock<VecDeque<Vec<u8>>> = SpinLock::new(VecDeque::new());

pub fn broadcast(action: &str, devpath: &str, props: &[(&str, &str)]) {
    let mut msg = alloc::format!("{}@{}\0ACTION={}\0DEVPATH={}\0", action, devpath, action, devpath);
    for (k, v) in props {
        msg.push_str(k); msg.push('='); msg.push_str(v); msg.push('\0');
    }
    UEVENT_QUEUE.lock().push_back(msg.into_bytes());
    crate::tasks::wake_netlink_waiters(15); // reuse/extend the existing waiter mechanism
}
```

Wire delivery into the protocol-15 `recvfrom` path so a bound userspace socket dequeues these. Message format is the standard `action@devpath\0KEY=val\0...` netlink-uevent payload (no netlink header — kobject uevents are raw datagrams).

- [ ] **Step 3: Emit on register/unregister** — call `broadcast("add", "/devices/virtual/input/inputN", &[("SUBSYSTEM","input"),("ID_INPUT","1"),...])` from `input_alloc_node`, `"remove"` from free.
- [ ] **Step 4: Build**

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1`
Expected: success.

- [ ] **Step 5: Commit**

```bash
git add kernel/src/drivers/uevent.rs kernel/src/arch/x86_64/syscalls/net.rs kernel/src/drivers/keyboard.rs
git commit -m "feat(input): kobject uevent broadcast on input node add/remove"
```

---

### Task 4: Per-device nodes for real drivers + mywima rescan

**Files:**
- Modify: `kernel/src/drivers/usb/xhci.rs` (`enumerate_port`, hotplug disconnect), `kernel/src/drivers/ps2_mouse.rs:init`, keyboard PS/2 init, `kernel/src/arch/x86_64/syscalls/fsio.rs:518-612`, `/home/jyash8/Projects/mywima/src/backends/kms.rs`

- [ ] **Step 1: Allocate nodes per physical device.** USB keyboard → `input_alloc_node("dragos-usbkbd", kbd_caps)`, store minor in `UsbDevice` (replace `dev: usize` 0/1 constants in `push_event` calls). PS/2 kbd → its own node, PS/2 mouse → its own node. Delete the `INPUT_DEV_KEYBOARD/MOUSE` shims once no references remain (`grep -rn INPUT_DEV_KEYBOARD kernel/src` must be empty).
- [ ] **Step 2: Caps-driven ioctls.** `input_evdev_ioctl` reads bitmaps from `INPUT_CAPS[dev]` instead of `dev == MOUSE` branches. Event0-compat: keep names `dragos-input`/`dragos-mouse` only until mywima handles new names (it matches nothing by name — verify with `grep -rn "dragos" ~/Projects/mywima/src`).
- [ ] **Step 3: mywima uevent listener.** Open `AF_NETLINK/SOCK_DGRAM/15`, bind, add fd to the calloop event loop in `kms.rs`; on readable datagram, drain it and re-run device discovery (libinput udev backend picks up new `/sys` nodes on `resume()`; if not, drop and recreate the `Libinput` context — measure which works, keep the working one).
- [ ] **Step 4: Full rebuild + interactive verification**

Run: `cargo run -p tools -- build-kernel 2>&1 | tail -1 && cd ~/Projects/mywima && cargo build --release 2>&1 | tail -1 && cd ~/Projects/drag_os && cargo run -p tools -- build-fs 2>&1 | tail -1`
Expected: all succeed.
Interactive run: unplug/replug USB mouse (QEMU: detach/reattach `usb-mouse`), expect `[input] DeviceAdded` for the new node without compositor restart, and `mouse(readers=1)` in `[input-stats]`.

- [ ] **Step 5: Commit**

```bash
git add kernel/src/drivers/usb/xhci.rs kernel/src/drivers/ps2_mouse.rs kernel/src/arch/x86_64/syscalls/fsio.rs
git commit -m "feat(input): per-physical-device evdev nodes with hotplug"
```

---

## Self-review

- Spec coverage: dynamic nodes (Tasks 1,4), sysfs registry (Task 2), uevent broadcast (Task 3), userspace rescan (Task 4 Step 3). Each P1 requirement has a task.
- No placeholders: all steps name exact files/functions; Step 3.1 is an inspection step with explicit fallback.
- Type consistency: `input_push(ev, dev: usize)` signature preserved throughout; minor==map key==eventN-64 invariant stated once and reused.
