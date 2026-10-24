use alloc::collections::{BTreeMap, VecDeque};
use crate::arch::sync::spinlock::SpinLock;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct InputEvent {
    pub time_sec: u64,
    pub time_usec: u64,
    pub type_: u16,  // EV_KEY = 1
    pub code: u16,   // Linux Key Code
    pub value: i32,  // 1 = press, 0 = release
}

// ponytail: boundary counters to tell flood vs stall vs break:
// pushed/dropped at producers, consumed at drain. Printed by timer tick.
pub static INPUT_PUSHED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static INPUT_DROPPED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static INPUT_CONSUMED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static INPUT_OPENED: [core::sync::atomic::AtomicU64; 2] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];
pub static INPUT_CLOSED: [core::sync::atomic::AtomicU64; 2] = [
    core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];
// ponytail: TEMP-DIAG PS/2 scancode count (IRQ1 bytes). Pairs with the USB
// KBD_REPORT_* counters so [input-stats] shows which keyboard path is alive.
pub static PS2K_IRQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Per-open event queue depth. A reader that falls this far behind (a grab
/// path re-rendering on every motion event is the realistic way to do it)
/// starts losing motion samples rather than growing without bound.
pub const INPUT_QUEUE_DEPTH: usize = 256;

// ponytail: Linux-like per-device, per-open evdev streams, resolved from the
// open path against the live INPUT_DEV table, never from globals, so a second
// opener (e.g. seatd) can no longer steal libinput's events. Each open() gets
// its own INPUT_QUEUE_DEPTH-deep FIFO; pushes fan out to all of them. Keyed by
// the OpenFile Arc pointer, so dup()/fork() share the stream like a shared file
// offset.
//
// A key CAN go stale: the close paths unregister only when the Arc is the last
// reference, and an fd handed over via SCM_RIGHTS overwrites its slot without
// unregistering the displaced OpenFile. `input_push` therefore treats a grab
// whose key is no longer registered as dead — it logs `[input-ungrab]` and
// falls back to fan-out rather than dropping every event on the floor.
//
// Minors 0/1 are the PS/2 keyboard/mouse's own nodes (allocated first at
// boot); USB/tablet devices take 2+. Aggregates are gone: every push targets
// exactly one physical device's node.
pub static PS2_KBD_DEV: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);
pub static PS2_MOUSE_DEV: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

pub struct InputDev {
    pub readers: BTreeMap<usize, VecDeque<InputEvent>>,
    pub key_state: [u8; 96],
    pub abs_vals: [i32; 8],
    pub led_state: u8,
    pub grabbed_by: Option<usize>,
}

impl InputDev {
    const fn new() -> Self {
        Self {
            readers: BTreeMap::new(),
            key_state: [0u8; 96],
            abs_vals: [0i32; 8],
            led_state: 0,
            grabbed_by: None,
        }
    }
}

// ponytail: dynamic evdev table — minors allocated at boot (0=kbd, 1=mouse),
// later by hotplug. One outer lock keeps the park-path try_lock discipline
// (single try, never blocking) unchanged.
pub static INPUT_DEV: SpinLock<BTreeMap<usize, InputDev>> =
    SpinLock::new(BTreeMap::new());

static NEXT_MINOR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

// ponytail: informational caps per node (evdev bitmaps); fsio.rs serves
// EVIOCGBIT and EVIOCGABS straight from these.
#[derive(Debug, Clone, Copy)]
pub struct InputCaps {
    pub name: &'static str,
    pub ev: u64,
    pub key: [u8; 96],
    pub rel: u8,
    pub abs: u8,
    pub abs_info: [(i32, i32, i32, i32, i32, i32); 8],
}

pub static INPUT_CAPS: SpinLock<BTreeMap<usize, InputCaps>> = SpinLock::new(BTreeMap::new());

// ponytail: one constructor per physical kind so PS/2 init, USB attach, and
// boot agree on bitmaps; fsio.rs serves EVIOCGBIT straight from these.
pub fn kbd_caps(name: &'static str) -> InputCaps {
    let mut key = [0u8; 96];
    for code in 1..=248usize {
        key[code / 8] |= 1 << (code % 8);
    }
    InputCaps {
        name,
        ev: 0x03,
        key,
        rel: 0,
        abs: 0,
        abs_info: [(0, 0, 0, 0, 0, 0); 8],
    }
}

pub fn mouse_caps(name: &'static str) -> InputCaps {
    let mut key = [0u8; 96];
    for code in [272usize, 273, 274] {
        key[code / 8] |= 1 << (code % 8);
    }
    // ponytail: rel:u8 covers REL 0-7, REL_WHEEL=8 is implied by nonzero rel.
    InputCaps {
        name,
        ev: 0x07,
        key,
        rel: 0x03,
        abs: 0,
        abs_info: [(0, 0, 0, 0, 0, 0); 8],
    }
}

pub fn tablet_caps(name: &'static str, max_x: i32, max_y: i32) -> InputCaps {
    let mut key = [0u8; 96];
    for code in [272usize, 273, 274] {
        key[code / 8] |= 1 << (code % 8);
    }
    let mut abs_info = [(0, 0, 0, 0, 0, 0); 8];
    abs_info[0] = (0, 0, max_x, 0, 0, 0); // ABS_X
    abs_info[1] = (0, 0, max_y, 0, 0, 0); // ABS_Y
    // EV_SYN (0x01) | EV_KEY (0x02) | EV_REL (0x04) | EV_ABS (0x08) = 0x0F
    InputCaps {
        name,
        ev: 0x0F,
        key,
        rel: 0x03, // compat relative deltas
        abs: 0x03, // ABS_X (bit 0) and ABS_Y (bit 1)
        abs_info,
    }
}

pub fn input_alloc_node(caps: InputCaps) -> usize {
    let minor = NEXT_MINOR.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    INPUT_DEV.lock().insert(minor, InputDev::new());
    INPUT_CAPS.lock().insert(minor, caps);
    // ponytail: kobject add-event for udev-style listeners; tags mirror the
    // sysfs.rs uevent strings, picked off rel (pointer axes imply the
    // mouse/pointer tags). Queue lock is taken+dropped inside broadcast.
    let devpath = alloc::format!("/devices/virtual/input/input{minor}/event{minor}");
    let minor_s = alloc::format!("{}", 64 + minor);
    let devname = alloc::format!("input/event{minor}");
    let mut props: [(&str, &str); 8] = [
        ("SUBSYSTEM", "input"),
        ("MAJOR", "13"),
        ("MINOR", minor_s.as_str()),
        ("DEVNAME", devname.as_str()),
        ("ID_INPUT", "1"),
        ("ID_INPUT_KEYBOARD", "1"),
        ("ID_INPUT_MOUSE", "1"),
        ("ID_INPUT_POINTER", "1"),
    ];
    // ponytail: keyboard nodes carry all ID tags (props[..8]) to mirror
    // sysfs event0; pointer nodes swap slot 5 for MOUSE+POINTER (props[..7]).
    let n = if caps.rel != 0 || caps.abs != 0 {
        props[5] = ("ID_INPUT_MOUSE", "1");
        props[6] = ("ID_INPUT_POINTER", "1");
        7
    } else {
        8
    };
    crate::drivers::uevent::broadcast("add", &devpath, &props[..n]);
    minor
}

// ponytail: no callers yet — hotplug (P1T4) calls this on detach. Drops the
// map+caps entries, then broadcasts remove so listeners can pair add/remove.
pub fn input_free_node(minor: usize) {
    INPUT_DEV.lock().remove(&minor);
    INPUT_CAPS.lock().remove(&minor);
    let devpath = alloc::format!("/devices/virtual/input/input{minor}/event{minor}");
    let minor_s = alloc::format!("{}", 64 + minor);
    let devname = alloc::format!("input/event{minor}");
    let props: [(&str, &str); 4] = [
        ("SUBSYSTEM", "input"),
        ("MAJOR", "13"),
        ("MINOR", minor_s.as_str()),
        ("DEVNAME", devname.as_str()),
    ];
    crate::drivers::uevent::broadcast("remove", &devpath, &props);
}

/// Device index from an event-node path, resolved against the live table:
/// `eventN` → minor N iff allocated, else None (→ ENOENT, never an alias).
/// The legacy `dev/input/events` aggregate keeps pointing at the PS/2
/// keyboard node so pre-dynamic userspace keeps working.
pub fn input_dev_of(path: &str) -> Option<usize> {
    if path.ends_with("dev/input/events") || path.ends_with("/events") {
        let d = PS2_KBD_DEV.load(core::sync::atomic::Ordering::Relaxed);
        return if d == usize::MAX { None } else { Some(d) };
    }
    let digits = path
        .rsplit("event")
        .next()
        .filter(|_| path.contains("event"))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: usize = digits.parse().ok()?;
    if INPUT_DEV.lock().contains_key(&n) { Some(n) } else { None }
}

pub fn input_register(key: usize, dev: usize) {
    // ponytail: TEMP-DIAG opener pid — same pid twice = one process
    // double-opening (resume storm); different pids = two compositors.
    // Pid read before the map lock (lock order: CPUS released first).
    // Remove after root cause.
    let opener = crate::tasks::current_thread()
        .map(|t| t.process.lock().pid)
        .unwrap_or(0);
    let mut map = INPUT_DEV.lock();
    if let Some(d) = map.get_mut(&dev) {
        d.readers.insert(key, VecDeque::new());
        // ponytail: TEMP-DIAG open churn (tablet reader vanishing = the
        // mouse-death signature). Remove after root cause.
        crate::dprintln!("[input-open] dev={} key={:#x} readers={} pid={}", dev, key, d.readers.len(), opener);
        // ponytail: TEMP-DIAG per-node open counts — separates "libinput
        // never opened event1" (opens1=0) from "opened then rejected"
        // (opens1>0, readers back to 0). Sampling readers=0 can't tell.
        if let Some(c) = INPUT_OPENED.get(dev) {
            c.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub fn input_unregister(key: usize) {
    let mut map = INPUT_DEV.lock();
    for (i, d) in map.iter_mut() {
        if d.grabbed_by == Some(key) {
            // Log the release: a grab outliving its opener is the failure mode
            // that used to deafen a device, so it needs to be visible.
            crate::dprintln!("[input-ungrab] dev={} key={:#x} reason=unregister", i, key);
            d.grabbed_by = None;
        }
        if d.readers.remove(&key).is_some() {
            // ponytail: TEMP-DIAG close churn (pair with input-open).
            crate::dprintln!("[input-close] dev={} key={:#x} readers={}", i, key, d.readers.len());
            if let Some(c) = INPUT_CLOSED.get(*i) {
                c.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

pub fn input_push(mut ev: InputEvent, dev: usize) {
    if ev.time_sec == 0 && ev.time_usec == 0 {
        let ticks = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed);
        let ns = crate::time::monotonic_nanos(ticks);
        ev.time_sec = (ns / 1_000_000_000) as u64;
        ev.time_usec = ((ns % 1_000_000_000) / 1_000) as u64;
    }

    push_locked(ev, dev);

    // Wake *after* releasing INPUT_DEV. `input_push` runs from IRQ handlers
    // (PS/2, xHCI) with interrupts already disabled by the spinlock; taking
    // SCHEDULER while holding INPUT_DEV inverted the documented order
    // (AGENTS.md §1: producers take the data queue, push, release it, and only
    // then take SCHEDULER) and blocked the whole input path for the duration
    // of the scheduler's critical section — which itself spans process.lock()
    // and a full thread dispatch. On a busy compositor that is a stall the
    // reader cannot make progress through.
    //
    // Safe to do unconditionally: the enqueue happened-before the release of
    // INPUT_DEV, and a parker's re-check reads the queue under INPUT_DEV, so a
    // wake that lands before the park is caught by the register-then-recheck
    // in smp.rs's Input branch.
    crate::tasks::wake_input_waiters();
    crate::tasks::wake_pipe_waiters();
}

/// The body of `input_push`, run with INPUT_DEV held.
fn push_locked(ev: InputEvent, dev: usize) {
    let mut map = INPUT_DEV.lock();
    let Some(d) = map.get_mut(&dev) else { return };

    // Update shadow state
    if ev.type_ == 1 /* EV_KEY */ {
        let code = ev.code as usize;
        if code < 768 {
            if ev.value != 0 {
                d.key_state[code / 8] |= 1 << (code % 8);
            } else {
                d.key_state[code / 8] &= !(1 << (code % 8));
            }
        }
        // ponytail: Ctrl+Alt+F10 (any ctrl + any alt) self-NMIs the CPU for
        // the vector-2 hang dump. QMP `send-key` reaches here even with
        // userspace wedged; the NMI handler only try_locks, so firing with
        // INPUT_DEV held can't deadlock it. Repeat events (val=2) ignored.
        if ev.value == 1 && code == 68 /* KEY_F10 */ {
            let down = |c: usize| d.key_state[c / 8] & (1 << (c % 8)) != 0;
            if (down(29) || down(97)) && (down(56) || down(100)) {
                crate::arch::x86_64::apic::lapic_self_nmi();
            }
        }

        // Ctrl+Alt+F1..F8 switch virtual terminals, as on Linux.
        //
        // This used to be refused outright whenever the console was in
        // KD_GRAPHICS, on the reasoning that a compositor owns the framebuffer
        // then. That made VT switching unreachable in exactly the situation it
        // exists for — a running compositor is precisely when you want to get
        // to a text console.
        //
        // Linux resolves this by having the VC layer *take the console back*
        // when you activate a VT it owns, putting the console into KD_TEXT.
        // `take_console_back_from_graphical` does that half. The other half —
        // breaking the DRM master's lease so the compositor knows to stop — is
        // not implemented, so a compositor that keeps submitting flips will
        // overwrite the text console. See docs/tty-status.md.
        //
        // Known deviation: the key event still reaches userspace, whereas
        // Linux's VC layer consumes it. Consuming it means not pushing the
        // event, which changes what every reader of evdev sees.
        if ev.value == 1 && (59..=66).contains(&code) {
            // KEY_F1 = 59 … KEY_F8 = 66
            let vt = (code - 59 + 1) as u16;
            let down = |c: usize| d.key_state[c / 8] & (1 << (c % 8)) != 0;
            let ctrl = down(29) || down(97); // KEY_LEFTCTRL / KEY_RIGHTCTRL
            let alt = down(56) || down(100); // KEY_LEFTALT / KEY_RIGHTALT
            if ctrl && alt {
                let graphical = crate::graphics::vt::GRAPHICAL_VT
                    .load(core::sync::atomic::Ordering::Relaxed);
                // Only reclaim the console when the target VT is a different
                // one from the graphical client's — switching to the VT the
                // compositor already owns should not yank the console back and
                // fight it pointlessly.
                let reclaim = graphical != 0 && graphical != vt;
                if crate::graphics::vt::switch_vt(vt) {
                    if reclaim {
                        let dropped =
                            crate::graphics::vt::take_console_back_from_graphical();
                        crate::dprintln!(
                            "[VT] Ctrl+Alt+F{vt} -> VT {vt} (took console back from VT {dropped})"
                        );
                    } else {
                        crate::dprintln!("[VT] Ctrl+Alt+F{vt} -> VT {vt}");
                    }
                    // Repaint: the incoming VT's cells have never been on
                    // this framebuffer, so a switch has to draw them.
                    crate::graphics::terminal::WRITER.lock().repaint_all();
                }
            }
        }

        // Ctrl+Alt+F12 dumps the active VT's cell buffer to the debug log.
        //
        // A diagnostic, not a feature: "did the full-screen program actually
        // draw anything?" has no answer without it. The screen is normally
        // owned by a compositor, `screendump` reports no surface for a virgl
        // scanout, and a full-screen program renders into the VT's cells and
        // nowhere else — so its output is otherwise invisible to a headless
        // run. F12 is unused (VT switching stops at F8).
        //
        // 88, not 87: in the evdev table KEY_F1..KEY_F10 are 59..68, then the
        // numbering jumps and KEY_F11 is 87. Guessing 87 silently binds the
        // dump to F11, which is how the first attempt appeared to do nothing.
        if ev.value == 1 && code == 88 /* KEY_F12 */ {
            let down = |c: usize| d.key_state[c / 8] & (1 << (c % 8)) != 0;
            if (down(29) || down(97)) && (down(56) || down(100)) {
                let active = crate::graphics::vt::active_vt_number();
                crate::graphics::vt::debug_dump(active);
            }
        }
    } else if ev.type_ == 3 /* EV_ABS */ {
        let code = ev.code as usize;
        if code < 8 {
            d.abs_vals[code] = ev.value;
        }
    }

    if d.readers.is_empty() {
        // No opener yet (pre-seat boot); same as Linux with nobody reading.
        crate::dprintln!("[input-drop] dev={} ev_type={} code={} val={}", dev, ev.type_, ev.code, ev.value);
        INPUT_DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return;
    }
    if ev.type_ == 1 /* EV_KEY */ {
        crate::dprintln!("[input-push] dev={} ev_key code={} val={} readers={}", dev, ev.code, ev.value, d.readers.len());
    }
    INPUT_PUSHED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    // Deliver to the grabber if the grab is live, else fan out to every reader.
    //
    // The grabber is keyed by the opener's Arc address, which can go stale
    // (a dup that outlives close, or an fd handed over via SCM_RIGHTS that
    // overwrote the slot). A stale key must NOT swallow the event: falling
    // through to fan-out keeps the device usable, and the one-shot log makes
    // the stale grab visible instead of leaving the device silently deaf.
    let mut live_grabber = None;
    if let Some(grabber) = d.grabbed_by {
        if d.readers.contains_key(&grabber) {
            live_grabber = Some(grabber);
        } else {
            crate::dprintln!("[input-ungrab] dev={} key={:#x} reason=stale-grabber", dev, grabber);
            INPUT_DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            d.grabbed_by = None;
        }
    }

    if let Some(grabber) = live_grabber {
        if let Some(q) = d.readers.get_mut(&grabber) {
            if q.len() < INPUT_QUEUE_DEPTH {
                q.push_back(ev);
            } else {
                INPUT_DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
    } else {
        for q in d.readers.values_mut() {
            if q.len() < INPUT_QUEUE_DEPTH {
                q.push_back(ev);
            } else {
                INPUT_DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

pub fn input_pop(key: usize, dev: usize) -> Option<InputEvent> {
    let ev = INPUT_DEV.lock().get_mut(&dev)?.readers.get_mut(&key)?.pop_front();
    if ev.is_some() {
        INPUT_CONSUMED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
    // ponytail: TEMP-DIAG. The consumer reads from a queue selected by the
    // OpenFile Arc pointer (`key`), which is only stable while that Arc is the
    // slot's occupant. If the slot is displaced -- dup/fork, or an fd handed
    // over via SCM_RIGHTS, which installs into the slot without unregistering
    // the OpenFile it displaces -- then this key resolves to nothing while
    // events pile up under the *old* key. The reader is woken, pops None, and
    // re-parks: a livelock, not a lost wakeup.
    //
    // The distinguishing signature is exactly this: the key we were given is
    // not registered for this dev, AND some other reader's queue for this dev
    // is non-empty. Report it, because "queue full, reader registered,
    // consumed frozen" looks identical from [input-stats] alone.
    {
        let m = INPUT_DEV.lock();
        let registered = m.get(&dev).map(|d| d.readers.contains_key(&key)).unwrap_or(false);
        if !registered {
            let backlog: usize = m
                .get(&dev)
                .map(|d| d.readers.values().map(|q| q.len()).sum())
                .unwrap_or(0);
            if backlog > 0 {
                crate::dprintln!(
                    "[input-stale-key] dev={} key={:#x} NOT REGISTERED, {}/8 reader queues hold {} unread events",
                    dev,
                    key,
                    m.get(&dev).map(|d| d.readers.len()).unwrap_or(0),
                    backlog
                );
            }
        }
    }
    ev
}

pub fn input_pending(key: usize, dev: usize) -> bool {
    INPUT_DEV
        .lock()
        .get(&dev)
        .map(|d| d.readers.get(&key).map(|q| !q.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

/// Lock-free probe for the park path: on try_lock failure the holder is
/// mid-push and its wake-up will find us already queued.
pub fn input_pending_try(key: usize, dev: usize) -> Option<bool> {
    let m = INPUT_DEV.try_lock()?;
    let d = m.get(&dev)?;
    Some(
        d.readers
            .get(&key)
            .map(|q| !q.is_empty())
            .unwrap_or(false),
    )
}

pub struct Keyboard {
    shift_down: bool,
    ctrl_down: bool,
    extended_mode: bool,
    key_states: [bool; 128],
}

impl Keyboard {
    pub const fn new() -> Self {
        Self {
            shift_down: false,
            ctrl_down: false,
            extended_mode: false,
            key_states: [false; 128],
        }
    }

    pub fn is_key_down(&self, scancode: u8) -> bool {
        if (scancode as usize) < 128 {
            self.key_states[scancode as usize]
        } else {
            false
        }
    }

    pub fn process_scancode(&mut self, scancode: u8) -> Option<char> {
        if scancode == 0xE0 {
            self.extended_mode = true;
            return None;
        }

        let is_release = (scancode & 0x80) != 0;
        let base_scancode = scancode & 0x7F;

        if (base_scancode as usize) < 128 {
            self.key_states[base_scancode as usize] = !is_release;
        }

        // Convert scancode to Linux Key Code and push to this keyboard's own
        // evdev node (never the aggregate — aggregates are gone).
        let linux_code = scancode_to_linux_code(base_scancode, self.extended_mode);
        let dev = PS2_KBD_DEV.load(core::sync::atomic::Ordering::Relaxed);
        crate::dprintln!("[kbd-ps2] scancode={:#04x} linux_code={} val={} dev={}", base_scancode, linux_code, if is_release { 0 } else { 1 }, dev);
        let event = InputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: 1, // EV_KEY
            code: linux_code,
            value: if is_release { 0 } else { 1 },
        };

        input_push(event, dev);

        // EV_SYN/SYN_REPORT terminates the report frame (libevdev requires it).
        // input_push wakes both INPUT_WAIT_QUEUE and PIPE_WAIT_QUEUE itself,
        // so the waiters parked in drain_input_events and the epoll_wait
        // threads are both covered without a second wake here.
        input_push(InputEvent { time_sec: 0, time_usec: 0, type_: 0x00, code: 0x00, value: 0 }, dev);

        // Handle shift keys (Left Shift = 0x2A, Right Shift = 0x36)
        if base_scancode == 0x2A || base_scancode == 0x36 {
            self.shift_down = !is_release;
            self.extended_mode = false;
            return None;
        }

        // Handle ctrl keys (Left Ctrl = 0x1D, Right Ctrl = 0xE0 0x1D)
        if base_scancode == 0x1D {
            self.ctrl_down = !is_release;
            self.extended_mode = false;
            return None;
        }

        // Ctrl+C generates SIGINT
        if self.ctrl_down && base_scancode == 0x2E && !is_release {
            crate::interrupts::signal_ctrl_c_from_isr();
            self.extended_mode = false;
            return None;
        }

        if is_release {
            self.extended_mode = false;
            return None;
        }

        // Reset extended mode for the next key
        let was_extended = self.extended_mode;
        self.extended_mode = false;

        if !was_extended && (base_scancode < 0x3A) {
            let key = if self.shift_down {
                SCANCODE_MAP_SHIFTED[base_scancode as usize]
            } else {
                SCANCODE_MAP_UNSHIFTED[base_scancode as usize]
            };
            return key;
        }
        None
    }
}

pub fn scancode_to_linux_code(base_scancode: u8, extended: bool) -> u16 {
    if extended {
        // ponytail: E0-prefixed codes do NOT equal Linux keycodes (unlike the
        // main block) — every used one needs an explicit arm, else e.g. Super
        // reports 91 and Super+T can never match modifiers.logo.
        return match base_scancode {
            0x48 => 103, // UP
            0x4B => 105, // LEFT
            0x4D => 106, // RIGHT
            0x50 => 108, // DOWN
            0x1D => 97,  // RIGHTCTRL
            0x38 => 100, // RIGHTALT
            0x5B => 125, // LEFTMETA (Super)
            0x5C => 126, // RIGHTMETA (Super)
            0x5D => 127, // COMPOSE (Menu)
            0x47 => 102, // HOME
            0x4F => 107, // END
            0x49 => 104, // PAGEUP
            0x51 => 109, // PAGEDOWN
            0x52 => 110, // INSERT
            0x53 => 111, // DELETE
            0x35 => 98,  // KPSLASH
            0x1C => 96,  // KPENTER
            _ => base_scancode as u16,
        };
    }

    match base_scancode {
        0x01 => 1,   // KEY_ESC
        0x02 => 2,   // 1
        0x03 => 3,   // 2
        0x04 => 4,   // 3
        0x05 => 5,   // 4
        0x06 => 6,   // 5
        0x07 => 7,   // 6
        0x08 => 8,   // 7
        0x09 => 9,   // 8
        0x0A => 10,  // 9
        0x0B => 11,  // 0
        0x0C => 12,  // MINUS
        0x0D => 13,  // EQUAL
        0x0E => 14,  // BACKSPACE
        0x0F => 15,  // TAB
        0x10 => 16,  // Q
        0x11 => 17,  // W
        0x12 => 18,  // E
        0x13 => 19,  // R
        0x14 => 20,  // T
        0x15 => 21,  // Y
        0x16 => 22,  // U
        0x17 => 23,  // I
        0x18 => 24,  // O
        0x19 => 25,  // P
        0x1C => 28,  // ENTER
        0x1D => 29,  // LEFTCTRL
        0x1E => 30,  // A
        0x1F => 31,  // S
        0x20 => 32,  // D
        0x21 => 33,  // F
        0x22 => 34,  // G
        0x23 => 35,  // H
        0x24 => 36,  // J
        0x25 => 37,  // K
        0x26 => 38,  // L
        0x2A => 42,  // LEFTSHIFT
        0x2C => 44,  // Z
        0x2D => 45,  // X
        0x2E => 46,  // C
        0x2F => 47,  // V
        0x30 => 48,  // B
        0x31 => 49,  // N
        0x32 => 50,  // M
        0x36 => 54,  // RIGHTSHIFT
        0x38 => 56,  // LEFTALT
        0x39 => 57,  // SPACE
        _ => base_scancode as u16,
    }
}

// ---------------------------------------------------------
// Standard US QWERTY Layout Maps (Index == Scancode)
// ---------------------------------------------------------

const SCANCODE_MAP_UNSHIFTED: [Option<char>; 58] = [
    None,
    None,
    Some('1'),
    Some('2'),
    Some('3'),
    Some('4'),
    Some('5'),
    Some('6'),
    Some('7'),
    Some('8'),
    Some('9'),
    Some('0'),
    Some('-'),
    Some('='),
    Some('\u{8}'), // Backspace
    Some('\t'),
    Some('q'),
    Some('w'),
    Some('e'),
    Some('r'),
    Some('t'),
    Some('y'),
    Some('u'),
    Some('i'),
    Some('o'),
    Some('p'),
    Some('['),
    Some(']'),
    Some('\n'),
    None,
    Some('a'),
    Some('s'),
    Some('d'),
    Some('f'),
    Some('g'),
    Some('h'),
    Some('j'),
    Some('k'),
    Some('l'),
    Some(';'),
    Some('\''),
    Some('`'),
    None,
    Some('\\'),
    Some('z'),
    Some('x'),
    Some('c'),
    Some('v'),
    Some('b'),
    Some('n'),
    Some('m'),
    Some(','),
    Some('.'),
    Some('/'),
    None,
    Some('*'),
    None,
    Some(' '),
];

const SCANCODE_MAP_SHIFTED: [Option<char>; 58] = [
    None,
    None,
    Some('!'),
    Some('@'),
    Some('#'),
    Some('$'),
    Some('%'),
    Some('^'),
    Some('&'),
    Some('*'),
    Some('('),
    Some(')'),
    Some('_'),
    Some('+'),
    Some('\u{8}'), // Backspace
    Some('\t'),
    Some('Q'),
    Some('W'),
    Some('E'),
    Some('R'),
    Some('T'),
    Some('Y'),
    Some('U'),
    Some('I'),
    Some('O'),
    Some('P'),
    Some('{'),
    Some('}'),
    Some('\n'),
    None,
    Some('A'),
    Some('S'),
    Some('D'),
    Some('F'),
    Some('G'),
    Some('H'),
    Some('J'),
    Some('K'),
    Some('L'),
    Some(':'),
    Some('"'),
    Some('~'),
    None,
    Some('|'),
    Some('Z'),
    Some('X'),
    Some('C'),
    Some('V'),
    Some('B'),
    Some('N'),
    Some('M'),
    Some('<'),
    Some('>'),
    Some('?'),
    None,
    Some('*'),
    None,
    Some(' '),
];
