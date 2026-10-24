//! Minimal xHCI (USB 3.x / 2.x) host controller driver.
//!
//! Supports HID boot-protocol keyboards and mice end-to-end:
//! controller reset → port reset → enable-slot → address-device →
//! control transfers (descriptors, SET_CONFIG, SET_PROTOCOL boot) →
//! interrupt IN endpoints polled by the timer tick, feeding per-device evdev
//! nodes (/dev/input/eventN, one minor per physical device) and stdin.
//!
//! ponytail: polled event ring (no MSI/MSI-X), boot protocol only.
//! Hotplug connect/disconnect is detected in the timer tick but serviced from
//! the idle loop (service_hotplug): input_alloc/free + uevent broadcast take
//! SCHEDULER and heap-allocate, both banned in IRQ context. Upgrade path:
//! MSI + full HID report-descriptor parsing when a consumer appears.

#![allow(unused)]

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::sync::spinlock::SpinLock;
use crate::arch::x86_64::pci::{self, PciAddress};
use crate::drivers::virtio::DmaBuf;

// ponytail: TEMP-DIAG counters for the bottom-up input chain — same idea as
// ps2_mouse: first counter that stops climbing tells us where the break is.
// decode_mouse and decode_tablet increment REPORT_COUNT on success and
// REPORT_DROP on short packets.
pub static REPORT_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static REPORT_DROP: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static MOUSE_REPORT_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static MOUSE_REPORT_DROP: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static TABLET_REPORT_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static TABLET_REPORT_DROP: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static KBD_REPORT_COUNT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
pub static KBD_REPORT_DROP: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);
// ponytail: TEMP-DIAG chain-of-custody for the silent tablet death: last
// tick ANY event-ring entry was seen, last tick an int-endpoint completion
// succeeded, and total refill doorbells rung. One post-stall snapshot shows
// exactly which link breaks (controller stops writing events vs driver
// stops re-arming). Remove after root cause.
pub static XHCI_EVT_TICK: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static XHCI_OK_TICK: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
pub static XHCI_REARM: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
use shared::boot::boot_info::HHDM_BASE;
use shared::memory::address::VirtAddr;

// ── Capability registers (mmio base + …) ─────────────────────────────────────
const CAPLENGTH: usize = 0x00; // u8: op-reg base
const HCSPARAMS1: usize = 0x04; // MaxSlots [7:0], MaxPorts [31:24]
const HCCPARAMS1: usize = 0x10; // CSZ bit2
const DBOFF: usize = 0x14; // ×4
const RTSOFF: usize = 0x18; // ×4

// ── Operational registers ────────────────────────────────────────────────────
const OP_USBCMD: usize = 0x00;
const OP_PAGESIZE: usize = 0x08;
const OP_USBSTS: usize = 0x04;
const OP_CRCR: usize = 0x18;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;
const OP_PORTS: usize = 0x400;

const USBCMD_RS: u32 = 1 << 0;
const USBCMD_HCRST: u32 = 1 << 1;
const USBSTS_CNR: u32 = 1 << 10;

const PSC_CCS: u32 = 1 << 0;
const PSC_PED: u32 = 1 << 1;
const PSC_PR: u32 = 1 << 4;
const PSC_PP: u32 = 1 << 9;
const PSC_CSC: u32 = 1 << 17;
const PSC_PEC: u32 = 1 << 18;
const PSC_SPEED_SHIFT: u32 = 10;

// ── Runtime registers (rt_base + …) ──────────────────────────────────────────
const RT_IR0: usize = 0x20;
const IR_IMAN: usize = 0x00;
const IR_IMOD: usize = 0x04;
const IR_ERSTSZ: usize = 0x08;
const IR_ERSTBA: usize = 0x10;
const IR_ERDP: usize = 0x18;

// ── TRB types ([15:10] of control word) ──────────────────────────────────────
const TRB_NORMAL: u32 = 1;
const TRB_SETUP_STAGE: u32 = 2;
const TRB_DATA_STAGE: u32 = 3;
const TRB_STATUS_STAGE: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIG_EP: u32 = 12;
const TRB_EVALUATE_CTX: u32 = 13;
const TRB_RESET_EP: u32 = 14;
const TRB_SET_DEQ: u32 = 15;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_CMD_COMPLETION: u32 = 33;

const F_CH: u32 = 1 << 4;
const F_IOC: u32 = 1 << 5;
const F_IDT: u32 = 1 << 6;
const F_DIR_IN: u32 = 1 << 16;

const CC_SUCCESS: u8 = 1;
const CC_SHORT_PACKET: u8 = 13;

const TRBS_PER_RING: usize = 256;
const MAX_SLOTS: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DevKind {
    Keyboard,
    Mouse,
    Tablet,
    Unknown,
}

#[derive(Clone, Copy, Debug)]
pub struct ReportField {
    pub usage_page: u16,
    pub usage: u16,
    pub bit_off: u16,
    pub bit_size: u8,
    pub logical_min: i32,
    pub logical_max: i32,
    pub is_variable: bool,
}

#[derive(Clone, Debug)]
pub struct ReportLayout {
    pub fields: alloc::vec::Vec<ReportField>,
    pub total_bits: u16,
    pub has_keyboard: bool,
    pub has_pointer: bool,
    pub is_absolute: bool,
}

/// Parse a HID report descriptor into a layout describing each input field.
/// Short items only: Usage Page (0x04), Usage (0x08), Usage Min/Max (0x18/0x28),
/// Logical Min/Max (0x14/0x24), Report Size (0x74), Report Count (0x94), Input (0x80),
/// Collection (0xA0), End Collection (0xC0).
pub fn parse_report_map(desc: &[u8]) -> Option<ReportLayout> {
    let mut fields = alloc::vec::Vec::new();
    let mut cur_bit_off: u16 = 0;

    let mut usage_page: u16 = 0;
    let mut logical_min: i32 = 0;
    let mut logical_max: i32 = 0;
    let mut report_size: u32 = 0;
    let mut report_count: u32 = 0;
    let mut usages: alloc::vec::Vec<u16> = alloc::vec::Vec::new();
    let mut usage_min: Option<u16> = None;
    let mut usage_max: Option<u16> = None;

    let mut has_keyboard = false;
    let mut has_pointer = false;
    let mut is_absolute = false;

    let mut i = 0;
    while i < desc.len() {
        let b = desc[i];
        if b == 0xFE {
            // Long item: 0xFE, size, tag, data...
            if i + 1 >= desc.len() {
                break;
            }
            let data_size = desc[i + 1] as usize;
            i += 3 + data_size;
            continue;
        }

        let b_size = (b & 0x03) as usize;
        let data_len = if b_size == 3 { 4 } else { b_size };
        let b_type = (b >> 2) & 0x03;
        let b_tag = (b >> 4) & 0x0F;

        if i + 1 + data_len > desc.len() {
            break;
        }

        let raw_data = &desc[i + 1..i + 1 + data_len];
        let unsigned_val: u32 = match data_len {
            0 => 0,
            1 => raw_data[0] as u32,
            2 => u16::from_le_bytes([raw_data[0], raw_data[1]]) as u32,
            4 => u32::from_le_bytes([raw_data[0], raw_data[1], raw_data[2], raw_data[3]]),
            _ => 0,
        };
        let signed_val: i32 = match data_len {
            0 => 0,
            1 => raw_data[0] as i8 as i32,
            2 => i16::from_le_bytes([raw_data[0], raw_data[1]]) as i32,
            4 => i32::from_le_bytes([raw_data[0], raw_data[1], raw_data[2], raw_data[3]]),
            _ => 0,
        };

        match (b_type, b_tag) {
            // Global items
            (1, 0x00) => {
                // Usage Page
                usage_page = unsigned_val as u16;
            }
            (1, 0x01) => {
                // Logical Minimum
                logical_min = signed_val;
            }
            (1, 0x02) => {
                // Logical Maximum
                logical_max = signed_val;
            }
            (1, 0x07) => {
                // Report Size (bits)
                report_size = unsigned_val;
            }
            (1, 0x09) => {
                // Report Count
                report_count = unsigned_val;
            }

            // Local items
            (2, 0x00) => {
                // Usage
                usages.push(unsigned_val as u16);
            }
            (2, 0x01) => {
                // Usage Minimum
                usage_min = Some(unsigned_val as u16);
            }
            (2, 0x02) => {
                // Usage Maximum
                usage_max = Some(unsigned_val as u16);
            }

            // Main items
            (0, 0x08) => {
                // Input (flags in unsigned_val)
                let is_constant = (unsigned_val & 0x01) != 0;
                let is_variable = (unsigned_val & 0x02) != 0;

                // Expand usages
                if let (Some(umin), Some(umax)) = (usage_min, usage_max) {
                    if umax >= umin {
                        for u in umin..=umax {
                            usages.push(u);
                        }
                    }
                }

                for field_idx in 0..report_count {
                    if fields.len() >= 64 {
                        break;
                    }
                    let usage = if !usages.is_empty() {
                        if (field_idx as usize) < usages.len() {
                            usages[field_idx as usize]
                        } else {
                            *usages.last().unwrap()
                        }
                    } else {
                        0
                    };

                    if !is_constant && report_size > 0 {
                        if usage_page == 0x07 {
                            has_keyboard = true;
                        } else if usage_page == 0x01 {
                            if usage == 0x30 || usage == 0x31 {
                                has_pointer = true;
                                if logical_max > 255 || logical_min < -128 {
                                    is_absolute = true;
                                }
                            }
                        } else if usage_page == 0x09 {
                            // Buttons
                            has_pointer = true;
                        }

                        fields.push(ReportField {
                            usage_page,
                            usage,
                            bit_off: cur_bit_off,
                            bit_size: report_size as u8,
                            logical_min,
                            logical_max,
                            is_variable,
                        });
                    }
                    cur_bit_off = cur_bit_off.saturating_add(report_size as u16);
                }

                usages.clear();
                usage_min = None;
                usage_max = None;
            }
            (0, 0x09) => {
                // Output report fields belong to host->device OUT reports (e.g. keyboard LEDs).
                // They do NOT occupy bits in the device->host INPUT report.
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
            (0, 0x0B) => {
                // Feature - skip bits
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
            (0, 0x0A) | (0, 0x0C) => {
                // Collection / End Collection
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
            _ => {}
        }

        i += 1 + data_len;
    }

    if fields.is_empty() {
        return None;
    }

    Some(ReportLayout {
        fields,
        total_bits: cur_bit_off,
        has_keyboard,
        has_pointer,
        is_absolute,
    })
}


struct Ring {
    buf: DmaBuf,
    cycle: u8,
    enq: usize,
}

impl Ring {
    fn new() -> Option<Self> {
        // ponytail: 64K-align everything; real controllers may report a
        // PAGESIZE up to 64K and require ring/context alignment to match.
        Some(Self { buf: DmaBuf::new_aligned(TRBS_PER_RING * 16, 64 * 1024)?, cycle: 1, enq: 0 })
    }

    #[inline]
    fn trb_ptr(&self, i: usize) -> *mut u64 {
        unsafe { self.buf.as_mut_ptr().add(i * 16) as *mut u64 }
    }

    fn phys(&self) -> u64 {
        self.buf.phys().0
    }

    /// Enqueue one TRB, handling the ring wrap with a LINK TRB. Returns the
    /// physical address the TRB was written at (for completion matching).
    /// Layout: [0..8]=parameter, [8..12]=status, [12..16]=control; the
    /// ring's cycle bit goes into control bit 0.
    /// ponytail: 255 usable slots (0..254); slot 255 always holds the LINK
    /// with TC=1. The old write+advance wrote a live TRB at 255, clobbered
    /// it with a LINK whose TC bit was 0 (must be 1), so after the first
    /// wrap the controller ignored every TRB and the endpoint died silently
    /// (~235 tablet reports + boot traffic, no error, no log).
    pub fn push_trb(&mut self, param: u64, status: u32, mut control: u32) -> u64 {
        if self.enq == TRBS_PER_RING - 1 {
            // LINK TRB cycle bit MUST match controller's CURRENT CCS (old cycle).
            // Controller reads Link TRB with old CCS, sees TC=1, toggles CCS for next TRB.
            // Driver then toggles its cycle to match controller's new CCS.
            let link_control = (TRB_LINK << 10) | (1 << 1) | self.cycle as u32;
            unsafe {
                core::ptr::write_volatile(self.trb_ptr(self.enq), self.phys());
                // ponytail: control goes in the HIGH dword (like normal TRBs below);
                // writing it raw put Type/TC in the status dword, so the controller
                // never saw a Link and the endpoint died silently at first wrap (~255).
                core::ptr::write_volatile(
                    self.trb_ptr(self.enq).add(1),
                    (link_control as u64) << 32,
                );
            }
            self.enq = 0;
            self.cycle ^= 1;
        }
        control |= self.cycle as u32;
        let sc = (control as u64) << 32 | status as u64;
        let at = self.phys() + (self.enq * 16) as u64;
        unsafe {
            core::ptr::write_volatile(self.trb_ptr(self.enq), param);
            core::ptr::write_volatile(self.trb_ptr(self.enq).add(1), sc);
        }
        self.enq += 1;
        at
    }


    fn deq_with_cycle(&self) -> u64 {
        self.phys() | self.cycle as u64
    }
}

/// Context sizes: 32 or 64 bytes depending on HCCPARAMS1.CSZ.
struct Ctx {
    buf: *mut u8,
    size: usize,
}

impl Ctx {
    /// Offset of context #n within a context block.
    #[inline]
    fn off(&self, n: usize) -> usize {
        n * self.size
    }

    unsafe fn wr32(&self, off: usize, val: u32) {
        unsafe { core::ptr::write_volatile(self.buf.add(off) as *mut u32, val) };
    }
}

struct UsbDevice {
    kind: DevKind,
    /// Root-hub port index (0-based) this device is attached to.
    port: usize,
    /// This device's own evdev minor (2+); decode_* push only here, never to
    /// an aggregate. usize::MAX until finish_attach runs (process context).
    evdev_minor: usize,
    ep0: Ring,
    int_ring: Option<Ring>,
    int_ep_addr: u8,
    iface_num: u8,
    report_buf: Option<DmaBuf>,
    report_len: usize,
    last_mods: u8,
    last_keys: [u8; 6],
    last_buttons: u8,
    last_x: u16,
    last_y: u16,
    last_pos_valid: bool,
    // ponytail: sub-pixel remainders for tablet HID-unit → screen-px scaling
    rem_x: i32,
    rem_y: i32,
    layout: Option<ReportLayout>,
}

pub struct Xhci {
    mmio: VirtAddr,
    op_base: usize,
    rt_base: usize,
    db_base: usize,
    max_ports: usize,
    ctx_size: usize,
    dcbaa: DmaBuf,
    input_ctx: DmaBuf,
    cmd_ring: Ring,
    event_ring: DmaBuf,
    event_cycle: u8,
    event_idx: usize,
    devices: [Option<UsbDevice>; MAX_SLOTS],
    pub msi_x: bool,
}

static XHCI: SpinLock<Option<Xhci>> = SpinLock::new(None);
static XHCI_PRESENT: AtomicBool = AtomicBool::new(false);

// ponytail: IRQ→process handoff for hotplug. scan_hotplug runs in the LAPIC
// timer-tick path (IF=0): it only acks the PORTSC change bits and sets this.
// The idle loop (cpu_idle_loop, no locks held) sees it and runs
// service_hotplug, which does the MMIO + input_alloc/free + broadcast +
// sysfs work. No heap, no SCHEDULER in IRQ context (AGENTS.md §1-2).
static HOTPLUG_DIRTY: AtomicBool = AtomicBool::new(false);

pub fn present() -> bool {
    XHCI_PRESENT.load(Ordering::Relaxed)
}

impl Xhci {
    #[inline]
    fn rd(&self, off: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.mmio.0 + off as u64) as *const u32) }
    }
    #[inline]
    fn wr32(&self, off: usize, v: u32) {
        unsafe { core::ptr::write_volatile((self.mmio.0 + off as u64) as *mut u32, v) }
    }
    #[inline]
    fn op_rd(&self, off: usize) -> u32 {
        self.rd(self.op_base + off)
    }
    #[inline]
    fn op_wr(&self, off: usize, v: u32) {
        self.wr32(self.op_base + off, v);
    }
    #[inline]
    fn portsc(&self, port: usize) -> u32 {
        self.rd(self.op_base + OP_PORTS + port * 16)
    }
    #[inline]
    fn portsc_set(&self, port: usize, bits: u32, clear: u32) {
        let v = self.portsc(port);
        self.wr32(self.op_base + OP_PORTS + port * 16, (v | bits) & !clear);
    }
    #[inline]
    fn doorbell(&self, target: u32, task: u32) {
        // Doorbell 0 must be rung with value 0 (QEMU: val==0 → process cmds).
        // EP doorbells carry the endpoint ID (nonzero).
        unsafe { core::ptr::write_volatile((self.db_base + 4 * target as usize) as *mut u32, task) };
    }

    // ── Input-context helpers ────────────────────────────────────────────

    fn input_ctx(&self) -> Ctx {
        Ctx { buf: self.input_ctx.as_mut_ptr(), size: self.ctx_size }
    }

    /// Zero the whole input context block (control + 33 contexts).
    fn clear_input_ctx(&self) {
        unsafe { core::ptr::write_bytes(self.input_ctx.as_mut_ptr(), 0, self.input_ctx.len()) };
    }

    // ── Event ring ───────────────────────────────────────────────────────

    fn next_event(&mut self) -> Option<(u64, u32, u32)> {
        let trb_phys = self.event_ring.phys().0 + (self.event_idx * 16) as u64;
        let ptr = (HHDM_BASE.0 + trb_phys) as *const u64;
        let sc = unsafe { core::ptr::read_volatile(ptr.add(1)) };
        let control = (sc >> 32) as u32;
        if (control & 1) as u8 != self.event_cycle {
            return None;
        }
        let param = unsafe { core::ptr::read_volatile(ptr) };
        let status = sc as u32;

        self.event_idx += 1;
        if self.event_idx == TRBS_PER_RING {
            // ponytail: single ERST segment of TRBS_PER_RING TRBs with NO
            // link TRB — the controller uses all 256 slots (0..255). The old
            // `- 1` skipped slot 255 every cycle, orphaning one completion
            // per wrap; its endpoint was never refilled and went silent
            // forever (tablet death at ~235 reports + boot events).
            self.event_idx = 0;
            self.event_cycle ^= 1;
        }
        // Advance ERDP (64-bit split regs) so the host can reuse entries.
        let erdp = self.event_ring.phys().0 + (self.event_idx * 16) as u64;
        self.wr32(self.rt_base + RT_IR0 + IR_ERDP, erdp as u32);
        self.wr32(self.rt_base + RT_IR0 + IR_ERDP + 4, (erdp >> 32) as u32);
        XHCI_EVT_TICK.store(crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed), core::sync::atomic::Ordering::Relaxed);
        Some((param, status, control))
    }

    fn drain_events(&mut self) {
        loop {
            let ev = match self.next_event() {
                Some(e) => e,
                None => break,
            };
            let (param, status, control) = ev;
            let ty = (control >> 10) & 0x3F;
            if ty != TRB_TRANSFER_EVENT {
                continue;
            }
            let cc = (status >> 24) as u8;
            let ep_id = ((control >> 16) & 0x1F) as usize;
            let slot = (control >> 24) as usize;
            if cc != CC_SUCCESS && cc != CC_SHORT_PACKET {
                // ponytail: never silently drop — one transient error here
                // (STALL / MISSED_SERVICE under load) permanently silenced
                // the endpoint: no re-arm, no log, tablet dead while kernel
                // queues stayed perfect. Recover like EP0, then re-arm so
                // traffic resumes; the dprintln names future offenders.
                crate::dprintln!("[XHCI] int_xfer cc={} slot={} ep={}", cc, slot, ep_id);
                if self.devices.get(slot).is_some() {
                    let deq = self.devices.get(slot).and_then(|d| d.as_ref())
                        .and_then(|d| d.int_ring.as_ref())
                        .map(|r| r.deq_with_cycle())
                        .unwrap_or(0);
                    self.recover_ep(slot as u8, ep_id as u32, deq);
                    self.refill_int_ring(slot as u8);
                }
                continue;
            }
            if ep_id == 1 {
                continue; // ep0 (control) completions are polled synchronously
            }
            XHCI_OK_TICK.store(crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed), core::sync::atomic::Ordering::Relaxed);
            // Deliver report, then refill the ring.
            if let Some(Some(dev)) = self.devices.get_mut(slot) {
                dev.deliver_report();
                self.refill_int_ring(slot as u8);
            }
        }
    }

    fn refill_int_ring(&mut self, slot: u8) {
        let (ep_addr,) = match self.devices.get(slot as usize).and_then(|d| d.as_ref()) {
            Some(d) => match d.int_ring.is_some() {
                true => (d.int_ep_addr,),
                false => return,
            },
            None => return,
        };
        let dev = self.devices.get_mut(slot as usize).and_then(|d| d.as_mut()).unwrap();
        let ring = dev.int_ring.as_mut().unwrap();
        let buf_phys = dev.report_buf.as_ref().map(|b| b.phys().0).unwrap_or(0);
        ring.push_trb(buf_phys, dev.report_len as u32, (TRB_NORMAL << 10) | F_IOC);
        let db = ((ep_addr & 0x0F) as u32) * 2 + 1;
        self.doorbell(slot as u32, db);
        XHCI_REARM.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    /// Clear a HALT on an endpoint: RESET_ENDPOINT, then SET_DEQUEUE_POINTER
    /// past the dead transfer (the controller rewinds dequeue to the failed
    /// TD on stall — without this, every retry re-executes the stale SETUP).
    fn recover_ep(&mut self, slot: u8, ep_id: u32, deq: u64) {
        let ctl = (TRB_RESET_EP << 10) | ((slot as u32) << 24) | (ep_id << 16);
        let r1 = self.submit_cmd(0, 0, ctl);
        crate::dprintln!("[XHCI] reset_ep slot={} ep={} -> {:?}", slot, ep_id, r1.map(|(_, c)| c));
        let ctl = (TRB_SET_DEQ << 10) | ((slot as u32) << 24) | (ep_id << 16);
        let r2 = self.submit_cmd(deq & !0xF | (deq & 1), 0, ctl);
        crate::dprintln!("[XHCI] set_deq -> {:?}", r2.map(|(_, c)| c));
    }

    // ── Commands ─────────────────────────────────────────────────────────

    fn submit_cmd(&mut self, param: u64, status: u32, control: u32) -> Option<(u8, u8)> {
        let trb_addr = self.cmd_ring.push_trb(param, status, control);
        self.doorbell(0, 0);

        let deadline = 50_000_000usize;
        for _ in 0..deadline {
            if let Some((p, s, c)) = self.next_event() {
                if (c >> 10) & 0x3F == TRB_CMD_COMPLETION && p == trb_addr {
                    return Some(((c >> 24) as u8, (s >> 24) as u8));
                }
            }
        }
        let sts = self.op_rd(OP_USBSTS);
        let crcr_lo = self.op_rd(OP_CRCR);
        let ir = self.rt_base + RT_IR0;
        let ev0 = unsafe {
            let p = (HHDM_BASE.0 + self.event_ring.phys().0) as *const u64;
            (core::ptr::read_volatile(p), core::ptr::read_volatile(p.add(1)))
        };
        crate::dprintln!(
            "[XHCI] cmd timeout: hch={} cnr={} crcr={:#x} iman={:#x} erdp={:#x} ev0={:#x}/{:#x}",
            sts & 1, (sts >> 10) & 1,
            ((self.op_rd(OP_CRCR + 4) as u64) << 32) | crcr_lo as u64,
            self.rd(ir),
            ((self.rd(ir + IR_ERDP + 4) as u64) << 32) | self.rd(ir + IR_ERDP) as u64,
            ev0.0, ev0.1
        );
        None
    }

    /// Synchronous control transfer on EP0 of `slot`. `data` is in/out.
    fn ctrl_xfer(
        &mut self,
        slot: u8,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        w_length: u16,
        data: &mut [u8],
    ) -> bool {
        let dir_in = bm_request_type & 0x80 != 0;
        let len = w_length as usize;
        let dev = match self.devices.get_mut(slot as usize).and_then(|d| d.as_mut()) {
            Some(d) => d,
            None => return false,
        };

        let stage_buf = if len > 0 {
            match DmaBuf::new(len.max(64)) {
                Some(b) => Some(b),
                None => return false,
            }
        } else {
            None
        };
        if let (Some(buf), false) = (&stage_buf, dir_in) {
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buf.as_mut_ptr(), len) };
        }

        // SETUP (immediate data).
        let setup_param = bm_request_type as u64
            | (b_request as u64) << 8
            | (w_value as u64) << 16
            | (w_index as u64) << 32
            | (len as u64) << 48;
        let c = (TRB_SETUP_STAGE << 10) | F_IDT | if len > 0 { F_CH } else { 0 };
        dev.ep0.push_trb(setup_param, 8, c);

        // DATA stage (only when there is a payload).
        if len > 0 {
            let buf = stage_buf.as_ref().unwrap();
            let c = (TRB_DATA_STAGE << 10) | F_CH | if dir_in { F_DIR_IN } else { 0 };
            dev.ep0.push_trb(buf.phys().0, len as u32, c);
        }

        // STATUS stage, opposite direction, IOC.
        let sc = (TRB_STATUS_STAGE << 10)
            | if dir_in { 0 } else { F_DIR_IN }
            | F_IOC;
        dev.ep0.push_trb(0, 0, sc);
        self.doorbell(slot as u32, 1); // control EP doorbell target = 1

        let deadline = 50_000_000usize;
        for _ in 0..deadline {
            if let Some((_p, s, c)) = self.next_event() {
                if (c >> 10) & 0x3F != TRB_TRANSFER_EVENT {
                    continue;
                }
                let cc = (s >> 24) as u8;
                if cc != CC_SUCCESS && cc != CC_SHORT_PACKET {
                    crate::dprintln!("[XHCI] ctrl_xfer cc={} req={:#x}", cc, b_request);
                    // A STALL halts EP0 and rewinds its ring; point it past
                    // the dead transfer so later transfers work.
                    let deq = self
                        .devices
                        .get_mut(slot as usize)
                        .and_then(|d| d.as_ref())
                        .map(|d| d.ep0.deq_with_cycle())
                        .unwrap_or(0);
                    self.recover_ep(slot, 1, deq);
                    return false;
                }
                if let Some(buf) = stage_buf.as_ref() {
                    if dir_in && len > 0 {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                buf.as_mut_ptr(),
                                data.as_mut_ptr(),
                                len,
                            );
                        }
                    }
                }
                return true;
            }
        }
        crate::dprintln!("[XHCI] ctrl_xfer timeout req={:#x}", b_request);
        false
    }
}

// ── Init & enumeration ───────────────────────────────────────────────────────

const PCI_MAX_BUSES: u8 = 255;
const PCI_MAX_DEVICES: u8 = 32;
const PCI_MAX_FUNCTIONS: u8 = 8;

pub fn init() -> bool {
    let mut found = false;
    for bus in 0..PCI_MAX_BUSES {
        for device in 0..PCI_MAX_DEVICES {
            for function in 0..PCI_MAX_FUNCTIONS {
                let addr = PciAddress::new(bus, device, function);
                if !addr.present() {
                    continue;
                }
                let (base, sub, prog_if) = addr.class_code();
                if base == 0x0C && sub == 0x03 {
                    crate::dprintln!("[XHCI] probe bus {} dev {} fn {}: prog_if={:#x}", bus, device, function, prog_if);
                }
                if base == 0x0C && sub == 0x03 && prog_if == 0x30 {
                    found |= init_controller(addr);
                }
            }
        }
    }
    XHCI_PRESENT.store(found, Ordering::Relaxed);
    found
}

fn init_controller(addr: PciAddress) -> bool {
    crate::dprintln!("[XHCI] init_controller: probing");
    addr.enable_memory_and_master();
    let mmio = match addr.map_bar(0) {
        Ok((v, _)) => v,
        Err(e) => {
            crate::dprintln!("[XHCI] map_bar failed: {}", e);
            return false;
        }
    };

    unsafe {
        let caplength = core::ptr::read_volatile(mmio.0 as *const u8) as usize;
        let hcs1 = core::ptr::read_volatile((mmio.0 + HCSPARAMS1 as u64) as *const u32);
        let hcc1 = core::ptr::read_volatile((mmio.0 + HCCPARAMS1 as u64) as *const u32);
        // DBOFF/RTSOFF are already byte offsets (bits [3:0] reserved).
        let dboff =
            core::ptr::read_volatile((mmio.0 + DBOFF as u64) as *const u32) as usize & !0xF;
        let rtsoff =
            core::ptr::read_volatile((mmio.0 + RTSOFF as u64) as *const u32) as usize & !0xF;

        let op_base = caplength;
        let cmd_reg = mmio.0 + (op_base + OP_USBCMD) as u64;
        let sts_reg = mmio.0 + (op_base + OP_USBSTS) as u64;

        // HCRST, wait for it to complete.
        core::ptr::write_volatile((cmd_reg as *mut u32), core::ptr::read_volatile(cmd_reg as *const u32) | USBCMD_HCRST);
        let mut n = 0;
        while core::ptr::read_volatile(cmd_reg as *const u32) & USBCMD_HCRST != 0 && n < 1_000_000 {
            core::hint::spin_loop();
            n += 1;
        }
        n = 0;
        while core::ptr::read_volatile(sts_reg as *const u32) & USBSTS_CNR != 0 && n < 1_000_000 {
            core::hint::spin_loop();
            n += 1;
        }
        if n >= 1_000_000 {
            crate::dprintln!("[XHCI] controller reset timeout");
            return false;
        }

        crate::dprintln!("[XHCI] caps ok: ports={} slots={} csz={}", (hcs1 >> 24) as usize, hcs1 & 0xFF, if hcc1 & 4 != 0 {64} else {32});
        if hcc1 & 1 == 0 {
            crate::dprintln!("[XHCI] WARNING: controller lacks 64-bit DMA (AC64=0); buffers above 4GB will corrupt");
        }
        let pagesz = core::ptr::read_volatile((mmio.0 + op_base as u64 + OP_PAGESIZE as u64) as *const u32);
        crate::dprintln!("[XHCI] pagesize reg={:#x}", pagesz);
        crate::dprintln!(
            "[XHCI] caps: caplength={:#x} dboff={:#x} rtsoff={:#x}",
            caplength, dboff, rtsoff
        );
        let max_slots = (hcs1 & 0xFF) as usize;
        let max_ports = (hcs1 >> 24) as usize;
        let ctx_size = if hcc1 & (1 << 2) != 0 { 64 } else { 32 };

        let dcbaa = match DmaBuf::new_aligned(4096, 64 * 1024) {
            Some(b) => b,
            None => {
                crate::dprintln!("[XHCI] dcbaa alloc failed");
                return false;
            }
        };
        let input_ctx = match DmaBuf::new_aligned(4096, 64 * 1024) {
            Some(b) => b,
            None => {
                crate::dprintln!("[XHCI] input_ctx alloc failed");
                return false;
            }
        };
        let cmd_ring = match Ring::new() {
            Some(r) => r,
            None => {
                crate::dprintln!("[XHCI] cmd_ring alloc failed");
                return false;
            }
        };
        let event_ring = match DmaBuf::new_aligned(TRBS_PER_RING * 16, 64 * 1024) {
            Some(b) => b,
            None => {
                crate::dprintln!("[XHCI] event_ring alloc failed");
                return false;
            }
        };

        let mut hc = Xhci {
            mmio,
            op_base,
            rt_base: rtsoff,
            db_base: mmio.0 as usize + dboff,
            max_ports,
            ctx_size,
            dcbaa,
            input_ctx,
            cmd_ring,
            event_ring,
            event_cycle: 1,
            event_idx: 0,
            devices: Default::default(),
            msi_x: false,
        };

        // Check for MSI-X capability (0x11).
        let mut msi_x_enabled = false;
        if let Some(cap_off) = addr.find_capability(0x11) {
            let ctrl = addr.read_config_u16(cap_off as u32 + 2);
            let table_info = addr.read_config_u32(cap_off as u32 + 4);
            let table_bir = (table_info & 0x7) as u32;
            let table_offset = (table_info & !0x7) as usize;

            crate::dprintln!(
                "[XHCI] MSI-X cap found at {:#x}: ctrl={:#x}, bir={}, offset={:#x}",
                cap_off, ctrl, table_bir, table_offset
            );

            // Table BIR is typically BAR 0 (or another memory BAR).
            let table_vaddr = if table_bir == 0 {
                Some(mmio.0 + table_offset as u64)
            } else {
                addr.map_bar(table_bir).ok().map(|(v, _)| v.0 + table_offset as u64)
            };

            if let Some(tbl_ptr) = table_vaddr {
                // Table Entry 0:
                // Offset 0: Message Address (Lower 32 bits)
                // Offset 4: Message Upper Address
                // Offset 8: Message Data
                // Offset 12: Vector Control (bit 0 = Mask)
                let lapic_id = crate::arch::x86_64::apic::lapic_id();
                let msg_addr = 0xFEE0_0000u64 | ((lapic_id as u64) << 12);
                let msg_data = crate::interrupts::XHCI_INTERRUPT_ID as u32;

                unsafe {
                    let entry_ptr = tbl_ptr as *mut u32;
                    core::ptr::write_volatile(entry_ptr.add(0), msg_addr as u32);
                    core::ptr::write_volatile(entry_ptr.add(1), (msg_addr >> 32) as u32);
                    core::ptr::write_volatile(entry_ptr.add(2), msg_data);
                    core::ptr::write_volatile(entry_ptr.add(3), 0); // Unmask entry 0
                }

                // Enable MSI-X in PCI capability (bit 15 = MSI-X Enable, bit 14 = Function Mask clear)
                let new_ctrl = (ctrl | (1 << 15)) & !(1 << 14);
                unsafe { addr.write_config_u16(cap_off as u32 + 2, new_ctrl) };

                msi_x_enabled = true;
                hc.msi_x = true;
                crate::dprintln!("[XHCI] MSI-X enabled: lapic_id={} vector={:#x}", lapic_id, msg_data);
            }
        }

        hc.op_wr(OP_CONFIG, max_slots.min(MAX_SLOTS) as u32);

        let p = hc.dcbaa.phys().0;
        hc.op_wr(OP_DCBAAP, p as u32);
        hc.op_wr(OP_DCBAAP + 4, (p >> 32) as u32);

        crate::dprintln!("[XHCI] stage: regs written");
        // CRCR with Ring Cycle State = 1 (initial cycle).
        let crcr = hc.cmd_ring.phys() | 1;
        hc.op_wr(OP_CRCR, crcr as u32);
        hc.op_wr(OP_CRCR + 4, (crcr >> 32) as u32);

        // Event ring segment table (single segment).
        let erst = match DmaBuf::new_aligned(64, 64 * 1024) {
            Some(b) => b,
            None => {
                crate::dprintln!("[XHCI] erst alloc failed");
                return false;
            }
        };
        unsafe {
            let e = erst.as_mut_ptr() as *mut u64;
            let seg = hc.event_ring.phys().0;
            core::ptr::write_volatile(e, seg);
            core::ptr::write_volatile(e.add(1), TRBS_PER_RING as u64);
        }
        let ir = hc.rt_base + RT_IR0;
        // If MSI-X is enabled, set Interrupt Enable (bit 1) and ack pending (bit 0); otherwise polled (IE=0).
        let iman_val = if msi_x_enabled { 3 } else { 1 };
        hc.wr32(ir + IR_IMAN, iman_val);
        hc.wr32(ir + IR_ERSTSZ, 1);
        let ba = erst.phys().0;
        hc.wr32(ir + IR_ERSTBA, ba as u32);
        hc.wr32(ir + IR_ERSTBA + 4, (ba >> 32) as u32);
        let dp = hc.event_ring.phys().0;
        hc.wr32(ir + IR_ERDP, dp as u32);
        hc.wr32(ir + IR_ERDP + 4, (dp >> 32) as u32);

        crate::dprintln!("[XHCI] stage: rings programmed (iman={:#x})", iman_val);
        // Run (RS=1, and if MSI-X enabled, INTE=1).
        let usbcmd = hc.op_rd(OP_USBCMD) | USBCMD_RS | if msi_x_enabled { (1 << 2) /* INTE */ } else { 0 };
        hc.op_wr(OP_USBCMD, usbcmd);
        crate::dprintln!("[XHCI] controller running (usbcmd={:#x})", usbcmd);

        // Power ports, ack change bits.
        for i in 0..hc.max_ports {
            hc.portsc_set(i, PSC_PP, 0);
            hc.portsc_set(i, PSC_CSC | PSC_PEC, 0);
        }
        spin_delay_ms(100);

        crate::dprintln!("[XHCI] stage: ports powered");
        let mut any = false;
        for i in 0..hc.max_ports {
            if hc.portsc(i) & PSC_CCS == 0 {
                continue;
            }
            if enumerate_port(&mut hc, i) {
                finish_attach(&mut hc, i);
                any = true;
                crate::dprintln!("[XHCI] port {}: HID device online", i + 1);
            }
        }

        if !any {
            crate::dprintln!("[XHCI] no devices attached");
        }
        *XHCI.lock() = Some(hc);
        true
    }
}

fn spin_delay_ms(ms: u64) {
    // ponytail: TICKS isn't ticking yet during early boot (pci scan runs
    // before lapic_timer_start), so use a calibrated busy-wait.
    // ~200k iterations ≈ 1ms at typical TSC speeds under KVM.
    let iters = ms * 200_000;
    for _ in 0..iters {
        core::hint::spin_loop();
    }
}

/// Reset port, assign a slot, address the device, read descriptors,
/// SET_CONFIG, switch to boot protocol, open the interrupt IN endpoint.
fn enumerate_port(hc: &mut Xhci, port: usize) -> bool {
    // Reset.
    hc.portsc_set(port, PSC_PR, 0);
    spin_delay_ms(30);
    let mut enabled = false;
    for _ in 0..20 {
        if hc.portsc(port) & PSC_PED != 0 {
            enabled = true;
            break;
        }
        spin_delay_ms(5);
    }
    if !enabled {
        crate::dprintln!("[XHCI] port {}: no enable after reset", port + 1);
        return false;
    }
    let speed = (hc.portsc(port) >> PSC_SPEED_SHIFT) & 0xF;

    // Enable slot.
    let (slot, cc) = match hc.submit_cmd(0, 0, TRB_ENABLE_SLOT << 10) {
        Some(v) => v,
        None => return false,
    };
    if cc != CC_SUCCESS || slot == 0 || slot as usize >= MAX_SLOTS {
        crate::dprintln!("[XHCI] enable_slot cc={}", cc);
        return false;
    }

    // Output device context page for this slot.
    let dev_ctx = match DmaBuf::new_aligned(4096, 64 * 1024) {
        Some(b) => b,
        None => return false,
    };
    unsafe {
        let e = (hc.dcbaa.as_mut_ptr() as *mut u64).add(slot as usize);
        core::ptr::write_volatile(e, dev_ctx.phys().0);
    }

    let mps0: u32 = match speed {
        1 => 8,   // low-speed
        2 => 64,  // full-speed
        3 => 64,  // high-speed
        _ => 512, // super-speed
    };

    // EP0 transfer ring must exist before ADDRESS_DEVICE (its dequeue goes
    // into the input context).
    let ep0_ring = match Ring::new() {
        Some(r) => r,
        None => return false,
    };

    // ADDRESS_DEVICE with input context: slot ctx (A0) + EP0 ctx (A1).
    hc.clear_input_ctx();
    {
        let ctx = hc.input_ctx();
        let deq = ep0_ring.deq_with_cycle();
        unsafe {
            ctx.wr32(ctx.off(0) + 4, 0b11); // ADD flags: slot + ep0
            // Slot context.
            ctx.wr32(ctx.off(1) + 0, 0); // route string
            ctx.wr32(ctx.off(1) + 4, (port as u32 + 1) << 16); // RH port
            ctx.wr32(
                ctx.off(1) + 8,
                (speed << 20) | (1 << 27),
            ); // speed, context entries = 1
            write_ep_ctx(&ctx, 2, mps0, 4, 0, 8, deq);
        }
    }
    let ip = hc.input_ctx.phys().0;
    match hc.submit_cmd(ip, 0, (TRB_ADDRESS_DEVICE << 10) | (slot as u32) << 24) {
        Some((_slot2, CC_SUCCESS)) => {}
        Some((_, cc)) => {
            crate::dprintln!("[XHCI] address_device cc={}", cc);
            return false;
        }
        None => return false,
    }

    let dev = UsbDevice {
        kind: DevKind::Unknown,
        port,
        evdev_minor: usize::MAX,
        ep0: ep0_ring,
        int_ring: None,
        int_ep_addr: 0,
        iface_num: 0,
        report_buf: None,
        report_len: 0,
        last_mods: 0,
        last_keys: [0; 6],
        last_buttons: 0,
        last_x: 0,
        last_y: 0,
        last_pos_valid: false,
        rem_x: 0,
        rem_y: 0,
        layout: None,
    };
    hc.devices[slot as usize] = Some(dev);

    // Read first 8 bytes of the device descriptor → bMaxPacketSize0.
    let mut dd = [0u8; 8];
    if !hc.ctrl_xfer(slot, 0x80, 0x06, 0x0100, 0, 8, &mut dd) {
        return false;
    }
    let real_mps = dd[7] as u32;
    if real_mps > 0 && real_mps != mps0 {
        hc.clear_input_ctx();
        let ctx = hc.input_ctx();
        let deq = hc.devices[slot as usize]
            .as_ref()
            .map(|d| d.ep0.deq_with_cycle())
            .unwrap_or(0);
        unsafe {
            ctx.wr32(ctx.off(0) + 4, 0b11);
            ctx.wr32(ctx.off(1) + 8, (speed << 20) | (1 << 27));
            write_ep_ctx(&ctx, 2, real_mps, 4, 0, 8, deq);
        }
        let ip = hc.input_ctx.phys().0;
        let _ = hc.submit_cmd(ip, 0, (TRB_EVALUATE_CTX << 10) | (slot as u32) << 24);
    }

    // Full device descriptor (for logging/class info).
    let mut dd18 = [0u8; 18];
    let _ = hc.ctrl_xfer(slot, 0x80, 0x06, 0x0100, 0, 18, &mut dd18);

    // Config descriptor header → total length.
    let mut cfg_hdr = [0u8; 9];
    if !hc.ctrl_xfer(slot, 0x80, 0x06, 0x0200, 0, 9, &mut cfg_hdr) {
        return false;
    }
    let total = u16::from_le_bytes([cfg_hdr[2], cfg_hdr[3]]) as usize;
    let total = total.clamp(9, 512);
    let mut cfg = alloc::vec![0u8; total];
    if !hc.ctrl_xfer(slot, 0x80, 0x06, 0x0200, 0, total as u16, &mut cfg) {
        return false;
    }

    // Walk descriptors: find HID interface + its interrupt-IN endpoint and HID descriptor.
    let mut iface_num: u8 = 0;
    let mut iface_proto: u8 = 0;
    let mut iface_sub: u8 = 0;
    let mut hid_iface = false;
    let mut ep_addr: u8 = 0;
    let mut ep_interval: u8 = 8;
    let mut hid_report_desc_len: usize = 0;
    let mut i = 0usize;
    while i + 2 <= cfg.len() {
        let dlen = cfg[i] as usize;
        let dtype = cfg[i + 1];
        if dlen < 2 || i + dlen > cfg.len() {
            break;
        }
        match dtype {
            0x04 => {
                // Interface descriptor
                hid_iface = cfg[i + 5] == 0x03; // bInterfaceClass == HID
                iface_num = cfg[i + 2];
                iface_proto = cfg[i + 7]; // 1=boot keyboard, 2=boot mouse
                iface_sub = cfg[i + 6];   // 1=boot keyboard, 2=boot mouse
            }
            0x21 => {
                // HID descriptor: wDescriptorLength at offset 7..9
                if hid_iface && dlen >= 9 {
                    hid_report_desc_len = u16::from_le_bytes([cfg[i + 7], cfg[i + 8]]) as usize;
                }
            }
            0x05 => {
                // Endpoint descriptor
                let attr = cfg[i + 3];
                let addr_ = cfg[i + 2];
                if hid_iface && attr & 0x03 == 0x03 && addr_ & 0x80 != 0 {
                    ep_addr = addr_;
                    ep_interval = cfg[i + 6];
                }
            }
            _ => {}
        }
        i += dlen;
    }
    if ep_addr == 0 {
        crate::dprintln!("[XHCI] slot {}: no HID interrupt-IN endpoint", slot);
        return false;
    }

    // Classify from descriptor bytes only (no GET_PROTOCOL probe: unsupported
    // class requests STALL EP0 and complicate recovery).
    // ponytail: boot subclass(1)+proto decides; old (1,_)->kbd arm swallowed boot mice.
    let boot_kind = match (iface_sub, iface_proto) {
        (1, 2) => DevKind::Mouse, // boot mouse (usb-mouse: sub=1, proto=2)
        (1, _) => DevKind::Keyboard,
        (_, 2) => DevKind::Mouse,
        _ => DevKind::Tablet, // no boot protocol: absolute HID pointer
    };
    let boot_report_len = match boot_kind {
        DevKind::Keyboard => 8,
        DevKind::Mouse => 4,
        DevKind::Tablet => 8,
        DevKind::Unknown => 8,
    };

    // SET_CONFIGURATION(1).
    if !hc.ctrl_xfer(slot, 0x00, 0x09, 1, 0, 0, &mut []) {
        let mut probe = [0u8; 1];
        let proto_ok = hc.ctrl_xfer(slot, 0xA1, 0x03, 0, iface_num as u16, 1, &mut probe);
        crate::dprintln!(
            "[XHCI] slot {}: SET_CONFIG failed (post-probe GET_PROTOCOL ok={} val={})",
            slot, proto_ok, probe[0]
        );
    }

    // Fetch and parse HID report descriptor if advertised
    let mut parsed_layout: Option<ReportLayout> = None;
    if hid_report_desc_len > 0 {
        let rlen = hid_report_desc_len.clamp(1, 512);
        let mut rdesc = alloc::vec![0u8; rlen];
        if hc.ctrl_xfer(
            slot,
            0x81,
            0x06,
            0x2200,
            iface_num as u16,
            rlen as u16,
            &mut rdesc,
        ) {
            if let Some(layout) = parse_report_map(&rdesc) {
                crate::dprintln!(
                    "[XHCI] slot {}: report layout {} fields {} bits (kbd={} ptr={} abs={})",
                    slot,
                    layout.fields.len(),
                    layout.total_bits,
                    layout.has_keyboard,
                    layout.has_pointer,
                    layout.is_absolute,
                );
                parsed_layout = Some(layout);
            } else {
                crate::dprintln!("[XHCI] slot {}: report-desc parse failed, boot fallback", slot);
            }
        } else {
            crate::dprintln!("[XHCI] slot {}: GET_DESCRIPTOR(HID_REPORT) failed, boot fallback", slot);
        }
    }

    let (kind, report_len) = if let Some(ref layout) = parsed_layout {
        let k = if layout.has_keyboard {
            DevKind::Keyboard
        } else if layout.has_pointer {
            if layout.is_absolute {
                DevKind::Tablet
            } else {
                DevKind::Mouse
            }
        } else {
            boot_kind
        };
        let rlen = ((layout.total_bits as usize + 7) / 8).max(boot_report_len);
        (k, rlen)
    } else {
        crate::dprintln!("[XHCI] slot {}: report-desc parse failed, boot fallback", slot);
        // SET_PROTOCOL(boot) — class request to interface.
        let mut none = [];
        let _ = hc.ctrl_xfer(slot, 0x21, 0x0B, 0, iface_num as u16, 0, &mut none);
        (boot_kind, boot_report_len)
    };

    // Open the interrupt IN endpoint via CONFIG_ENDPOINT.
    let report_buf = match DmaBuf::new_aligned(64, 64 * 1024) {
        Some(b) => b,
        None => return false,
    };
    let int_ring = match Ring::new() {
        Some(r) => r,
        None => return false,
    };

    hc.clear_input_ctx();
    {
        let ctx = hc.input_ctx();
        let ep_index = 2 * (ep_addr & 0x0F) as usize + 1; // IN ep1 → 3
        let deq = int_ring.deq_with_cycle();
        unsafe {
            ctx.wr32(ctx.off(0) + 4, (1 << ep_index) | 1); // add slot + this EP
            ctx.wr32(
                ctx.off(1) + 8,
                (speed << 20) | (((ep_index + 1) as u32) << 27),
            );
            // Input-side EP contexts sit one index above the completion
            // index (ctrl=0, slot=1, ep0=2, ...): QEMU reads EP comp-index i
            // at ictx + 32*(i+1).
            // Context interval encoding differs by bus speed:
            // FS/LS: period in 1ms frames. HS: exponent, period = 2^p μframes
            // while the descriptor already encodes 2^(bInterval-1).
            let ivf = match speed {
                3 => ep_interval.saturating_sub(1) as u32,
                _ => ep_interval.max(1) as u32,
            };
            write_ep_ctx(&ctx, ep_index + 1, 8, 7 /*interrupt IN*/, ivf, 8, deq);
        }
    }
    let ip = hc.input_ctx.phys().0;
    match hc.submit_cmd(ip, 0, (TRB_CONFIG_EP << 10) | (slot as u32) << 24) {
        Some((_, CC_SUCCESS)) => {}
        Some((_, cc)) => {
            crate::dprintln!("[XHCI] config_ep cc={}", cc);
            return false;
        }
        None => return false,
    }

    // Fill device record, queue the first NORMAL TRB, ring the doorbell.
    {
        unsafe { core::ptr::write_bytes(report_buf.as_mut_ptr(), 0, 64) };
        let dev = hc.devices[slot as usize].as_mut().unwrap();
        dev.kind = kind;
        dev.int_ep_addr = ep_addr;
        dev.iface_num = iface_num;
        dev.report_buf = Some(report_buf);
        dev.report_len = report_len;
        dev.int_ring = Some(int_ring);
        dev.layout = parsed_layout;
    }
    // Arm two reports so a keystroke landing right after CONFIG_EP isn't
    // NAK'd while the timer refills.
    hc.refill_int_ring(slot);
    hc.refill_int_ring(slot);

    crate::dprintln!(
        "[XHCI] slot {} port {}: {:?} (ep {:#x}, interval {})",
        slot, port + 1, kind, ep_addr, ep_interval
    );
    true
}

// ponytail: process-context tail of attach (boot init + service_hotplug):
// allocates this device's own evdev node, registers sysfs, stores the minor.
// Never called from the timer tick — input_alloc_node broadcasts (heap +
// SCHEDULER) and sysfs_register_input leaks the devname.
fn finish_attach(hc: &mut Xhci, port: usize) {
    let (slot, kind) = match hc
        .devices
        .iter()
        .enumerate()
        .find_map(|(s, d)| d.as_ref().filter(|d| d.port == port).map(|d| (s, d.kind)))
    {
        Some(v) => v,
        None => return,
    };
    let (name, sys_caps, caps) = match kind {
        DevKind::Keyboard => (
            "dragos-usbkbd",
            crate::fs::sysfs::EVDEV_CAPS_KBD,
            crate::drivers::keyboard::kbd_caps("dragos-usbkbd"),
        ),
        DevKind::Mouse => (
            "dragos-usbmouse",
            crate::fs::sysfs::EVDEV_CAPS_MOUSE,
            crate::drivers::keyboard::mouse_caps("dragos-usbmouse"),
        ),
        DevKind::Tablet => (
            "dragos-usbtablet",
            crate::fs::sysfs::EVDEV_CAPS_MOUSE,
            crate::drivers::keyboard::tablet_caps("dragos-usbtablet", 32767, 32767),
        ),
        DevKind::Unknown => return,
    };
    let minor = crate::drivers::keyboard::input_alloc_node(caps);
    if let Some(d) = hc.devices[slot].as_mut() {
        d.evdev_minor = minor;
    }
    crate::fs::sysfs::sysfs_register_input(minor, sys_caps);
    crate::dprintln!(
        "[XHCI] port {}: {name} attached as event{minor} (minor {minor})",
        port + 1
    );
}

fn detect_kind(hc: &mut Xhci, slot: u8, iface: u8) -> DevKind {
    // HID descriptor protocol byte requires parsing the HID class desc; the
    // lazy equivalent: issue GET_REPORT-free probe via GET_PROTOCOL.
    let mut proto = [0u8; 1];
    if hc.ctrl_xfer(slot, 0xA1, 0x03, 0, iface as u16, 1, &mut proto) {
        match proto[0] {
            1 => return DevKind::Keyboard,
            2 => return DevKind::Mouse,
            _ => {}
        }
    }
    DevKind::Unknown
}

/// Write an endpoint context at context index `n`.
/// Spec layout: DW0=interval[23:16], DW1=MPS[31:16]|type[5:3],
/// DW2/3=TR dequeue ptr (+DCS bit0), DW4=avg TRB len[31:16].
fn write_ep_ctx(
    ctx: &Ctx,
    n: usize,
    mps: u32,
    ep_type: u32,
    interval: u32,
    avg_trb: u32,
    deq: u64,
) {
    let o = ctx.off(n);
    unsafe {
        ctx.wr32(o + 0, interval << 16);
        ctx.wr32(o + 4, (ep_type << 3) | (mps << 16));
        ctx.wr32(o + 8, ((deq & !0xF) as u32) | (deq & 1) as u32);
        ctx.wr32(o + 12, (deq >> 32) as u32);
        ctx.wr32(o + 16, avg_trb << 16);
    }
}

fn extract_bits(data: &[u8], bit_off: usize, bit_size: usize) -> u32 {
    let mut val: u32 = 0;
    for b in 0..bit_size {
        let bit_index = bit_off + b;
        let byte_index = bit_index / 8;
        let bit_in_byte = bit_index % 8;
        if byte_index < data.len() {
            let bit = (data[byte_index] >> bit_in_byte) & 1;
            val |= (bit as u32) << b;
        }
    }
    val
}

fn sign_extend(val: u32, bit_size: u8) -> i32 {
    if bit_size == 0 || bit_size >= 32 {
        return val as i32;
    }
    let sign_bit = 1u32 << (bit_size - 1);
    if val & sign_bit != 0 {
        (val | !(sign_bit - 1)) as i32
    } else {
        val as i32
    }
}

// ── Report decoding & evdev injection ────────────────────────────────────────

impl UsbDevice {
    /// Copy the delivered report out of DMA and push evdev events (+stdin).
    fn deliver_report(&mut self) {
        let buf = match self.report_buf.as_ref() {
            Some(b) => b,
            None => return,
        };
        let len = self.report_len.min(buf.len());
        let mut report = [0u8; 64];
        let n = len.min(report.len());
        unsafe { core::ptr::copy_nonoverlapping(buf.as_mut_ptr(), report.as_mut_ptr(), n) };

        if let Some(layout) = self.layout.clone() {
            decode_layout(self, &report[..n], &layout);
            return;
        }

        match self.kind {
            DevKind::Keyboard => decode_keyboard(self, &report[..n]),
            DevKind::Mouse => decode_mouse(self, &report[..n]),
            DevKind::Tablet => decode_tablet(self, &report[..n]),
            DevKind::Unknown => {}
        }
    }
}

fn decode_layout(dev: &mut UsbDevice, report: &[u8], layout: &ReportLayout) {
    const EV_KEY: u16 = 1;
    const EV_REL: u16 = 2;
    const MOD_LSHIFT: u8 = 1 << 1;
    const MOD_RSHIFT: u8 = 1 << 5;
    const BTN_LEFT: u16 = 0x110;
    const BTN_RIGHT: u16 = 0x111;
    const BTN_MIDDLE: u16 = 0x112;
    const BTN_SIDE: u16 = 0x113;
    const BTN_EXTRA: u16 = 0x114;

    let mut current_mods: u8 = 0;
    let mut current_keys: [u8; 6] = [0; 6];
    let mut key_count = 0;

    let mut buttons: u8 = 0;
    let mut rel_dx: i32 = 0;
    let mut rel_dy: i32 = 0;
    let mut abs_x: Option<i32> = None;
    let mut abs_y: Option<i32> = None;
    let mut abs_x_max: i32 = 32767;
    let mut abs_y_max: i32 = 32767;
    let mut wheel: i32 = 0;

    let mut has_kbd_fields = false;
    let mut has_pointer_fields = false;

    for field in &layout.fields {
        let bit_off = field.bit_off as usize;
        let bit_size = field.bit_size as usize;
        if bit_size == 0 {
            continue;
        }

        match field.usage_page {
            0x07 => {
                // Keyboard / Keypad
                has_kbd_fields = true;
                if field.is_variable && field.bit_size == 1 {
                    // Modifier bit
                    let val = extract_bits(report, bit_off, 1);
                    if (0xE0..=0xE7).contains(&field.usage) {
                        let mod_idx = (field.usage - 0xE0) as u8;
                        if val != 0 {
                            current_mods |= 1 << mod_idx;
                        }
                    }
                } else if !field.is_variable {
                    // Array of keys (e.g. 8-bit usage value)
                    let usage_val = extract_bits(report, bit_off, bit_size) as u8;
                    if usage_val >= 4 && key_count < 6 {
                        current_keys[key_count] = usage_val;
                        key_count += 1;
                    }
                }
            }
            0x09 => {
                // Button page
                has_pointer_fields = true;
                let val = extract_bits(report, bit_off, bit_size.min(1));
                let btn_idx = field.usage.saturating_sub(1);
                if btn_idx < 5 && val != 0 {
                    buttons |= 1 << btn_idx;
                }
            }
            0x01 => {
                // Generic Desktop
                match field.usage {
                    0x30 => {
                        // X
                        has_pointer_fields = true;
                        if field.logical_max > 255 || field.logical_min < -128 {
                            // Absolute X
                            let raw = extract_bits(report, bit_off, bit_size);
                            abs_x = Some(raw as i32);
                            abs_x_max = (field.logical_max - field.logical_min).max(1);
                        } else {
                            // Relative X
                            let raw = extract_bits(report, bit_off, bit_size);
                            rel_dx += sign_extend(raw, field.bit_size);
                        }
                    }
                    0x31 => {
                        // Y
                        has_pointer_fields = true;
                        if field.logical_max > 255 || field.logical_min < -128 {
                            // Absolute Y
                            let raw = extract_bits(report, bit_off, bit_size);
                            abs_y = Some(raw as i32);
                            abs_y_max = (field.logical_max - field.logical_min).max(1);
                        } else {
                            // Relative Y (screen Y down -> invert standard USB HID)
                            let raw = extract_bits(report, bit_off, bit_size);
                            rel_dy -= sign_extend(raw, field.bit_size);
                        }
                    }
                    0x38 => {
                        // Wheel
                        has_pointer_fields = true;
                        let raw = extract_bits(report, bit_off, bit_size);
                        wheel += sign_extend(raw, field.bit_size);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // Process keyboard events
    if has_kbd_fields || layout.has_keyboard {
        KBD_REPORT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let modifiers: [(u8, u16); 8] = [
            (1 << 0, 29),  // LCtrl
            (1 << 1, 42),  // LShift
            (1 << 2, 56),  // LAlt
            (1 << 3, 125), // LGUI
            (1 << 4, 97),  // RCtrl
            (1 << 5, 54),  // RShift
            (1 << 6, 100), // RAlt
            (1 << 7, 126), // RGUI
        ];
        for (bit, code) in modifiers.iter() {
            let was = dev.last_mods & bit != 0;
            let now = current_mods & bit != 0;
            if was != now {
                push_event(EV_KEY, *code, now as i32, dev.evdev_minor);
            }
        }
        for k in current_keys.iter() {
            if *k >= 4 && !dev.last_keys.contains(k) {
                press_key(*k, current_mods & (MOD_LSHIFT | MOD_RSHIFT) != 0, dev.evdev_minor);
            }
        }
        for k in dev.last_keys.iter() {
            if *k >= 4 && !current_keys.contains(k) {
                let code = hid_to_linux(*k);
                crate::dprintln!("[kbd-usb] release hid={:#02x} linux_code={} dev={}", *k, code, dev.evdev_minor);
                push_event(EV_KEY, code, 0, dev.evdev_minor);
            }
        }
        dev.last_mods = current_mods;
        dev.last_keys = current_keys;
    }

    // Process pointer events (mouse / tablet)
    if has_pointer_fields || layout.has_pointer {
        if layout.is_absolute || abs_x.is_some() || abs_y.is_some() {
            TABLET_REPORT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        } else {
            MOUSE_REPORT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }

        // Buttons
        let edges: [(u8, u16); 5] = [
            (1 << 0, BTN_LEFT),
            (1 << 1, BTN_RIGHT),
            (1 << 2, BTN_MIDDLE),
            (1 << 3, BTN_SIDE),
            (1 << 4, BTN_EXTRA),
        ];
        for (bit, code) in edges.iter() {
            let was = dev.last_buttons & bit != 0;
            let now = buttons & bit != 0;
            if was != now {
                push_event(EV_KEY, *code, now as i32, dev.evdev_minor);
            }
        }
        dev.last_buttons = buttons;

        // Motion
        if let (Some(x), Some(y)) = (abs_x, abs_y) {
            const EV_ABS: u16 = 3;
            push_event(EV_ABS, 0 /* ABS_X */, x, dev.evdev_minor);
            push_event(EV_ABS, 1 /* ABS_Y */, y, dev.evdev_minor);

            if dev.last_pos_valid {
                let (sw, sh) = unsafe {
                    crate::graphics::FRAMEBUFFER_INFO
                        .map(|f| (f.width as i32, f.height as i32))
                        .unwrap_or((1280, 800))
                };
                dev.rem_x += (x - dev.last_x as i32) * sw;
                dev.rem_y += (y - dev.last_y as i32) * sh;
                let dx = dev.rem_x.div_euclid(abs_x_max);
                let dy = dev.rem_y.div_euclid(abs_y_max);
                dev.rem_x -= dx * abs_x_max;
                dev.rem_y -= dy * abs_y_max;
                if dx != 0 {
                    push_event(EV_REL, 0, dx, dev.evdev_minor);
                }
                if dy != 0 {
                    push_event(EV_REL, 1, dy, dev.evdev_minor);
                }
            }
            dev.last_x = x as u16;
            dev.last_y = y as u16;
            dev.last_pos_valid = true;
        } else {
            if rel_dx != 0 {
                push_event(EV_REL, 0, rel_dx, dev.evdev_minor);
            }
            if rel_dy != 0 {
                push_event(EV_REL, 1, rel_dy, dev.evdev_minor);
            }
        }

        if wheel != 0 {
            push_event(EV_REL, 8 /* REL_WHEEL */, -wheel, dev.evdev_minor);
        }
    }

    push_syn(dev.evdev_minor);
}


/// EV_SYN/SYN_REPORT terminates an evdev report frame; libevdev withholds
/// events from libinput until it sees one.
fn push_syn(dev: usize) {
    crate::drivers::keyboard::input_push(
        crate::drivers::keyboard::InputEvent {
            time_sec: 0,
            time_usec: 0,
            type_: 0x00,
            code: 0x00,
            value: 0,
        },
        dev,
    );
    // input_push already wakes INPUT_WAIT_QUEUE and PIPE_WAIT_QUEUE.
}

fn push_event(type_: u16, code: u16, value: i32, dev: usize) {
    crate::drivers::keyboard::input_push(
        crate::drivers::keyboard::InputEvent {
            time_sec: 0,
            time_usec: 0,
            type_,
            code,
            value,
        },
        dev,
    );
    // input_push already wakes INPUT_WAIT_QUEUE and PIPE_WAIT_QUEUE.
}

fn wake_stdin_waiters() {
    let mut sched = crate::tasks::SCHEDULER.lock();
    let mut wq = crate::tasks::KEYBOARD_WAIT_QUEUE.lock();
    while let Some(mut t) = wq.pop_front() {
        t.pending_block = None;
        sched.enqueue_with_boost(t, 4);
    }
}

fn push_char(c: char) {
    // Through the line discipline into the active VT's queue, like the PS/2
    // keyboard: echo, canonical editing and signal characters must behave the
    // same whichever keyboard produced the keystroke.
    let action = crate::tty::input_active(c as u8);
    let vt = crate::graphics::vt::active_vt_number();
    wake_stdin_waiters();
    crate::tty::dispatch(action, vt);
}fn decode_keyboard(dev: &mut UsbDevice, report: &[u8]) {
    if report.len() < 8 {
        KBD_REPORT_DROP.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return;
    }
    KBD_REPORT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    const EV_KEY: u16 = 1;
    const MOD_LSHIFT: u8 = 1 << 1;
    const MOD_RSHIFT: u8 = 1 << 5;

    let mods = report[0];
    let keys: [u8; 6] = [
        report[2], report[3], report[4], report[5], report[6], report[7],
    ];

    // HID modifier bits → Linux keycodes
    // (LCtrl=29, LShift=42, LAlt=56, LGUI=125, RCtrl=97, RShift=54, RAlt=100, RGUI=126).
    let modifiers: [(u8, u16); 8] = [
        (1 << 0, 29),
        (1 << 1, 42),
        (1 << 2, 56),
        (1 << 3, 125),
        (1 << 4, 97),
        (1 << 5, 54),
        (1 << 6, 100),
        (1 << 7, 126),
    ];
    for (bit, code) in modifiers.iter() {
        let was = dev.last_mods & bit != 0;
        let now = mods & bit != 0;
        if was != now {
            push_event(EV_KEY, *code, now as i32, dev.evdev_minor);
        }
    }

    // Key edges.
    for k in keys.iter() {
        if *k >= 4 && !dev.last_keys.contains(k) {
            press_key(*k, mods & (MOD_LSHIFT | MOD_RSHIFT) != 0, dev.evdev_minor);
        }
    }
    for k in dev.last_keys.iter() {
        if *k >= 4 && !keys.contains(k) {
            let code = hid_to_linux(*k);
            crate::dprintln!("[kbd-usb] release hid={:#02x} linux_code={} dev={}", *k, code, dev.evdev_minor);
            push_event(EV_KEY, code, 0, dev.evdev_minor);
        }
    }

    dev.last_mods = mods;
    dev.last_keys = keys;
    push_syn(dev.evdev_minor);
}

/// QEMU usb-tablet (absolute HID pointer): [btn][X lo][X hi][Y lo][Y hi][wheel].
/// Converted to relative evdev deltas so consumers see uniform events.
fn decode_tablet(dev: &mut UsbDevice, report: &[u8]) {
    if report.len() < 5 {
        TABLET_REPORT_DROP.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return;
    }
    TABLET_REPORT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    const EV_KEY: u16 = 1;
    const EV_REL: u16 = 2;
    const BTN_LEFT: u16 = 0x110;
    const BTN_RIGHT: u16 = 0x111;
    const BTN_MIDDLE: u16 = 0x112;

    let buttons = report[0] & 0x07;
    let x = ((report[2] as u16) << 8) | report[1] as u16;
    let y = ((report[4] as u16) << 8) | report[3] as u16;
    let wheel = if report.len() > 5 { report[5] as i8 } else { 0 };

    // Button edges only.
    let edges: [(u8, u16); 3] = [(1, BTN_LEFT), (2, BTN_RIGHT), (4, BTN_MIDDLE)];
    for (bit, code) in edges.iter() {
        let was = dev.last_buttons & bit != 0;
        let now = buttons & bit != 0;
        if was != now {
            push_event(EV_KEY, *code, now as i32, dev.evdev_minor);
        }
    }

    const EV_ABS: u16 = 3;
    push_event(EV_ABS, 0 /* ABS_X */, x as i32, dev.evdev_minor);
    push_event(EV_ABS, 1 /* ABS_Y */, y as i32, dev.evdev_minor);

    if dev.last_pos_valid {
        // ponytail: tablet reports absolute 0..32767 HID units (Y down, like screen);
        // scale to screen px via boot fb size, keeping remainders so slow moves aren't lost.
        let (sw, sh) = unsafe {
            crate::graphics::FRAMEBUFFER_INFO
                .map(|f| (f.width as i32, f.height as i32))
                .unwrap_or((1280, 800))
        };
        const HID_MAX: i32 = 32767;
        dev.rem_x += (x as i32 - dev.last_x as i32) * sw;
        dev.rem_y += (y as i32 - dev.last_y as i32) * sh;
        let dx = dev.rem_x.div_euclid(HID_MAX);
        let dy = dev.rem_y.div_euclid(HID_MAX);
        dev.rem_x -= dx * HID_MAX;
        dev.rem_y -= dy * HID_MAX;
        if dx != 0 {
            push_event(EV_REL, 0, dx, dev.evdev_minor);
        }
        if dy != 0 {
            push_event(EV_REL, 1, dy, dev.evdev_minor);
        }
    }
    if wheel != 0 {
        push_event(EV_REL, 8 /* REL_WHEEL */, -(wheel as i32), dev.evdev_minor);
    }
    dev.last_x = x;
    dev.last_y = y;
    dev.last_pos_valid = true;
    dev.last_buttons = buttons;
    push_syn(dev.evdev_minor);
}

fn decode_mouse(dev: &mut UsbDevice, report: &[u8]) {
    if report.is_empty() {
        MOUSE_REPORT_DROP.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return;
    }
    MOUSE_REPORT_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    const EV_KEY: u16 = 1;
    const EV_REL: u16 = 2;
    let buttons = report[0] & 0x07;
    let dx = if report.len() > 1 { report[1] as i8 as i32 } else { 0 };
    let dy = if report.len() > 2 { -(report[2] as i8 as i32) } else { 0 }; // screen Y down

    let edges: [(u8, u16); 3] = [(1, 0x110), (2, 0x111), (4, 0x112)];
    for (bit, code) in edges.iter() {
        let was = dev.last_buttons & bit != 0;
        let now = buttons & bit != 0;
        if was != now {
            push_event(EV_KEY, *code, now as i32, dev.evdev_minor);
        }
    }
    if dx != 0 { push_event(EV_REL, 0, dx, dev.evdev_minor); }
    if dy != 0 { push_event(EV_REL, 1, dy, dev.evdev_minor); }
    dev.last_buttons = buttons;
    push_syn(dev.evdev_minor);
}

/// Translate a HID usage ID to a printable char and inject into stdin +
/// evdev press event.
fn press_key(hid: u8, shift: bool, dev: usize) {
    let linux_code = hid_to_linux(hid);
    crate::dprintln!("[kbd-usb] press hid={:#02x} linux_code={} dev={}", hid, linux_code, dev);
    push_event(1, linux_code, 1, dev);
    // ponytail: console shell reads STDIN_BUFFER, foot reads its own pty —
    // separate consumers, so feeding both is correct, not a double-inject.
    // (PS/2 fed the console the same way before i8042=off.)
    if let Some(c) = hid_to_char(hid, shift) {
        push_char(c);
    }
}

fn hid_to_linux(hid: u8) -> u16 {
    // Letters a-z: HID usages run alphabetically (0x04=a..0x1D=z) but
    // Linux keycodes run by keyboard row (Q=16..P=25, A=30..L=38,
    // Z=44..M=50), so a sequential map reports the wrong key (e.g. T
    // as N). Fixed positional table instead.
    const LETTERS: [u16; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, // a-m
        49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, // n-z
    ];
    if (0x04..=0x1D).contains(&hid) {
        return LETTERS[(hid - 0x04) as usize];
    }
    // Digits 1..9,0: HID 0x1E..=0x27 → KEY_1(2)..KEY_0(11)
    if (0x1E..=0x27).contains(&hid) {
        return 2 + (hid - 0x1E) as u16;
    }
    // Function keys F1..F10: HID 0x3A..=0x43 → KEY_F1(59)..KEY_F10(68)
    if (0x3A..=0x43).contains(&hid) {
        return 59 + (hid - 0x3A) as u16;
    }
    // Function keys F13..F24: HID 0x68..=0x73 → KEY_F13(183)..KEY_F24(194)
    if (0x68..=0x73).contains(&hid) {
        return 183 + (hid - 0x68) as u16;
    }
    // Keypad digits 1..9: HID 0x59..=0x61 → KEY_KP1(79), KEY_KP2(80), KEY_KP3(81),
    // KEY_KP4(75), KEY_KP5(76), KEY_KP6(77), KEY_KP7(71), KEY_KP8(72), KEY_KP9(73)
    const KP_DIGITS: [u16; 9] = [79, 80, 81, 75, 76, 77, 71, 72, 73];
    if (0x59..=0x61).contains(&hid) {
        return KP_DIGITS[(hid - 0x59) as usize];
    }

    match hid {
        0x28 => 28,  // KEY_ENTER
        0x29 => 1,   // KEY_ESC
        0x2A => 14,  // KEY_BACKSPACE
        0x2B => 15,  // KEY_TAB
        0x2C => 57,  // KEY_SPACE
        0x2D => 12,  // KEY_MINUS
        0x2E => 13,  // KEY_EQUAL
        0x2F => 26,  // KEY_LEFTBRACE '['
        0x30 => 27,  // KEY_RIGHTBRACE ']'
        0x31 => 43,  // KEY_BACKSLASH '\'
        0x32 => 86,  // KEY_102ND (Non-US # and ~)
        0x33 => 39,  // KEY_SEMICOLON ';'
        0x34 => 40,  // KEY_APOSTROPHE '\''
        0x35 => 41,  // KEY_GRAVE '`'
        0x36 => 51,  // KEY_COMMA ','
        0x37 => 52,  // KEY_DOT '.'
        0x38 => 53,  // KEY_SLASH '/'
        0x39 => 58,  // KEY_CAPSLOCK

        0x44 => 87,  // KEY_F11
        0x45 => 88,  // KEY_F12

        0x46 => 99,  // KEY_SYSRQ (PrintScreen)
        0x47 => 70,  // KEY_SCROLLLOCK
        0x48 => 119, // KEY_PAUSE
        0x49 => 110, // KEY_INSERT
        0x4A => 102, // KEY_HOME
        0x4B => 104, // KEY_PAGEUP
        0x4C => 111, // KEY_DELETE
        0x4D => 107, // KEY_END
        0x4E => 109, // KEY_PAGEDOWN
        0x4F => 106, // KEY_RIGHT
        0x50 => 105, // KEY_LEFT
        0x51 => 108, // KEY_DOWN
        0x52 => 103, // KEY_UP

        0x53 => 69,  // KEY_NUMLOCK
        0x54 => 98,  // KEY_KPSLASH '/'
        0x55 => 55,  // KEY_KPASTERISK '*'
        0x56 => 74,  // KEY_KPMINUS '-'
        0x57 => 78,  // KEY_KPPLUS '+'
        0x58 => 96,  // KEY_KPENTER
        0x62 => 82,  // KEY_KP0
        0x63 => 83,  // KEY_KPDOT '.'
        0x64 => 86,  // KEY_102ND (Non-US \ and |)
        0x65 => 127, // KEY_COMPOSE (Application / Menu)
        0x66 => 116, // KEY_POWER
        0x67 => 117, // KEY_KPEQUAL '='

        // Modifiers (when reported as individual keys)
        0xE0 => 29,  // KEY_LEFTCTRL
        0xE1 => 42,  // KEY_LEFTSHIFT
        0xE2 => 56,  // KEY_LEFTALT
        0xE3 => 125, // KEY_LEFTMETA
        0xE4 => 97,  // KEY_RIGHTCTRL
        0xE5 => 54,  // KEY_RIGHTSHIFT
        0xE6 => 100, // KEY_RIGHTALT
        0xE7 => 126, // KEY_RIGHTMETA

        _ => {
            crate::dprintln!("[kbd-usb] unmapped hid={:#02x}", hid);
            0
        }
    }
}

fn hid_to_char(hid: u8, shift: bool) -> Option<char> {
    const UNSHIFTED: &[u8] = b"abcdefghijklmnopqrstuvwxyz1234567890";
    const SHIFTED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ!@#$%^&*()";
    let idx = match hid {
        0x04..=0x1D => (hid - 0x04) as usize,
        0x1E..=0x27 => 26 + (hid - 0x1E) as usize,
        _ => return match (hid, shift) {
            (0x2C, _) => Some(' '),
            (0x28, _) => Some('\n'),
            (0x2A, _) => Some('\u{8}'),
            (0x2B, _) => Some('\t'),
            (0x2D, false) => Some('-'),
            (0x2D, true) => Some('_'),
            (0x2E, false) => Some('='),
            (0x2E, true) => Some('+'),
            (0x2F, false) => Some('['),
            (0x2F, true) => Some('{'),
            (0x30, false) => Some(']'),
            (0x30, true) => Some('}'),
            (0x31, false) => Some('\\'),
            (0x31, true) => Some('|'),
            (0x33, false) => Some(';'),
            (0x33, true) => Some(':'),
            (0x34, false) => Some('\''),
            (0x34, true) => Some('"'),
            (0x35, false) => Some('`'),
            (0x35, true) => Some('~'),
            (0x36, false) => Some(','),
            (0x36, true) => Some('<'),
            (0x37, false) => Some('.'),
            (0x37, true) => Some('>'),
            (0x38, false) => Some('/'),
            (0x38, true) => Some('?'),
            (0x54, _) => Some('/'),
            (0x55, _) => Some('*'),
            (0x56, _) => Some('-'),
            (0x57, _) => Some('+'),
            (0x58, _) => Some('\n'),
            (0x59..=0x61, _) => Some((b'1' + (hid - 0x59)) as char),
            (0x62, _) => Some('0'),
            (0x63, _) => Some('.'),
            _ => None,
        },
    };
    let table = if shift { SHIFTED } else { UNSHIFTED };
    table.get(idx).map(|&b| b as char)
}

// ── Timer-tick polling hook & MSI-X handler ─────────────────────────────────

/// MSI-X interrupt handler for xHCI event ring.
pub fn handle_interrupt() {
    if !present() {
        return;
    }
    let mut guard = XHCI.lock();
    if let Some(hc) = guard.as_mut() {
        hc.drain_events();
        // Ack pending interrupt in IMAN
        let ir = hc.rt_base + RT_IR0;
        let iman = hc.rd(ir + IR_IMAN);
        hc.wr32(ir + IR_IMAN, iman | 1); // write 1 to IP clears it
    }
}

/// Drain pending USB reports and service hot-plug events. Called from the
/// LAPIC timer path (~every tick).
pub fn poll() {
    if !present() {
        return;
    }
    let mut guard = XHCI.lock();
    if let Some(hc) = guard.as_mut() {
        hc.drain_events();
        hc.scan_hotplug();
    }
}

impl Xhci {
    /// Timer-tick hook: acknowledge PORTSC change bits and defer everything
    /// else. Runs with IF=0 — no MMIO beyond the ack, no alloc, no locks
    /// beyond XHCI itself (already held by poll), no broadcast.
    fn scan_hotplug(&mut self) {
        for port in 0..self.max_ports {
            let sc = self.portsc(port);
            if sc & PSC_CSC == 0 {
                continue;
            }
            self.portsc_set(port, PSC_CSC | PSC_PEC, 0);
            HOTPLUG_DIRTY.store(true, Ordering::Release);
        }
    }
}

/// Reconcile ports vs devices after a deferred hotplug event. Process context
/// only (called from cpu_idle_loop with no locks held): enumerate + attach new
/// devices, tear down + free removed ones.
pub fn service_hotplug() {
    if !HOTPLUG_DIRTY.swap(false, Ordering::AcqRel) {
        return;
    }
    if !present() {
        return;
    }
    let mut guard = XHCI.lock();
    let hc = match guard.as_mut() {
        Some(h) => h,
        None => return,
    };
    for port in 0..hc.max_ports {
        let connected = hc.portsc(port) & PSC_CCS != 0;
        let slot = hc
            .devices
            .iter()
            .position(|d| d.as_ref().map(|d| d.port == port).unwrap_or(false));
        match (connected, slot) {
            (false, Some(s)) => {
                crate::dprintln!("[XHCI] port {}: disconnected (slot {})", port + 1, s);
                if let Some(d) = hc.devices[s].take() {
                    if d.evdev_minor != usize::MAX {
                        crate::drivers::keyboard::input_free_node(d.evdev_minor);
                        crate::fs::sysfs::sysfs_unregister(13, 64 + d.evdev_minor as u32);
                    }
                }
                let _ = hc.submit_cmd(0, 0, (TRB_DISABLE_SLOT << 10) | (s as u32) << 24);
                unsafe {
                    core::ptr::write_volatile(
                        (hc.dcbaa.as_mut_ptr() as *mut u64).add(s),
                        0,
                    );
                }
            }
            (true, None) => {
                crate::dprintln!("[XHCI] port {}: connect detected", port + 1);
                if enumerate_port(&mut *hc, port) {
                    finish_attach(&mut *hc, port);
                    crate::dprintln!("[XHCI] port {}: HID device online", port + 1);
                }
            }
            _ => {}
        }
    }
}

