//! PS/2 mouse driver (8042 auxiliary port, IRQ12).
//!
//! Decodes standard 3-byte movement packets and feeds evdev-style events
//! (EV_KEY BTN_*, EV_REL REL_X/REL_Y) into INPUT_EVENT_QUEUE alongside
//! keyboard events. Each push targets this mouse's own evdev node.

use crate::arch::sync::spinlock::SpinLock;
use crate::drivers::keyboard::InputEvent;

// ponytail: TEMP-DIAG counters for the bottom-up input chain. The first
// one that stops climbing tells us where the break is (irq → packet →
// push → consume). Surfaced in [input-stats].
pub static IRQ_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PACKET_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PACKET_DROP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// Linux evdev constants
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const REL_WHEEL: u16 = 0x08;

const PORT_STATUS: u16 = 0x64;
const PORT_DATA: u16 = 0x60;

/// Packet accumulator state shared with the IRQ handler.
static PACKET: SpinLock<PacketBuf> = SpinLock::new(PacketBuf::new());

struct PacketBuf {
    bytes: [u8; 4],
    idx: u8,
    /// Last button state for press/release edge detection.
    last_buttons: u8,
    /// True after the ImPS/2 magic sequence (wheel byte in packet[3]).
    wheel: bool,
}

impl PacketBuf {
    const fn new() -> Self {
        Self { bytes: [0; 4], idx: 0, last_buttons: 0, wheel: false }
    }

    fn pkt_len(&self) -> usize {
        if self.wheel { 4 } else { 3 }
    }
}

#[inline]
fn wait_write() {
    // Wait until controller input buffer is free (status bit 1 clear).
    let mut n = 0usize;
    while unsafe { crate::arch::x86_64::io::inb(PORT_STATUS) } & 0x02 != 0 && n < 100_000 {
        core::hint::spin_loop();
        n += 1;
    }
}

#[inline]
fn wait_read() -> Option<u8> {
    let mut n = 0usize;
    while unsafe { crate::arch::x86_64::io::inb(PORT_STATUS) } & 0x01 == 0 && n < 100_000 {
        core::hint::spin_loop();
        n += 1;
    }
    if n >= 100_000 { return None; }
    Some(unsafe { crate::arch::x86_64::io::inb(PORT_DATA) })
}

/// Send a command byte to the mouse (via controller port 0xD4).
unsafe fn mouse_send(cmd: u8) -> Option<u8> {
    wait_write();
    unsafe { crate::arch::x86_64::io::outb(PORT_STATUS, 0xD4) };
    wait_write();
    unsafe { crate::arch::x86_64::io::outb(PORT_DATA, cmd) };
    wait_read() // ACK (0xFA) or error
}

/// Enable the auxiliary port, turn on IRQ12 streaming at ~40 samples/s.
// ponytail: claims this mouse's own evdev node (second alloc at boot, so
// minor 1) + sysfs/uevent registration; pushes target it, never an aggregate.
pub fn init() -> bool {
    unsafe {
        // Drain any stale output (bounded: with no 8042 controller the
        // status port reads 0xFF forever, so bail out like the kbd init).
        let mut drain = 0usize;
        while crate::arch::x86_64::io::inb(PORT_STATUS) & 0x01 != 0 {
            crate::arch::x86_64::io::inb(PORT_DATA);
            drain += 1;
            if drain >= 1000 {
                crate::dprintln!("[PS2MOUSE] no 8042 controller, skipping");
                return false;
            }
        }

        // Enable the auxiliary (mouse) port.
        wait_write();
        crate::arch::x86_64::io::outb(PORT_STATUS, 0xA8);

        // Read config byte, enable IRQ12 + mouse clock.
        wait_write();
        crate::arch::x86_64::io::outb(PORT_STATUS, 0x20);
        let cfg = match wait_read() { Some(b) => b, None => return false };
        wait_write();
        crate::arch::x86_64::io::outb(PORT_STATUS, 0x60);
        wait_write();
        crate::arch::x86_64::io::outb(PORT_DATA, (cfg | 0x03) & !0x30);

        // Reset mouse, expect 0xAA self-test pass then device id.
        if mouse_send(0xFF) != Some(0xFA) { return false; }
        let _aa = wait_read(); // 0xAA
        let _id = wait_read(); // 0x00

        // Set defaults.
        if mouse_send(0xF6) != Some(0xFA) { return false; }

        // ImPS/2 wheel magic: sample rates 200,200,60 then read ID. A wheel
        // mouse answers with id 3 and appends a 4th signed byte per packet.
        let mut wheel = false;
        for r in [200u8, 200, 60] {
            if mouse_send(0xF3) != Some(0xFA) { break; }
            if mouse_send(r) != Some(0xFA) { break; }
        }
        if mouse_send(0xF2) == Some(0xFA) {
            if let Some(id) = wait_read() {
                wheel = id == 3;
            }
        }
        PACKET.lock().wheel = wheel;

        // Start data reporting.
        if mouse_send(0xF3) != Some(0xFA) { return false; }
        if mouse_send(40) != Some(0xFA) { return false; }
        if mouse_send(0xF4) != Some(0xFA) { return false; }

        let _ = wheel;
    }

    let wheel = PACKET.lock().wheel;
    crate::dprintln!("[PS2MOUSE] enabled (IRQ12, wheel={})", wheel);
    let minor = crate::drivers::keyboard::input_alloc_node(
        crate::drivers::keyboard::mouse_caps("dragos-ps2mouse"),
    );
    crate::drivers::keyboard::PS2_MOUSE_DEV
        .store(minor, core::sync::atomic::Ordering::Relaxed);
    crate::fs::sysfs::sysfs_register_input(minor, crate::fs::sysfs::EVDEV_CAPS_MOUSE);
    crate::dprintln!("[PS2MOUSE] evdev node event{minor} (minor {minor})");
    true
}

/// IRQ12 handler body. Returns true if a complete packet was decoded.
pub fn handle_irq() -> bool {
    let status = unsafe { crate::arch::x86_64::io::inb(PORT_STATUS) };
    if status & 0x01 == 0 || status & 0x20 == 0 {
        return false; // not aux data
    }
    let byte = unsafe { crate::arch::x86_64::io::inb(PORT_DATA) };

    IRQ_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    let mut pkt = PACKET.lock();
    // Bit 3 of the first packet byte is always set — resync on framing errors.
    if pkt.idx == 0 && byte & 0x08 == 0 {
        PACKET_DROP.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return false;
    }

    let idx = pkt.idx as usize;
    pkt.bytes[idx] = byte;
    pkt.idx += 1;
    if (pkt.idx as usize) < pkt.pkt_len() {
        return false;
    }
    pkt.idx = 0;

    PACKET_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

    let b0 = pkt.bytes[0];
    let dx_raw = pkt.bytes[1] as i32;
    let dy_raw = pkt.bytes[2] as i32;
    let buttons = b0 & 0x07;
    let x_ovf = b0 & 0x40 != 0;
    let y_ovf = b0 & 0x80 != 0;

    // Sign-extend 9-bit two's complement values.
    let mut dx = dx_raw - ((b0 as i32 & 0x10) << 4);
    let mut dy = dy_raw - ((b0 as i32 & 0x20) << 4);
    // Screen Y grows downward; PS/2 Y grows upward.
    dy = -dy;
    if x_ovf { dx = 0; }
    if y_ovf { dy = 0; }

    // Button press/release edges only.
    for (bit, code) in [(0x01u8, BTN_LEFT), (0x02, BTN_RIGHT), (0x04, BTN_MIDDLE)] {
        let was = pkt.last_buttons & bit != 0;
        let now = buttons & bit != 0;
        if was != now {
            push_event(EV_KEY, code, now as i32);
        }
    }
    if dx != 0 { push_event(EV_REL, REL_X, dx); }
    if dy != 0 { push_event(EV_REL, REL_Y, dy); }
    if pkt.pkt_len() == 4 && pkt.bytes[3] != 0 {
        // Screen Y down: wheel up (positive device value) scrolls up (-1).
        push_event(EV_REL, REL_WHEEL, -(pkt.bytes[3] as i8 as i32));
    }
    push_syn();

    let changed = buttons != pkt.last_buttons || dx != 0 || dy != 0;
    pkt.last_buttons = buttons;
    changed
}

/// EV_SYN/SYN_REPORT terminates an evdev report frame; libevdev withholds
/// events from libinput until it sees one.
fn push_syn() {
    let dev = crate::drivers::keyboard::PS2_MOUSE_DEV.load(core::sync::atomic::Ordering::Relaxed);
    crate::drivers::keyboard::input_push(InputEvent { time_sec: 0, time_usec: 0, type_: 0x00, code: 0x00, value: 0 }, dev);
}

fn push_event(type_: u16, code: u16, value: i32) {
    let dev = crate::drivers::keyboard::PS2_MOUSE_DEV.load(core::sync::atomic::Ordering::Relaxed);
    // input_push already wakes INPUT_WAIT_QUEUE *and* PIPE_WAIT_QUEUE (the
    // latter so epoll_wait-parked threads see the event). Waking again here
    // doubled that: a single motion packet became ~10 wake_input_waiters and
    // ~10 wake_pipe_waiters, and each wake_pipe_waiters drains the entire
    // PIPE_WAIT_QUEUE. That is a thundering herd on every packet, and it ran
    // from IRQ context while the input lock was held.
    crate::drivers::keyboard::input_push(InputEvent { time_sec: 0, time_usec: 0, type_, code, value }, dev);
}
