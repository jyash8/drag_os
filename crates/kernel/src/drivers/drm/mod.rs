//! DRM/KMS subsystem — kernel-side ioctl handlers.
//!
//! Exposes `/dev/dri/card0` (major 226, minor 0) as a DRM control node.
//! Implements the Linux DRM UAPI ioctls needed by `libdrm` and Mesa's
//! softpipe / llvmpipe renderer for software-rendered Vulkan/OpenGL.
//!
//! Architecture:
//!   GLOBAL_DRM (SpinLock<DrmDevice>)
//!     ├── CRTCs        – logical display controllers
//!     ├── Encoders     – signal converters (TMDS = HDMI/DVI)
//!     ├── Connectors   – physical ports (HDMI-A)
//!     ├── Framebuffers – registered pixel-format buffers
//!     └── DumbBuffers  – CPU-accessible BGRA dumb render targets

pub mod kms;
pub mod syncobj;
pub mod types;
pub mod virtgpu;
pub mod virtgpu_display;
pub mod virtgpu_drv;
pub mod virtgpu_fence;
pub mod virtgpu_gem;
pub mod virtgpu_ioctl;
pub mod virtgpu_object;
pub mod virtgpu_plane;
pub mod virtgpu_submit;
pub mod virtgpu_vq;

use alloc::sync::Arc;
use shared::boot::boot_info::HHDM_BASE;
use shared::memory::address::{PAGE_SIZE, PhysAddr};
use shared::memory::allocator::FrameAllocator;

use crate::arch::sync::spinlock::SpinLock;
use crate::fs::vfs::VNode;
use crate::memory::FRAME_ALLOCATOR;
use crate::tasks::{current_thread, set_current_thread};

use self::types::*;
use drm_uapi::*;

/// Per-submit / per-frame DRM diagnostics.
///
/// Off by default. These used to print unconditionally, which is not
/// affordable on this platform: debugcon is a single port-I/O vmexit *per
/// character*, serialized by a global spinlock, and the prints run from the
/// syscall path with interrupts masked. At two clients' frame rates that is
/// thousands of lines per second (measured: 54k of 67k log lines were four
/// `[VIRTGPU]` lines per submit), which saturates both CPUs, dilates guest
/// time, and starves userspace for seconds at a time — indistinguishable from
/// a compositor freeze.
///
/// Error paths (request rejected, ioctl failed) still print unconditionally;
/// only the success-path tracing is gated. Flip to `true` for bring-up.
pub static DRM_TRACE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[inline]
pub fn trace() -> bool {
    DRM_TRACE.load(core::sync::atomic::Ordering::Relaxed)
}
use crate::drivers::virtio::gpu::VirtioGpuRect;
use self::virtgpu::VIRTGPU_MANAGER;
use crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU;

// ── errno shorthands (match kernel convention: return as u64) ─────────────────
const EINVAL: u64 = (-22i64) as u64;
const EFAULT: u64 = (-14i64) as u64;
const ENOMEM: u64 = (-12i64) as u64;
const ENOENT: u64 = (-2i64) as u64;
const ENOSYS: u64 = (-38i64) as u64;
const ETIMEDOUT: u64 = (-110i64) as u64;
/// Upstream's `framebuffer_check` answers ERANGE when the framebuffer extent
/// is not representable (`drm_framebuffer.c`).
const ERANGE: u64 = (-34i64) as u64;

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Global dirty flag for scanout framebuffers.
pub static SCANOUT_DIRTY: AtomicBool = AtomicBool::new(true);

pub fn mark_scanout_dirty() {
    SCANOUT_DIRTY.store(true, Ordering::Release);
}

/// Pending damage clips (from DIRTYFB), consumed by the next flip.
/// Appended (never replaced): a rendered-but-skipped frame's damage merges
/// into the next flip, which stays correct (superset = same pixels).
/// Missing/empty/overflowing entries and explicit FULL all fall back to a
/// full-frame transfer.
///
/// # Per-framebuffer, and why the single global set was wrong
///
/// This was one global pending set for all framebuffers, on the reasoning that
/// "the compositor's GETCRTC reports the *displayed* fb while its damage belongs
/// to the *next* buffer (swapchain alternation), so per-fb filing always
/// misses."
///
/// That reasoning does not hold against what the client actually sends.
/// Measured on the 2D (`--no-gl`) path with DIRTYFB logging added:
///
/// ```text
/// [dmg] fb=7  clips=1 (0,0 1280x800)      damage filed for fb=7
/// [2dflip] rid=2 fb=11 CLIPS 0..800        consumed by the flip to fb=11  <- wrong buffer
/// [dmg] fb=11 clips=1 (0,770 1280x30)    damage filed for fb=11
/// [2dflip] rid=1 fb=7  CLIPS 770..800      consumed by the flip to fb=7   <- wrong buffer
/// ```
///
/// Every flip consumed the *other* buffer's damage. It happens to look correct
/// when both buffers show identical content, which is why it survived: the
/// misrouting is invisible until damage differs per buffer (a moving window, or
/// a partial redraw). The client names the framebuffer in `drm_mode_fb_dirty_cmd`
/// precisely so it can be honoured.
///
/// So damage is now filed per `fb_id` and consumed by the flip to that same
/// `fb_id`. A flip to a buffer with no pending damage falls back to a full-frame
/// transfer, which is always safe.
pub struct PendingDamage {
    rects: alloc::vec::Vec<VirtioGpuRect>,
    full: bool,
}

/// Pending damage keyed by `fb_id`. A small fixed-size table rather than a map:
/// `FB_DAMAGE` is taken on the DIRTYFB and page-flip paths, and a heap
/// allocation there would be a needless failure mode. Overflow falls back to
/// `overflow_full`, which forces a full transfer.
pub const MAX_PENDING_FBS: usize = 8;

pub struct PendingDamageTable {
    fb_ids: [u32; MAX_PENDING_FBS],
    entries: [Option<PendingDamage>; MAX_PENDING_FBS],
    /// Set when more framebuffers filed damage than the table holds. Forces a
    /// full-frame transfer rather than silently dropping damage.
    overflow_full: bool,
}

impl PendingDamageTable {
    const EMPTY: Self = Self {
        fb_ids: [0; MAX_PENDING_FBS],
        entries: [const { None }; MAX_PENDING_FBS],
        overflow_full: false,
    };
}

pub static FB_DAMAGE: SpinLock<PendingDamageTable> =
    SpinLock::named("FB_DAMAGE", PendingDamageTable::EMPTY);

// ponytail: last tick with a real flip/setcrtc (for the heartbeat gate
// below). Compositors flip on every change; the 5Hz heartbeat exists only
// for silent mmap renderers (doom flips per frame, so it never needs it).
pub static LAST_FLIP_TICK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// DIAGNOSTIC: running count of 3D command submissions, and of the bytes
/// submitted with them.
///
/// Why this exists: the screen alternates between a visible frame and black, and
/// every captured "visible" frame has been **byte-identical** to the one before
/// it — the compositor appears never to produce new content. There are two
/// possible causes with completely different fixes:
///
///   - the compositor is not drawing (compositor-side frame scheduling), or
///   - the compositor IS drawing but the host is not presenting it (kernel/host).
///
/// Counting submits per flip separates them **without needing the screen**, so
/// it works on a busy desktop where an automated capture would be occluded. A
/// flip with `submits == 0` proves the compositor did not draw; a flip with
/// submits > 0 that still shows black moves the fault to presentation.
///
/// Remove once the flicker is explained.
pub static DIAG_SUBMITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static DIAG_SUBMIT_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Record one execbuffer submission. Called from the virtgpu exec path.
pub fn diag_note_submit(bytes: u64) {
    DIAG_SUBMITS.fetch_add(1, Ordering::Relaxed);
    DIAG_SUBMIT_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// True when a 3D resource is the size of the current CRTC mode, i.e. a
/// swapchain candidate rather than a wallpaper/UI texture.
///
/// DIAGNOSTIC support for [`diag_note_target`]. Compares against the CRTC mode
/// where one is set, and otherwise against the common 1280x800, which is the
/// only mode this system drives.
pub fn res_is_scanout_sized(res: &crate::drivers::drm::virtgpu::VirtgpuResource3d) -> bool {
    dims_are_scanout_sized(res.width, res.height)
}

/// The one definition of "matches the scanout size", shared by the resource
/// helper above and by the page-flip path's `diag_note_target` call.
///
/// It was inline in `res_is_scanout_sized` while the flip path had no way to ask
/// the question at all, so a second copy was one copy away -- the same drift
/// already seen between `virtgpu_display::mode_valid` and its inline twin in
/// `handle_set_crtc`.
pub fn dims_are_scanout_sized(w: u32, h: u32) -> bool {
    // Prefer the live CRTC mode; fall back to the mode this system drives when
    // no mode is programmed yet. Mirrors how handle_page_flip derives
    // `mode_size`, so the diagnostic classifies against the same notion of
    // "scanout size" the flip path uses.
    let live = {
        let drm = GLOBAL_DRM.lock();
        drm.as_ref()
            .and_then(|d| d.crtcs.iter().find(|c| c.mode.is_some()))
            .and_then(|c| c.mode.map(|m| (m.width, m.height)))
    };
    let (mw, mh) = live.unwrap_or((1280, 800));
    w == mw && h == mh
}

/// DIAGNOSTIC: ordered trace of each 3D submit and the resources it references.
//
// §10 established that both swapchain buffers are drawn into every frame, but
// it said nothing about *order within* a frame. A fullscreen wallpaper quad drawn
// after the status bar would cover it completely, and every command would still
// report success — indistinguishable from "the status bar was never drawn" when
// looking only at per-flip aggregates. That is the specific question here: is
// the 104x28 status texture drawn, and is anything drawn over it?
//
// Bounded: a fixed budget of submits, then silent. Unlimited logging on the
// submit path would itself perturb timing.
pub static DIAG_TRACE_BUDGET: AtomicU64 = AtomicU64::new(240);
pub static DIAG_EXEC_SEQ: AtomicU64 = AtomicU64::new(0);

/// Record one submit and the resources it references, in order, if budget
/// remains.
pub fn diag_trace_submit(rid_info: &[(u32, u32, u32, bool)], bytes: u64) {
    if DIAG_TRACE_BUDGET.load(Ordering::Relaxed) == 0 {
        return;
    }
    DIAG_TRACE_BUDGET.fetch_sub(1, Ordering::Relaxed);
    let seq = DIAG_EXEC_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut s = alloc::string::String::new();
    for (rid, w, h, scanout_sized) in rid_info {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&alloc::format!(
            "{}{}:{}x{}",
            if *scanout_sized { "" } else { "~" },
            rid,
            w,
            h
        ));
    }
    crate::dprintln!("[exectrace] #{seq} bytes={bytes} targets={s}");
}

/// DIAGNOSTIC: count draw targets per resource id, so a flip can report *which*
/// buffers the compositor actually drew into.
///
/// Two small fixed tables rather than a map: this is on the submit hot path, and
/// a lock there would be a lock-ordering hazard for a diagnostic. Overflow
/// increments a counter that is reported, so saturation is visible rather than
/// silently wrong.
pub const DIAG_TARGET_SLOTS: usize = 8;
pub static DIAG_TARGET_RIDS: [AtomicU64; DIAG_TARGET_SLOTS] = [const { AtomicU64::new(0) }; DIAG_TARGET_SLOTS];
pub static DIAG_TARGET_COUNTS: [AtomicU64; DIAG_TARGET_SLOTS] = [const { AtomicU64::new(0) }; DIAG_TARGET_SLOTS];
pub static DIAG_TARGET_OVERFLOW: AtomicU64 = AtomicU64::new(0);

/// Record one draw-target reference to `rid`.
///
/// `scanout_sized` marks resources whose dimensions match the mode, i.e. the
/// swapchain candidates. Non-scanout-sized resources are the wallpaper and UI
/// textures and are counted separately so they cannot be mistaken for the
/// render target.
pub fn diag_note_target(rid: u32, scanout_sized: bool) {
    for i in 0..DIAG_TARGET_SLOTS {
        let cur = DIAG_TARGET_RIDS[i].load(Ordering::Relaxed);
        if cur == 0 {
            if DIAG_TARGET_RIDS[i].compare_exchange(0, rid as u64 + 1, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                DIAG_TARGET_COUNTS[i].fetch_add(1, Ordering::Relaxed);
                if !scanout_sized {
                    // Mark in the high bit that this slot is not a scanout-sized
                    // resource, so the report can separate them.
                    DIAG_TARGET_COUNTS[i].fetch_or(1u64 << 63, Ordering::Relaxed);
                }
                return;
            }
        } else if cur == rid as u64 + 1 {
            DIAG_TARGET_COUNTS[i].fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    DIAG_TARGET_OVERFLOW.fetch_add(1, Ordering::Relaxed);
}

/// Log how much was submitted since the previous flip, then reset the counter.
pub fn diag_report_flip(rid: u32) {
    static LAST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    static LASTB: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    let n = DIAG_SUBMITS.swap(0, Ordering::Relaxed);
    let b = DIAG_SUBMIT_BYTES.swap(0, Ordering::Relaxed);
    // Always log a flip that had NO work: that is the diagnostic signal, and a
    // sparse filter would hide exactly the interesting case.
    if n > 0 || LAST.swap(n, Ordering::Relaxed) != 0 {
        let mut targets = alloc::string::String::new();
        for i in 0..DIAG_TARGET_SLOTS {
            let raw = DIAG_TARGET_RIDS[i].load(Ordering::Relaxed);
            if raw == 0 {
                continue;
            }
            let c = DIAG_TARGET_COUNTS[i].swap(0, Ordering::Relaxed);
            let nsb = c & (1u64 << 63) != 0;
            DIAG_TARGET_RIDS[i].store(0, Ordering::Relaxed);
            targets.push_str(&alloc::format!(
                " {}{}:{}",
                if nsb { "~" } else { "" },   // ~ = not scanout-sized
                raw - 1,
                c & !(1u64 << 63)
            ));
        }
        crate::dprintln!(
            "[flipdiag] flip_to_rid={} submits={} bytes={} targets={}{}",
            rid,
            n,
            b,
            if targets.is_empty() { "<none>".into() } else { targets.clone() },
            if DIAG_TARGET_OVERFLOW.swap(0, Ordering::Relaxed) > 0 { " (+overflow)" } else { "" }
        );
    }
    LASTB.store(b, Ordering::Relaxed);
}

/// What the host currently has set as scanout on output 0, packed as
/// `(rid << 32) | (width << 16) | height`. Zero means "nothing set yet".
///
/// This exists because SET_SCANOUT must not be re-sent on every flip.
/// Linux sends `cmd_set_scanout` only when the fb or the geometry changes
/// and sends `cmd_resource_flush` on every update
/// (`virtio_gpu_primary_plane_update` in drivers/gpu/drm/virtio/
/// virtgpu_plane.c`); the flush is what names the resource and triggers the
/// blit, and it matches on the *already registered* scanout:
///
/// ```c
/// if (g->parent_obj.scanout[i].resource_id != rf.resource_id)
///         continue;
/// ```
///
/// Re-sending SET_SCANOUT per frame is not merely redundant, it is
/// destructive. `virgl_cmd_set_scanout` calls `qemu_console_resize` every
/// time, and `dpy_gfx_replace_surface` resets `scanout.kind` to
/// `SCANOUT_SURFACE`, replaces `con->surface` and frees the old one -- so
/// the GL scanout texture registered a moment earlier is torn down again
/// before the flush can draw it, and the window stays black. A compositor
/// that keeps flipping the same buffer (the common case, and what mywima
/// does) hit this on every single frame.
pub static HOST_SCANOUT: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// (unused; kept out of the build) 3D resource the scanout was pinned to
/// during the scanout-pin diagnostic.
static PINNED_SCANOUT_RID: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// TEMP-DIAG: distinct 3D resources the flip path has seen, as
/// (resource id, physical backing, pitch). The compositor alternates between
/// buffers, so measuring "whichever was just flipped" cannot distinguish
/// "never rendered into" from "rendered but not sampled yet" -- every buffer
/// has to be read back at the same instant.
static SEEN_3D: SpinLock<alloc::vec::Vec<(u32, u64, u32)>> =
    SpinLock::named("SEEN_3D", alloc::vec::Vec::new());

/// ponytail: diagnostic 3D presentation path. When set, a virgl 3D scanout is
/// read back into guest memory with TRANSFER_FROM_HOST_3D and presented by
/// scanning out a 2D resource attached to those same physical pages.
///
/// Why: the 2D scanout path is known to display correctly on this host (the
/// rainbow probe renders perfectly through it), while the GL scanout path
/// hands the texture to `qemu_console_gl_scanout_texture` and the window
/// stays black. This gives a visible image and proves the render pipeline
/// end to end, at the cost of a full-frame readback per flip.
///
/// This is *not* how Linux presents: `virtio_gpu_primary_plane_update` hands
/// the 3D resource straight to the host. It must go back to false once the
/// GL scanout path is understood.
pub static SCANOUT_READBACK: AtomicBool = AtomicBool::new(false);

/// 3D resource ids that already have a 2D present resource built for them.
///
/// A set rather than a single "last" entry: the compositor double-buffers and
/// alternates between two 3D resources every frame, so a one-entry cache asks
/// the host to create the same resource on every flip and gets
/// `RESP_ERR_INVALID_RESOURCE_ID` (0x1203) back, which
/// `virgl_cmd_create_resource_2d` returns for "resource already exists".
/// A resource's backing pages are fixed for its lifetime and ids are never
/// reused, so remembering the id is enough. 0 marks an unused slot.
static PRESENT_MADE: SpinLock<[u32; 64]> = SpinLock::named("PRESENT_MADE", [0; 64]);

/// Caps: beyond MAX_DIRTY_CLIPS per call or MAX_PENDING_CLIPS stored, or
/// more than MAX_FLIP_CLIPS at flip time, fall back to full frame (each
/// clip costs its own transfer+flush round-trip).
/// Upstream `DRM_MODE_FB_DIRTY_MAX_CLIPS` is 256. We used 64, which silently
/// downgraded any larger (entirely legal) request to a full-frame transfer.
pub const MAX_DIRTY_CLIPS: usize = DRM_MODE_FB_DIRTY_MAX_CLIPS as usize;
pub const MAX_PENDING_CLIPS: usize = 128;
pub const MAX_FLIP_CLIPS: usize = 12;

/// Damage resolved for one flip: full frame, or clip list + covered rows.
enum FlipDamage {
    Full,
    Clips {
        rects: alloc::vec::Vec<VirtioGpuRect>,
        y0: u32,
        y1: u32,
    },
}

/// Take (consume) pending damage for the next flip on this output.
/// Anything but a short clip list resolves to Full.
fn take_flip_damage(fb_w: u32, fb_h: u32) -> FlipDamage {
    take_flip_damage_for(0, fb_w, fb_h)
}

/// Take and clear the pending damage for `fb_id`.
///
/// `fb_id == 0` means "any": used by the paths that have no specific framebuffer
/// (set_crtc), which historically ran against a single global set.
fn take_flip_damage_for(fb_id: u32, fb_w: u32, fb_h: u32) -> FlipDamage {
    let entry = {
        let mut guard = FB_DAMAGE.lock();
        if fb_id == 0 {
            // Drain everything: merge all filed damage into one set. Used only
            // where no framebuffer is implied, so cross-buffer merging cannot
            // misroute anything.
            let mut merged = PendingDamage { rects: alloc::vec::Vec::new(), full: false };
            let mut overflow = guard.overflow_full;
            for i in 0..MAX_PENDING_FBS {
                if let Some(e) = guard.entries[i].take() {
                    if e.full {
                        merged.full = true;
                    }
                    merged.rects.extend(e.rects);
                }
                guard.fb_ids[i] = 0;
            }
            guard.overflow_full = false;
            if overflow {
                merged.full = true;
            }
            merged
        } else {
            let mut found = None;
            let mut overflow = guard.overflow_full;
            for i in 0..MAX_PENDING_FBS {
                if guard.fb_ids[i] == fb_id {
                    found = guard.entries[i].take();
                    guard.fb_ids[i] = 0;
                    break;
                }
            }
            // The overflow flag is global, not per-fb: if the table ever filled
            // we cannot know which buffer the dropped damage belonged to, so
            // every flip must fall back to a full transfer.
            guard.overflow_full = false;
            match found {
                Some(e) => {
                    if overflow {
                        PendingDamage { rects: e.rects, full: true }
                    } else {
                        e
                    }
                }
                None => PendingDamage {
                    rects: alloc::vec::Vec::new(),
                    // No damage filed for this buffer. Fall back to a full
                    // transfer rather than assuming the host already has it.
                    full: true,
                },
            }
        }
    };
    if entry.full || entry.rects.is_empty() || entry.rects.len() > MAX_FLIP_CLIPS {
        return FlipDamage::Full;
    }
    // Clamp defensively (clips were clamped at insert, belt-and-suspenders).
    let mut y0 = fb_h;
    let mut y1 = 0u32;
    let mut kept = alloc::vec::Vec::new();
    for r in entry.rects {
        if r.x >= fb_w || r.y >= fb_h || r.width == 0 || r.height == 0 {
            continue;
        }
        let w = r.width.min(fb_w - r.x);
        let h = r.height.min(fb_h - r.y);
        if w == 0 || h == 0 {
            continue;
        }
        y0 = y0.min(r.y);
        y1 = y1.max(r.y + h);
        kept.push(VirtioGpuRect { x: r.x, y: r.y, width: w, height: h });
    }
    if kept.is_empty() {
        FlipDamage::Full
    } else {
        FlipDamage::Clips { rects: kept, y0: y0.min(fb_h), y1: y1.min(fb_h) }
    }
}

/// Global singleton DRM device.  Populated by `drm_init()` at boot.
pub static GLOBAL_DRM: SpinLock<Option<DrmDevice>> = SpinLock::named("GLOBAL_DRM", None);

// ── DRM event queue (vblank / page-flip completion) ──────────────────────────

/// drm_event_vblank UAPI layout (32 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct DrmEventVblank {
    /// DRM_EVENT_VBLANK (0x01) or DRM_EVENT_FLIP_COMPLETE (0x02)
    pub event_type: u32,
    /// sizeof(DrmEventVblank) = 32
    pub length: u32,
    pub user_data: u64,
    pub tv_sec: u32,
    pub tv_usec: u32,
    pub sequence: u32,
    pub crtc_id: u32,
}

pub const DRM_EVENT_VBLANK: u32 = 0x01;
pub const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

static DRM_EVENTS: SpinLock<alloc::collections::VecDeque<DrmEventVblank>> =
    SpinLock::new(alloc::collections::VecDeque::new());

/// Queue a page-flip completion event for the compositor's read()/poll().
///
/// Also queues a `DRM_EVENT_VBLANK` alongside it, and that is load-bearing.
///
/// Smithay turns a DRM page-flip event into `DrmEvent::VBlank`
/// (`smithay/src/backend/drm/device/mod.rs:557`) and mywima calls
/// `frame_submitted()` **only** on that path
/// (`mywima/src/backends/kms.rs:1562`). `frame_submitted()` is what releases the
/// swapchain slot back to the pool, so if it never runs, `Swapchain::acquire()`
/// (`smithay/src/backend/allocator/swapchain.rs:162`) leaks a slot per frame
/// and eventually hands back buffers drawn long ago — the observed symptom of
/// alternating good/black frames.
///
/// Real DRM delivers vblank separately from flip-complete, so emitting both here
/// matches what a client expects: a vblank per frame, plus the completion for
/// the flip it was waiting on.
pub fn queue_flip_event(crtc_id: u32, user_data: u64) {
    let ns = crate::time::realtime_nanos();
    static SEQ: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);
    let seq = SEQ.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    {
        let mut q = DRM_EVENTS.lock();
        if q.len() < 64 {
            let len = core::mem::size_of::<DrmEventVblank>() as u32;
            let tv_sec = (ns.div_euclid(1_000_000_000) as u64) as u32;
            let tv_usec = ns.rem_euclid(1_000_000_000) as u32 / 1000;
            // VBLANK first: it is what drives the compositor's slot release, and
            // a client draining events in order should make progress on buffer
            // recycling before it handles the completion it is waiting on.
            q.push_back(DrmEventVblank {
                event_type: DRM_EVENT_VBLANK,
                length: len,
                user_data,
                tv_sec,
                tv_usec,
                sequence: seq,
                crtc_id,
            });
            q.push_back(DrmEventVblank {
                event_type: DRM_EVENT_FLIP_COMPLETE,
                length: len,
                user_data,
                tv_sec,
                tv_usec,
                sequence: seq,
                crtc_id,
            });
        }
    } // release DRM_EVENTS lock before waking waiters
    // Wake any thread sleeping in epoll_wait/poll on the DRM fd so the
    // compositor reads the flip-complete event without hitting its timeout.
    crate::tasks::wake_pipe_waiters();
}

/// True when read()/poll(POLLIN) has something to deliver.
pub fn events_pending() -> bool {
    !DRM_EVENTS.lock().is_empty()
}

/// Drain complete events into `buf` (kernel-side). Returns bytes written.
pub fn drain_events(buf: &mut [u8]) -> usize {
    let esz = core::mem::size_of::<DrmEventVblank>();
    let mut q = DRM_EVENTS.lock();
    let n = (buf.len() / esz).min(q.len());
    for i in 0..n {
        let ev = q.pop_front().unwrap();
        unsafe {
            core::ptr::copy_nonoverlapping(
                &ev as *const DrmEventVblank as *const u8,
                buf.as_mut_ptr().add(i * esz),
                esz,
            );
        }
    }
    n * esz
}

// ── Initialisation ────────────────────────────────────────────────────────────

/// Parse the *native* (detailed) descriptor out of an EDID blob.
///
/// EDID block 0 is 128 bytes; the first two 18-byte descriptors are the
/// established and standard timings, and a detailed timing starts with a pixel
/// clock of 0x01 as its first two bytes. We only need the native panel size, so
/// this walks the four detailed descriptors and returns the first one's
/// horizontal/vertical active pixels.
///
/// Returns `(0, 0)` when no detailed descriptor is present, which is a valid
/// EDID (a display may advertise only established timings) and must not be
/// treated as an error -- the caller falls through to the next source.
fn edid_native_resolution(edid: &[u8]) -> (u32, u32) {
    if edid.len() < 128 {
        return (0, 0);
    }
    // Descriptor offsets 54, 72, 90, 108 (18 bytes each).
    for off in [54usize, 72, 90, 108] {
        let d = &edid[off..off + 18];
        // A detailed timing descriptor is marked by pixel clock 0x01 0x01.
        if d[0] != 0x01 || d[1] != 0x01 {
            continue;
        }
        let h_active = u16::from_le_bytes([d[2], d[3]]) as u32;
        let v_active = u16::from_le_bytes([d[5], d[6]]) as u32;
        if h_active != 0 && v_active != 0 {
            return (h_active, v_active);
        }
    }
    (0, 0)
}

/// Initialise the DRM device with one CRTC, one encoder and one connector
/// reflecting the boot GOP framebuffer resolution.  Called from `kernel::init`
/// after `FRAMEBUFFER_INFO` is set.
pub fn drm_init() {
    // Build-time display offset (DRAGOS_SCANOUT_OFF), default 0,0.
    virtgpu_plane::init_scanout_offset();

    let mut dev = DrmDevice::new();

    // Mode discovery, in Linux's order (virtio_gpu_conn_get_modes,
    // virtgpu_display.c:182-211):
    //
    //   count = drm_edid_connector_add_modes(connector);
    //   if (count) return count;                 <-- EDID is the PRIMARY source
    //   ... then GET_DISPLAY_INFO geometry ...   <-- only a fallback
    //   ... then XRES_DEF/YRES_DEF               <-- last resort
    //
    // We had this inverted in effect: EDID was never read, `display_info` was
    // treated as the source of truth (and returns None -- see the
    // display_info diagnostics), so the mode list was anchored to the GOP boot
    // framebuffer plus a hardcoded ladder. That is why `1280x800 (PREFERRED)`
    // appeared in every log regardless of what QEMU was told, and why two
    // resolution experiments came out void.
    //
    // Only the native resolution is taken from EDID here. Turning the full
    // EDID detailed-timings block into a mode list is a larger job; the native
    // descriptor is enough to stop anchoring on GOP, and it is the value Linux
    // also prefers.
    let edid_native: Option<(u32, u32)> = {
        let mut g = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock();
        match g.as_mut() {
            Some(g) => {
                if !g.has_edid {
                    crate::dprintln!("[drm] EDID: device does not advertise VIRTIO_GPU_F_EDID");
                    None
                } else {
                    let mut buf = [0u8; 1024];
                    match g.get_edid(0, &mut buf) {
                        Some(n) if n >= 20 => {
                            crate::dprintln!(
                                "[drm] EDID [12..80] = {:02x?}",
                                &buf[12..80.min(n)]
                            );
                            crate::dprintln!(
                                "[drm] EDID h_addr(18)={} v_addr(19)={} h_screen_cm(14)={} v_screen_cm(15)={}",
                                buf[18], buf[19], buf[14], buf[15]
                            );
                            let (w, h) = edid_native_resolution(&buf[..n]);
                            crate::dprintln!(
                                "[drm] EDID: {} bytes, native descriptor {}x{}",
                                n, w, h
                            );
                            if w != 0 && h != 0 { Some((w, h)) } else { None }
                        }
                        Some(n) => {
                            crate::dprintln!("[drm] EDID: only {} bytes, too short", n);
                            None
                        }
                        None => {
                            crate::dprintln!("[drm] EDID: GET_EDID rejected by host");
                            None
                        }
                    }
                }
            }
            None => None,
        }
    };

    // Fallback: GET_DISPLAY_INFO, then GOP.
    let display_info_size: Option<(u32, u32)> = {
        let mut g = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock();
        match g.as_mut() {
            Some(g) if g.width > 0 && g.height > 0 => Some((g.width, g.height)),
            _ => None,
        }
    };

    // The console geometry QEMU actually blits, taken from the firmware
    // framebuffer. Needed *before* the mode is chosen, because it is the
    // reference the display_info geometry must not shrink below.
    let console_size: (u32, u32) = unsafe {
        crate::graphics::FRAMEBUFFER_INFO
            .map(|fb| (fb.width as u32, fb.height as u32))
            .unwrap_or((1280, 800))
    };

    // Ordering: EDID native, else display_info, else the firmware console.
    //
    // `display_info` is clamped against `console_size` -- NOT against whatever
    // `fb_w/fb_h` this expression goes on to produce. An earlier attempt clamped
    // against `fb_w/fb_h` and therefore compared display_info with itself
    // (640x480 >= 640x480), so the clamp never fired and the mode silently
    // shrank from 1280x800 to 640x480. A guard that references the value it is
    // meant to constrain is not a guard.
    let display_info_size: Option<(u32, u32)> = display_info_size.filter(|&d| {
        if d.0 == 0 || d.1 == 0 || d >= console_size {
            true
        } else {
            crate::dprintln!(
                "[drm] display_info reports {}x{}, smaller than the {}x{} console; ignoring it",
                d.0, d.1, console_size.0, console_size.1
            );
            false
        }
    });

    let (fb_w, fb_h) = edid_native.or(display_info_size).unwrap_or(console_size);

    // Mode-source precedence comes from the translated `virtgpu_display`, which
    // encodes upstream's order (EDID modes win; else display_info geometry;
    // else the firmware default). The previous inline `if/else` produced only a
    // log string, so the *rule* was a side effect of debug output -- and
    // `display_info FAILED -> fall back to GOP` on every boot is exactly this
    // decision landing on the third arm.
    let edid_mode_count = u32::from(edid_native.is_some());
    // Clamp a display_info geometry DOWN is never allowed: this host's EDID is
    // legacy-VESA only, so the one mode it reports enabled is 640x480, while the
    // console QEMU actually blits is 1280x800. Adopting it verbatim would shrink
    // a working display. Upstream would take display_info at face value; we take
    // the larger of the two and say so, rather than trusting an EDID we know is
    // deficient.
    // `display_info_size` is a plain tuple; (0, 0) is how "absent" is spelled.
    // Already clamped against the console geometry above.
    let (di_w, di_h) = display_info_size.unwrap_or((0, 0));
    let mode_source = match crate::drivers::drm::virtgpu_display::mode_source(
        edid_mode_count, di_w, di_h,
    ) {
        crate::drivers::drm::virtgpu_display::ModeSource::Edid => "EDID",
        crate::drivers::drm::virtgpu_display::ModeSource::DisplayInfo { .. } => "DISPLAY_INFO",
        crate::drivers::drm::virtgpu_display::ModeSource::Default => "GOP (fallback)",
    };
    crate::dprintln!("[drm] mode source: {} -> {}x{}", mode_source, fb_w, fb_h);

    // CRTC id=1
    // ponytail: boot with the live display size as current mode (never
    // None) — the flip paths clamp scanout to it; None means "raw FB
    // dims", which shows Mesa's padded rows as a black bar.
    let crtc_id = dev.alloc_id();
    dev.crtcs.push(DrmCrtcState {
        id: crtc_id,
        connector_id: 0,
        encoder_id: 0,
        fb_id: 0,
        mode: Some(DrmModeResolution {
            width: fb_w,
            height: fb_h,
            refresh_hz: 60,
            clock_khz: 0,
            preferred: true,
        }),
        mode_blob_id: 0,
        active: true,
        x: 0,
        y: 0,
    });

    // Encoder id=2  (TMDS = HDMI/DVI digital)
    let encoder_id = dev.alloc_id();
    dev.encoders.push(DrmEncoderState {
        id: encoder_id,
        encoder_type: DRM_MODE_ENCODER_TMDS,
        crtc_id,
        possible_crtcs: 1,
    });

    // Connector id=3  (HDMI-A, connected)
    let connector_id = dev.alloc_id();
    dev.connectors.push(DrmConnectorState {
        id: connector_id,
        crtc_id,
        connected: true,
        connector_type: DRM_CONNECTOR_TYPE_HDMIA,
        encoder_id,
        modes: DrmDevice::add_standard_modes(fb_w, fb_h),
    });

    // Primary plane id=4
    let primary_plane_id = dev.alloc_id();
    dev.planes.push(DrmPlaneState {
        id: primary_plane_id,
        crtc_id,
        fb_id: 0,
        crtc_x: 0,
        crtc_y: 0,
        crtc_w: fb_w,
        crtc_h: fb_h,
        src_x: 0,
        src_y: 0,
        src_w: fb_w << 16,
        src_h: fb_h << 16,
        possible_crtcs: 1 << 0,
        plane_type: DRM_PLANE_TYPE_PRIMARY,
        formats: alloc::vec![
            DRM_FORMAT_XRGB8888,
            DRM_FORMAT_ARGB8888,
            DRM_FORMAT_XBGR8888,
            DRM_FORMAT_ABGR8888,
            DRM_FORMAT_RGB565,
        ],
    });

    // Cursor plane id=5
    let cursor_plane_id = dev.alloc_id();
    dev.planes.push(DrmPlaneState {
        id: cursor_plane_id,
        crtc_id,
        fb_id: 0,
        crtc_x: 0,
        crtc_y: 0,
        crtc_w: 64,
        crtc_h: 64,
        src_x: 0,
        src_y: 0,
        src_w: 64 << 16,
        src_h: 64 << 16,
        possible_crtcs: 1 << 0,
        plane_type: DRM_PLANE_TYPE_CURSOR,
        formats: alloc::vec![DRM_FORMAT_ARGB8888],
    });

    // Back-fill CRTC links now that all IDs are known
    if let Some(crtc) = dev.crtcs.first_mut() {
        crtc.connector_id = connector_id;
        crtc.encoder_id = encoder_id;
    }

    *GLOBAL_DRM.lock() = Some(dev);
    kms::init_kms_backend();
    crate::tprintln!(
        "[DRM] initialised: CRTC={} encoder={} connector={} primary_plane={} ({}x{})",
        crtc_id,
        encoder_id,
        connector_id,
        primary_plane_id,
        fb_w,
        fb_h
    );
}

// ── Memory validation ─────────────────────────────────────────────────────────

#[inline]
fn vr(ptr: u64, size: usize) -> bool {
    crate::memory::space::validate_user_write_range(ptr, size)
}

#[inline]
fn vr_read(ptr: u64, size: usize) -> bool {
    crate::memory::space::validate_user_read_range(ptr, size)
}

// ── Main ioctl dispatcher ─────────────────────────────────────────────────────

pub fn drm_ioctl(cmd: u64, argp: u64) -> u64 {
    let cmd32 = (cmd & 0xFFFF_FFFF) as u64;
    let ret = match cmd32 {
        DRM_IOCTL_VERSION => handle_version(argp),
        DRM_IOCTL_GET_CAP => handle_get_cap(argp),
        DRM_IOCTL_SET_CLIENT_CAP => handle_set_client_cap(argp),
        DRM_IOCTL_SET_MASTER => handle_set_master(argp),
        DRM_IOCTL_DROP_MASTER => handle_drop_master(argp),
        DRM_IOCTL_AUTH_MAGIC => 0, // no-op: we don't authenticate
        DRM_IOCTL_MODE_GETRESOURCES => handle_get_resources(argp),
        DRM_IOCTL_MODE_GETCRTC => handle_get_crtc(argp),
        DRM_IOCTL_MODE_SETCRTC => handle_set_crtc(argp),
        DRM_IOCTL_MODE_GETENCODER => handle_get_encoder(argp),
        DRM_IOCTL_MODE_GETCONNECTOR => handle_get_connector(argp),
        DRM_IOCTL_MODE_GETGAMMA => 0,
        DRM_IOCTL_MODE_SETGAMMA => 0,
        DRM_IOCTL_MODE_OBJ_GETPROPERTIES => handle_obj_get_properties(argp),
        DRM_IOCTL_MODE_GETPROPERTY => handle_get_property(argp),
        DRM_IOCTL_MODE_GETPROPBLOB => handle_get_prop_blob(argp),
        DRM_IOCTL_MODE_CREATEPROPBLOB => handle_create_prop_blob(argp),
        DRM_IOCTL_MODE_DESTROYPROPBLOB => handle_destroy_prop_blob(argp),
        DRM_IOCTL_MODE_GETPLANERESOURCES => handle_get_plane_res(argp),
        DRM_IOCTL_MODE_GETPLANE => handle_get_plane(argp),
        DRM_IOCTL_MODE_SETPLANE => handle_set_plane(argp),
        DRM_IOCTL_MODE_ADDFB => handle_add_fb(argp),
        DRM_IOCTL_MODE_ADDFB2 => handle_add_fb2(argp),
        DRM_IOCTL_MODE_RMFB => handle_rm_fb(argp),
        DRM_IOCTL_MODE_PAGE_FLIP => handle_page_flip(argp),
        DRM_IOCTL_MODE_DIRTYFB => handle_dirtyfb(argp),
        DRM_IOCTL_MODE_CURSOR => handle_mode_cursor(argp),
        DRM_IOCTL_MODE_CREATE_DUMB => handle_create_dumb(argp),
        DRM_IOCTL_MODE_MAP_DUMB => handle_map_dumb(argp),
        DRM_IOCTL_MODE_DESTROY_DUMB => handle_destroy_dumb(argp),
        DRM_IOCTL_MODE_ATOMIC => handle_atomic_commit(argp),
        DRM_IOCTL_GEM_CLOSE => handle_gem_close(argp),
        DRM_IOCTL_PRIME_HANDLE_TO_FD => handle_prime_handle_to_fd(argp),
        DRM_IOCTL_PRIME_FD_TO_HANDLE => handle_prime_fd_to_handle(argp),
        DRM_IOCTL_VIRTGPU_MAP => virtgpu::handle_virtgpu_map(argp),
        DRM_IOCTL_VIRTGPU_EXECBUFFER => virtgpu::handle_virtgpu_execbuffer(argp),
        DRM_IOCTL_VIRTGPU_GETPARAM => virtgpu::handle_virtgpu_getparam(argp),
        DRM_IOCTL_VIRTGPU_RESOURCE_CREATE => virtgpu::handle_virtgpu_resource_create(argp),
        DRM_IOCTL_VIRTGPU_RESOURCE_INFO => virtgpu::handle_virtgpu_resource_info(argp),
        DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST => virtgpu::handle_virtgpu_transfer_from_host(argp),
        DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST => virtgpu::handle_virtgpu_transfer_to_host(argp),
        DRM_IOCTL_VIRTGPU_WAIT => virtgpu::handle_virtgpu_wait(argp),
        DRM_IOCTL_VIRTGPU_GET_CAPS => virtgpu::handle_virtgpu_get_caps(argp),
        DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB => virtgpu::handle_virtgpu_resource_create_blob(argp),
        DRM_IOCTL_VIRTGPU_CONTEXT_INIT => virtgpu::handle_virtgpu_context_init(argp),
        DRM_IOCTL_SYNCOBJ_CREATE => handle_syncoff_create(argp),
        DRM_IOCTL_SYNCOBJ_DESTROY => handle_syncoff_destroy(argp),
        DRM_IOCTL_SYNCOBJ_WAIT => handle_syncoff_wait(argp),
        DRM_IOCTL_SYNCOBJ_SIGNAL => handle_syncoff_signal(argp),
        DRM_IOCTL_SYNCOBJ_RESET => handle_syncoff_reset(argp),
        DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD => handle_syncoff_handle_to_fd(argp),
        DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE => handle_syncoff_fd_to_handle(argp),
        other => {
            // Unimplemented ioctl. Linux returns ENOTTY from drm_ioctl when the
            // request is valid but the driver has no handler, and EINVAL only
            // for a genuinely malformed request. Returning EINVAL here made
            // libdrm report "Invalid argument" for requests that were in fact
            // simply not supported, which sent clients down the wrong
            // recovery path.
            // Log the payload too: the request number alone does not identify
            // the call. But read it through `vr`, which validates the userspace
            // range first -- an unhandled ioctl can carry a NULL or bogus
            // argp, and dereferencing that here faults in supervisor mode and
            // takes the kernel down (13 tests regressed when this read raw).
            let mut p = [0u8; 8];
            let payload = if vr(argp, 8) {
                p.copy_from_slice(unsafe {
                    core::slice::from_raw_parts(argp as *const u8, 8)
                });
                Some(p)
            } else {
                None
            };
            crate::dprintln!(
                "[DRM] UNHANDLED IOCTL cmd={:#x} argp={:#x} payload={payload:?} -> ENOTTY",
                other, argp
            );
            crate::arch::x86_64::syscalls::ENOTTY
        }
    };
    ret
}

fn handle_set_client_cap(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmSetClientCap>()) {
        return EFAULT;
    }
    let cap = unsafe { &*(argp as *const DrmSetClientCap) };
    crate::dprintln!("[DRM] set_client_cap cap={} val={}", cap.capability, cap.value);
    match cap.capability {
        DRM_CLIENT_CAP_STEREO_3D => EINVAL,
        DRM_CLIENT_CAP_UNIVERSAL_PLANES => {
            // Support enabling Universal Planes (value == 1 or 0)
            0
        }
        DRM_CLIENT_CAP_ATOMIC => {
            if cap.value == 1 || cap.value == 0 {
                let mut dev = GLOBAL_DRM.lock();
                if let Some(d) = dev.as_mut() {
                    d.atomic_enabled = cap.value == 1;
                }
                0
            } else {
                EINVAL
            }
        }
        DRM_CLIENT_CAP_ASPECT_RATIO => 0,
        DRM_CLIENT_CAP_WRITEBACK_CONNECTORS => {
            if cap.value == 0 { 0 } else { EINVAL }
        }
        _ => EINVAL,
    }
}

pub const PROP_TYPE: u32 = 1;
pub const PROP_FB_ID: u32 = 2;
pub const PROP_CRTC_ID: u32 = 3;
pub const PROP_CRTC_X: u32 = 4;
pub const PROP_CRTC_Y: u32 = 5;
pub const PROP_CRTC_W: u32 = 6;
pub const PROP_CRTC_H: u32 = 7;
pub const PROP_SRC_X: u32 = 8;
pub const PROP_SRC_Y: u32 = 9;
pub const PROP_SRC_W: u32 = 10;
pub const PROP_SRC_H: u32 = 11;
pub const PROP_ACTIVE: u32 = 12;
pub const PROP_MODE_ID: u32 = 13;
pub const PROP_IN_FENCE_FD: u32 = 14;

fn handle_obj_get_properties(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeObjGetProperties>()) {
        return EFAULT;
    }
    let p = unsafe { &mut *(argp as *mut DrmModeObjGetProperties) };
    let dev = GLOBAL_DRM.lock();
    let d = match dev.as_ref() {
        Some(d) => d,
        None => return ENOSYS,
    };

    crate::dprintln!(
        "[DRM] obj_get_props obj_id={} obj_type={:#x} in_cnt={}",
        p.obj_id,
        p.obj_type,
        p.count_props
    );

    let (prop_ids, prop_vals): (alloc::vec::Vec<u32>, alloc::vec::Vec<u64>) =
        if let Some(plane) = d.planes.iter().find(|pl| pl.id == p.obj_id) {
            (
                alloc::vec![
                    PROP_TYPE,
                    PROP_FB_ID,
                    PROP_CRTC_ID,
                    PROP_CRTC_X,
                    PROP_CRTC_Y,
                    PROP_CRTC_W,
                    PROP_CRTC_H,
                    PROP_SRC_X,
                    PROP_SRC_Y,
                    PROP_SRC_W,
                    PROP_SRC_H,
                    PROP_IN_FENCE_FD,
                ],
                alloc::vec![
                    plane.plane_type as u64,
                    plane.fb_id as u64,
                    plane.crtc_id as u64,
                    plane.crtc_x as i64 as u64,
                    plane.crtc_y as i64 as u64,
                    plane.crtc_w as u64,
                    plane.crtc_h as u64,
                    plane.src_x as u64,
                    plane.src_y as u64,
                    plane.src_w as u64,
                    plane.src_h as u64,
                    u64::MAX, // -1: no in-fence
                ],
            )
        } else if let Some(crtc) = d.crtcs.iter().find(|c| c.id == p.obj_id) {
            (
                alloc::vec![PROP_ACTIVE, PROP_MODE_ID],
                alloc::vec![if crtc.active { 1 } else { 0 }, crtc.mode_blob_id as u64],
            )
        } else if let Some(conn) = d.connectors.iter().find(|c| c.id == p.obj_id) {
            (
                alloc::vec![PROP_CRTC_ID],
                alloc::vec![conn.crtc_id as u64],
            )
        } else {
            p.count_props = 0;
            return 0;
        };

    let total = prop_ids.len() as u32;
    if p.props_ptr != 0 && p.count_props >= total {
        if vr(p.props_ptr, (total as usize) * 4) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    prop_ids.as_ptr(),
                    p.props_ptr as *mut u32,
                    total as usize,
                );
            }
        }
    }
    if p.prop_values_ptr != 0 && p.count_props >= total {
        if vr(p.prop_values_ptr, (total as usize) * 8) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    prop_vals.as_ptr(),
                    p.prop_values_ptr as *mut u64,
                    total as usize,
                );
            }
        }
    }
    p.count_props = total;
    crate::dprintln!("[DRM] obj_get_props obj_id={} returning {} props", p.obj_id, total);
    0
}

fn handle_get_property(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeGetProperty>()) {
        return EFAULT;
    }
    let p = unsafe { &mut *(argp as *mut DrmModeGetProperty) };
    crate::dprintln!("[DRM] get_property prop_id={} in_enum_cnt={} in_val_cnt={}", p.prop_id, p.count_enum_blobs, p.count_values);

    let set_prop_name = |p: &mut DrmModeGetProperty, name_bytes: &[u8]| {
        let mut name = [0u8; 32];
        name[..name_bytes.len()].copy_from_slice(name_bytes);
        p.name = name;
    };

    let mut values_buf = [0u64; 8];
    let mut num_values = 0usize;

    let mut enums_buf = [DrmModePropertyEnum { value: 0, name: [0u8; 32] }; 8];
    let mut num_enums = 0usize;

    match p.prop_id {
        PROP_TYPE => {
            set_prop_name(p, b"type");
            p.flags = DRM_MODE_PROP_ENUM | DRM_MODE_PROP_IMMUTABLE;
            values_buf[0] = 0;
            values_buf[1] = 1;
            values_buf[2] = 2;
            num_values = 3;
            enums_buf[0] = DrmModePropertyEnum { value: 0, name: *b"Overlay\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0" };
            enums_buf[1] = DrmModePropertyEnum { value: 1, name: *b"Primary\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0" };
            enums_buf[2] = DrmModePropertyEnum { value: 2, name: *b"Cursor\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0" };
            num_enums = 3;
        }
        PROP_FB_ID => {
            set_prop_name(p, b"FB_ID");
            p.flags = DRM_MODE_PROP_OBJECT;
            values_buf[0] = DRM_MODE_OBJECT_FB as u64;
            num_values = 1;
        }
        PROP_CRTC_ID => {
            set_prop_name(p, b"CRTC_ID");
            p.flags = DRM_MODE_PROP_OBJECT;
            values_buf[0] = DRM_MODE_OBJECT_CRTC as u64;
            num_values = 1;
        }
        PROP_CRTC_X => {
            set_prop_name(p, b"CRTC_X");
            p.flags = DRM_MODE_PROP_SIGNED_RANGE;
            values_buf[0] = i32::MIN as u32 as u64;
            values_buf[1] = i32::MAX as u32 as u64;
            num_values = 2;
        }
        PROP_CRTC_Y => {
            set_prop_name(p, b"CRTC_Y");
            p.flags = DRM_MODE_PROP_SIGNED_RANGE;
            values_buf[0] = i32::MIN as u32 as u64;
            values_buf[1] = i32::MAX as u32 as u64;
            num_values = 2;
        }
        PROP_CRTC_W => {
            set_prop_name(p, b"CRTC_W");
            p.flags = DRM_MODE_PROP_RANGE;
            values_buf[0] = 0;
            values_buf[1] = u32::MAX as u64;
            num_values = 2;
        }
        PROP_CRTC_H => {
            set_prop_name(p, b"CRTC_H");
            p.flags = DRM_MODE_PROP_RANGE;
            values_buf[0] = 0;
            values_buf[1] = u32::MAX as u64;
            num_values = 2;
        }
        PROP_SRC_X => {
            set_prop_name(p, b"SRC_X");
            p.flags = DRM_MODE_PROP_RANGE;
            values_buf[0] = 0;
            values_buf[1] = (u32::MAX as u64) << 16;
            num_values = 2;
        }
        PROP_SRC_Y => {
            set_prop_name(p, b"SRC_Y");
            p.flags = DRM_MODE_PROP_RANGE;
            values_buf[0] = 0;
            values_buf[1] = (u32::MAX as u64) << 16;
            num_values = 2;
        }
        PROP_SRC_W => {
            set_prop_name(p, b"SRC_W");
            p.flags = DRM_MODE_PROP_RANGE;
            values_buf[0] = 0;
            values_buf[1] = (u32::MAX as u64) << 16;
            num_values = 2;
        }
        PROP_SRC_H => {
            set_prop_name(p, b"SRC_H");
            p.flags = DRM_MODE_PROP_RANGE;
            values_buf[0] = 0;
            values_buf[1] = (u32::MAX as u64) << 16;
            num_values = 2;
        }
        PROP_ACTIVE => {
            set_prop_name(p, b"ACTIVE");
            p.flags = DRM_MODE_PROP_RANGE;
            values_buf[0] = 0;
            values_buf[1] = 1;
            num_values = 2;
        }
        PROP_MODE_ID => {
            set_prop_name(p, b"MODE_ID");
            p.flags = DRM_MODE_PROP_BLOB;
            num_values = 0;
        }
        PROP_IN_FENCE_FD => {
            set_prop_name(p, b"IN_FENCE_FD");
            p.flags = DRM_MODE_PROP_SIGNED_RANGE;
            values_buf[0] = (-1i32) as u32 as u64;
            values_buf[1] = i32::MAX as u32 as u64;
            num_values = 2;
        }
        _ => return EINVAL,
    }

    if num_values > 0 && p.values_ptr != 0 && p.count_values >= num_values as u32 {
        let byte_sz = num_values * core::mem::size_of::<u64>();
        if vr(p.values_ptr, byte_sz) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    values_buf.as_ptr(),
                    p.values_ptr as *mut u64,
                    num_values,
                );
            }
        }
    }
    p.count_values = num_values as u32;

    if num_enums > 0 && p.enum_blob_ptr != 0 && p.count_enum_blobs >= num_enums as u32 {
        let byte_sz = num_enums * core::mem::size_of::<DrmModePropertyEnum>();
        if vr(p.enum_blob_ptr, byte_sz) {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    enums_buf.as_ptr(),
                    p.enum_blob_ptr as *mut DrmModePropertyEnum,
                    num_enums,
                );
            }
        }
    }
    p.count_enum_blobs = num_enums as u32;

    0
}

fn handle_create_prop_blob(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeCreateBlob>()) {
        return EFAULT;
    }
    let p = unsafe { &mut *(argp as *mut DrmModeCreateBlob) };

    // Cap the length, and never let the allocation panic.
    //
    // This handler used `Vec::with_capacity(p.length as usize)` straight from
    // the ioctl. `p.length` is a user u32, so a request for 0x40000000 made the
    // kernel panic:
    //
    //   KERNEL PANIC: memory allocation of 1073741824 bytes failed
    //
    // `with_capacity` *panics* when the allocation fails, so an ordinary
    // out-of-memory condition became a kernel panic. That is the actual defect,
    // and fixing it needs no upstream constant: a kernel must report -ENOMEM,
    // not die, because a user asked for memory.
    //
    // The cap itself is ours, deliberately, and is NOT claimed to be Linux's
    // `BLOB_MAX_SIZE` -- that lives in `drm_property.c` and appears in no public
    // header, so it could not be verified from here. 1 MiB is far more than any
    // mode/EDID-style property blob needs; the point is that the request is
    // bounded and its failure is an errno.
    const PROP_BLOB_MAX: usize = 1 << 20;
    let len = p.length as usize;
    if len > PROP_BLOB_MAX {
        crate::dprintln!(
            "[drm] create_blob: length {} exceeds cap {}",
            len, PROP_BLOB_MAX
        );
        return EINVAL;
    }
    if len > 0 && !vr_read(p.data, len) {
        return EFAULT;
    }
    let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    if data.try_reserve_exact(len).is_err() {
        return ENOMEM;
    }
    if len > 0 && p.data != 0 {
        let src = unsafe { core::slice::from_raw_parts(p.data as *const u8, len) };
        data.extend_from_slice(src);
    }
    let mut dev = GLOBAL_DRM.lock();
    let d = match dev.as_mut() {
        Some(d) => d,
        None => return ENOSYS,
    };
    let blob_id = d.alloc_id();
    d.property_blobs.insert(blob_id, data);
    p.blob_id = blob_id;
    0
}

fn handle_destroy_prop_blob(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeDestroyBlob>()) {
        return EFAULT;
    }
    let p = unsafe { &*(argp as *const DrmModeDestroyBlob) };
    let mut dev = GLOBAL_DRM.lock();
    let d = match dev.as_mut() {
        Some(d) => d,
        None => return ENOSYS,
    };
    if d.property_blobs.remove(&p.blob_id).is_some() {
        0
    } else {
        EINVAL
    }
}

fn handle_get_prop_blob(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeGetPropBlob>()) {
        return EFAULT;
    }
    let p = unsafe { &mut *(argp as *mut DrmModeGetPropBlob) };
    let dev = GLOBAL_DRM.lock();
    let d = match dev.as_ref() {
        Some(d) => d,
        None => return ENOSYS,
    };
    if let Some(blob_data) = d.property_blobs.get(&p.blob_id) {
        let blob_len = blob_data.len() as u32;
        if p.data != 0 && p.length > 0 {
            let copy_len = (p.length as usize).min(blob_data.len());
            if vr(p.data, copy_len) {
                let dst = unsafe { core::slice::from_raw_parts_mut(p.data as *mut u8, copy_len) };
                dst.copy_from_slice(&blob_data[..copy_len]);
            } else {
                return EFAULT;
            }
        }
        p.length = blob_len;
        0
    } else {
        p.length = 0;
        0
    }
}

fn handle_get_plane_res(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeGetPlaneRes>()) {
        crate::dprintln!("[DRM] get_plane_res vr failed for argp={:#x}", argp);
        return EFAULT;
    }
    let dev = GLOBAL_DRM.lock();
    let d = match dev.as_ref() {
        Some(d) => d,
        None => return ENOSYS,
    };
    let p = unsafe { &mut *(argp as *mut DrmModeGetPlaneRes) };
    let count = d.planes.len() as u32;
    crate::dprintln!(
        "[DRM] get_plane_res ptr={:#x} in_cnt={} total_planes={}",
        p.plane_id_ptr,
        p.count_planes,
        count
    );
    if p.plane_id_ptr != 0 {
        let n = if p.count_planes > 0 { p.count_planes.min(count) as usize } else { count as usize };
        if vr(p.plane_id_ptr, n * 4) {
            let dst = unsafe { core::slice::from_raw_parts_mut(p.plane_id_ptr as *mut u32, n) };
            for (i, pl) in d.planes.iter().take(n).enumerate() {
                dst[i] = pl.id;
            }
        } else {
            crate::dprintln!("[DRM] get_plane_res vr failed for plane_id_ptr={:#x} n={}", p.plane_id_ptr, n);
        }
    }
    p.count_planes = count;
    0
}

fn handle_get_plane(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeGetPlane>()) {
        crate::dprintln!("[DRM] get_plane vr failed for argp={:#x}", argp);
        return EFAULT;
    }
    let dev = GLOBAL_DRM.lock();
    let d = match dev.as_ref() {
        Some(d) => d,
        None => return ENOSYS,
    };
    let p = unsafe { &mut *(argp as *mut DrmModeGetPlane) };
    let plane_id = p.plane_id;
    let plane = match d.planes.iter().find(|pl| pl.id == plane_id) {
        Some(pl) => pl,
        None => return ENOENT,
    };
    p.crtc_id = plane.crtc_id;
    p.fb_id = plane.fb_id;
    p.possible_crtcs = plane.possible_crtcs;
    p.gamma_size = 0;
    let count = plane.formats.len() as u32;
    crate::dprintln!(
        "[DRM] get_plane id={} poss_crtcs={:#x} fmt_ptr={:#x} in_fmt_cnt={}",
        plane_id,
        p.possible_crtcs,
        p.format_type_ptr,
        p.count_format_types
    );
    if p.format_type_ptr != 0 {
        let n = if p.count_format_types > 0 { p.count_format_types.min(count) as usize } else { count as usize };
        if vr(p.format_type_ptr, n * 4) {
            let dst = unsafe { core::slice::from_raw_parts_mut(p.format_type_ptr as *mut u32, n) };
            for (i, &fmt) in plane.formats.iter().take(n).enumerate() {
                dst[i] = fmt;
            }
        }
    }
    p.count_format_types = count;
    0
}

fn handle_set_plane(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeSetPlane>()) {
        return EFAULT;
    }
    let mut dev = GLOBAL_DRM.lock();
    let d = match dev.as_mut() {
        Some(d) => d,
        None => return ENOSYS,
    };
    let p = unsafe { &*(argp as *const DrmModeSetPlane) };
    let plane_id = p.plane_id;
    let fb_id = p.fb_id;
    let crtc_id = p.crtc_id;

    let (old_plane, new_plane) = if let Some(pl) = d.planes.iter_mut().find(|pl| pl.id == plane_id) {
        let old_p = crate::drivers::drm::virtgpu_plane::PlaneState {
            fb: if pl.fb_id > 0 { Some(pl.fb_id) } else { None },
            src_w: pl.src_w >> 16,
            src_h: pl.src_h >> 16,
            src_x: pl.src_x >> 16,
            src_y: pl.src_y >> 16,
            crtc_w: pl.crtc_w,
            crtc_h: pl.crtc_h,
            crtc_x: pl.crtc_x as u32,
            crtc_y: pl.crtc_y as u32,
        };
        pl.fb_id = fb_id;
        pl.crtc_id = crtc_id;
        pl.crtc_x = p.crtc_x;
        pl.crtc_y = p.crtc_y;
        pl.crtc_w = p.crtc_w;
        pl.crtc_h = p.crtc_h;
        pl.src_x = p.src_x;
        pl.src_y = p.src_y;
        pl.src_w = p.src_w;
        pl.src_h = p.src_h;

        let new_p = crate::drivers::drm::virtgpu_plane::PlaneState {
            fb: if fb_id > 0 { Some(fb_id) } else { None },
            src_w: p.src_w >> 16,
            src_h: p.src_h >> 16,
            src_x: p.src_x >> 16,
            src_y: p.src_y >> 16,
            crtc_w: p.crtc_w,
            crtc_h: p.crtc_h,
            crtc_x: p.crtc_x as u32,
            crtc_y: p.crtc_y as u32,
        };
        (old_p, new_p)
    } else {
        (Default::default(), Default::default())
    };
    drop(dev);

    if fb_id > 0 {
        let (db_opt, fb_w, fb_h) = {
            let dev = GLOBAL_DRM.lock();
            if let Some(d) = dev.as_ref() {
                if let Some(fb) = d.framebuffers.get(&fb_id) {
                    (d.find_dumb(fb.handle).copied(), fb.width, fb.height)
                } else {
                    (None, 0, 0)
                }
            } else {
                (None, 0, 0)
            }
        };

        if let Some(db) = db_opt {
            let mut output = crate::drivers::drm::virtgpu_plane::OutputState {
                index: 0,
                active: true,
                needs_modeset: false,
            };

            let src_w = if new_plane.src_w > 0 { new_plane.src_w } else { fb_w };
            let src_h = if new_plane.src_h > 0 { new_plane.src_h } else { fb_h };
            let damage = crate::drivers::drm::virtgpu_plane::DamageRect::full_frame(
                src_w.min(u16::MAX as u32) as u16,
                src_h.min(u16::MAX as u32) as u16,
            );

            let bo = if let Some(rid) = db.virtio_rid {
                if db.is_3d {
                    let m = crate::drivers::drm::virtgpu::VIRTGPU_MANAGER.lock();
                    m.resources.get(&db.id).map(crate::drivers::drm::virtgpu_plane::PlaneBo::from_res3d)
                } else {
                    Some(crate::drivers::drm::virtgpu_plane::PlaneBo::from_dumb(&db, rid))
                }
            } else {
                None
            };

            if let Some(mut g) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock().as_mut() {
                let mut plane_st = new_plane;
                if plane_st.src_w == 0 { plane_st.src_w = src_w; }
                if plane_st.src_h == 0 { plane_st.src_h = src_h; }
                crate::drivers::drm::virtgpu_plane::primary_plane_update(
                    &plane_st,
                    &old_plane,
                    &mut output,
                    damage,
                    bo,
                    None,
                    g,
                );
            }

            let (mode_w, mode_h) = {
                let dev = GLOBAL_DRM.lock();
                dev.as_ref()
                    .and_then(|d| d.crtcs.iter().find(|c| c.id == crtc_id))
                    .and_then(|c| c.mode)
                    .map(|m| (m.width, m.height))
                    .unwrap_or((fb_w, fb_h))
            };
            let width = (if p.crtc_w > 0 { p.crtc_w } else { fb_w }).min(mode_w);
            let height = (if p.crtc_h > 0 { p.crtc_h } else { fb_h }).min(mode_h);
            let pitch = if db.pitch > 0 { db.pitch } else { width * 4 };
            if let Some(backend) = kms::KMS_BACKEND.lock().as_ref() {
                backend.set_scanout(db.phys.as_u64(), width, height, pitch);
            }
        }
    }
    0
}

// ── DRM_IOCTL_MODE_ATOMIC ───────────────────────────────────────────────────

fn handle_atomic_commit(argp: u64) -> u64 {
    if !vr_read(argp, core::mem::size_of::<DrmModeAtomic>()) {
        return EFAULT;
    }
    let req = unsafe { *(argp as *const DrmModeAtomic) };

    if trace() {
        crate::dprintln!(
            "[DRM] MODE_ATOMIC: flags={:#x} count_objs={} user_data={:#x}",
            req.flags,
            req.count_objs,
            req.user_data
        );
    }

    let count_objs = req.count_objs as usize;
    if count_objs == 0 {
        return 0;
    }

    if !vr_read(req.objs_ptr, count_objs * core::mem::size_of::<u32>())
        || !vr_read(req.count_props_ptr, count_objs * core::mem::size_of::<u32>())
    {
        return EFAULT;
    }

    let objs = unsafe { core::slice::from_raw_parts(req.objs_ptr as *const u32, count_objs) };
    let count_props = unsafe { core::slice::from_raw_parts(req.count_props_ptr as *const u32, count_objs) };

    let mut total_props: usize = 0;
    for &cnt in count_props {
        total_props = match total_props.checked_add(cnt as usize) {
            Some(t) => t,
            None => return EINVAL,
        };
    }

    if total_props > 0 {
        if !vr_read(req.props_ptr, total_props * core::mem::size_of::<u32>())
            || !vr_read(req.prop_values_ptr, total_props * core::mem::size_of::<u64>())
        {
            crate::dprintln!(
                "[DRM] MODE_ATOMIC: EFAULT on flat prop arrays total_props={}: pr_ptr={:#x} val_ptr={:#x}",
                total_props, req.props_ptr, req.prop_values_ptr
            );
            return EFAULT;
        }
    }

    let all_props = if total_props > 0 {
        unsafe { core::slice::from_raw_parts(req.props_ptr as *const u32, total_props) }
    } else {
        &[]
    };
    let all_values = if total_props > 0 {
        unsafe { core::slice::from_raw_parts(req.prop_values_ptr as *const u64, total_props) }
    } else {
        &[]
    };

    // Intermediate parsed state
    struct AtomicPlaneUpdate {
        plane_id: u32,
        fb_id: Option<u32>,
        crtc_id: Option<u32>,
        crtc_x: Option<i32>,
        crtc_y: Option<i32>,
        crtc_w: Option<u32>,
        crtc_h: Option<u32>,
        src_x: Option<u32>,
        src_y: Option<u32>,
        src_w: Option<u32>,
        src_h: Option<u32>,
    }

    struct AtomicCrtcUpdate {
        crtc_id: u32,
        active: Option<bool>,
        mode_id: Option<u32>,
    }

    struct AtomicConnUpdate {
        conn_id: u32,
        crtc_id: Option<u32>,
    }

    let mut plane_updates: alloc::vec::Vec<AtomicPlaneUpdate> = alloc::vec::Vec::new();
    let mut crtc_updates: alloc::vec::Vec<AtomicCrtcUpdate> = alloc::vec::Vec::new();
    let mut conn_updates: alloc::vec::Vec<AtomicConnUpdate> = alloc::vec::Vec::new();

    let mut prop_offset: usize = 0;
    for i in 0..count_objs {
        let obj_id = objs[i];
        let n_props = count_props[i] as usize;
        let p_ids = &all_props[prop_offset..prop_offset + n_props];
        let p_vals = &all_values[prop_offset..prop_offset + n_props];
        prop_offset += n_props;

        if trace() {
            crate::dprintln!(
                "[DRM] MODE_ATOMIC: obj[{}] id={} n_props={}",
                i, obj_id, n_props
            );
        }

        if n_props > 0 {
            // Determine what object kind this is
            let obj_kind = {
                let dev = GLOBAL_DRM.lock();
                if let Some(d) = dev.as_ref() {
                    if d.planes.iter().any(|pl| pl.id == obj_id) {
                        1 // Plane
                    } else if d.crtcs.iter().any(|c| c.id == obj_id) {
                        2 // CRTC
                    } else if d.connectors.iter().any(|cn| cn.id == obj_id) {
                        3 // Connector
                    } else {
                        0
                    }
                } else {
                    return ENOSYS;
                }
            };

            match obj_kind {
                1 => {
                    let mut pu = AtomicPlaneUpdate {
                        plane_id: obj_id,
                        fb_id: None,
                        crtc_id: None,
                        crtc_x: None,
                        crtc_y: None,
                        crtc_w: None,
                        crtc_h: None,
                        src_x: None,
                        src_y: None,
                        src_w: None,
                        src_h: None,
                    };
                    for j in 0..n_props {
                        match p_ids[j] {
                            PROP_FB_ID => pu.fb_id = Some(p_vals[j] as u32),
                            PROP_CRTC_ID => pu.crtc_id = Some(p_vals[j] as u32),
                            PROP_CRTC_X => pu.crtc_x = Some(p_vals[j] as i32),
                            PROP_CRTC_Y => pu.crtc_y = Some(p_vals[j] as i32),
                            PROP_CRTC_W => pu.crtc_w = Some(p_vals[j] as u32),
                            PROP_CRTC_H => pu.crtc_h = Some(p_vals[j] as u32),
                            PROP_SRC_X => pu.src_x = Some((p_vals[j] >> 16) as u32),
                            PROP_SRC_Y => pu.src_y = Some((p_vals[j] >> 16) as u32),
                            PROP_SRC_W => pu.src_w = Some((p_vals[j] >> 16) as u32),
                            PROP_SRC_H => pu.src_h = Some((p_vals[j] >> 16) as u32),
                            _ => {}
                        }
                    }
                    plane_updates.push(pu);
                }
                2 => {
                    let mut cu = AtomicCrtcUpdate {
                        crtc_id: obj_id,
                        active: None,
                        mode_id: None,
                    };
                    for j in 0..n_props {
                        match p_ids[j] {
                            PROP_ACTIVE => cu.active = Some(p_vals[j] != 0),
                            PROP_MODE_ID => cu.mode_id = Some(p_vals[j] as u32),
                            _ => {}
                        }
                    }
                    crtc_updates.push(cu);
                }
                3 => {
                    let mut co = AtomicConnUpdate {
                        conn_id: obj_id,
                        crtc_id: None,
                    };
                    for j in 0..n_props {
                        if p_ids[j] == PROP_CRTC_ID {
                            co.crtc_id = Some(p_vals[j] as u32);
                        }
                    }
                    conn_updates.push(co);
                }
                _ => {}
            }
        }
    }

    // TEST_ONLY check
    if (req.flags & DRM_MODE_ATOMIC_TEST_ONLY) != 0 {
        if trace() {
            crate::dprintln!("[DRM] MODE_ATOMIC: TEST_ONLY returning success");
        }
        return 0;
    }

    // Commit changes to internal state
    let mut dev = GLOBAL_DRM.lock();
    let d = match dev.as_mut() {
        Some(d) => d,
        None => return ENOSYS,
    };

    for cu in &crtc_updates {
        if let Some(crtc) = d.crtcs.iter_mut().find(|c| c.id == cu.crtc_id) {
            if let Some(active) = cu.active {
                crtc.active = active;
            }
            if let Some(mode_id) = cu.mode_id {
                crtc.mode_blob_id = mode_id;
                if mode_id > 0 {
                    if let Some(blob) = d.property_blobs.get(&mode_id) {
                        if blob.len() >= core::mem::size_of::<DrmModeInfo>() {
                            let mode_info = unsafe { &*(blob.as_ptr() as *const DrmModeInfo) };
                            crtc.mode = Some(DrmModeResolution {
                                width: mode_info.hdisplay as u32,
                                height: mode_info.vdisplay as u32,
                                refresh_hz: mode_info.vrefresh,
                                clock_khz: mode_info.clock,
                                preferred: (mode_info.type_ & DRM_MODE_TYPE_PREFERRED) != 0,
                            });
                        }
                    }
                } else {
                    crtc.mode = None;
                }
            }
        }
    }

    for co in &conn_updates {
        if let Some(conn) = d.connectors.iter_mut().find(|c| c.id == co.conn_id) {
            if let Some(crtc_id) = co.crtc_id {
                conn.crtc_id = crtc_id;
            }
        }
    }

    struct PlaneCommitInfo {
        plane_id: u32,
        plane_type: u32,
        old_state: crate::drivers::drm::virtgpu_plane::PlaneState,
        new_state: crate::drivers::drm::virtgpu_plane::PlaneState,
        db: Option<crate::drivers::drm::types::DumbBuffer>,
        fb_w: u32,
        fb_h: u32,
        crtc_id: u32,
        active: bool,
    }

    let mut commits: alloc::vec::Vec<PlaneCommitInfo> = alloc::vec::Vec::new();

    for pu in &plane_updates {
        if let Some(idx) = d.planes.iter().position(|p| p.id == pu.plane_id) {
            let old_s = {
                let pl = &d.planes[idx];
                crate::drivers::drm::virtgpu_plane::PlaneState {
                    fb: if pl.fb_id > 0 { Some(pl.fb_id) } else { None },
                    src_w: pl.src_w >> 16,
                    src_h: pl.src_h >> 16,
                    src_x: pl.src_x >> 16,
                    src_y: pl.src_y >> 16,
                    crtc_w: pl.crtc_w,
                    crtc_h: pl.crtc_h,
                    crtc_x: pl.crtc_x as u32,
                    crtc_y: pl.crtc_y as u32,
                }
            };

            {
                let pl = &mut d.planes[idx];
                if let Some(fb_id) = pu.fb_id { pl.fb_id = fb_id; }
                if let Some(crtc_id) = pu.crtc_id { pl.crtc_id = crtc_id; }
                if let Some(x) = pu.crtc_x { pl.crtc_x = x; }
                if let Some(y) = pu.crtc_y { pl.crtc_y = y; }
                if let Some(w) = pu.crtc_w { pl.crtc_w = w; }
                if let Some(h) = pu.crtc_h { pl.crtc_h = h; }
                if let Some(x) = pu.src_x { pl.src_x = x << 16; }
                if let Some(y) = pu.src_y { pl.src_y = y << 16; }
                if let Some(w) = pu.src_w { pl.src_w = w << 16; }
                if let Some(h) = pu.src_h { pl.src_h = h << 16; }
            }

            let (new_s, crtc_id, fb_id, pl_id, pl_type) = {
                let pl = &d.planes[idx];
                let new_s = crate::drivers::drm::virtgpu_plane::PlaneState {
                    fb: if pl.fb_id > 0 { Some(pl.fb_id) } else { None },
                    src_w: pl.src_w >> 16,
                    src_h: pl.src_h >> 16,
                    src_x: pl.src_x >> 16,
                    src_y: pl.src_y >> 16,
                    crtc_w: pl.crtc_w,
                    crtc_h: pl.crtc_h,
                    crtc_x: pl.crtc_x as u32,
                    crtc_y: pl.crtc_y as u32,
                };
                (new_s, pl.crtc_id, pl.fb_id, pl.id, pl.plane_type)
            };

            let (db, fb_w, fb_h) = if fb_id > 0 {
                if let Some(fb) = d.framebuffers.get(&fb_id) {
                    (d.find_dumb(fb.handle).copied(), fb.width, fb.height)
                } else {
                    (None, 0, 0)
                }
            } else {
                (None, 0, 0)
            };

            let active = d.crtcs.iter().find(|c| c.id == crtc_id).map(|c| c.active).unwrap_or(true);

            commits.push(PlaneCommitInfo {
                plane_id: pl_id,
                plane_type: pl_type,
                old_state: old_s,
                new_state: new_s,
                db,
                fb_w,
                fb_h,
                crtc_id,
                active,
            });
        }
    }

    drop(dev);

    // Apply hardware updates
    for c in &commits {
        if c.plane_type != DRM_PLANE_TYPE_PRIMARY {
            // Cursor or overlay plane: do not dispatch to primary_plane_update!
            // Passing fb_id=None to primary_plane_update tells QEMU set_scanout(res=0),
            // which disables the console scanout surface and shows "Display output is not active."!
            if trace() {
                crate::dprintln!("[ATOMIC_COMMIT] skip non-primary plane_id={} type={}", c.plane_id, c.plane_type);
            }
            continue;
        }

        let mut output = crate::drivers::drm::virtgpu_plane::OutputState {
            index: 0,
            active: c.active,
            needs_modeset: false,
        };

        let src_w = if c.new_state.src_w > 0 { c.new_state.src_w } else { c.fb_w };
        let src_h = if c.new_state.src_h > 0 { c.new_state.src_h } else { c.fb_h };

        let damage = crate::drivers::drm::virtgpu_plane::DamageRect::full_frame(
            src_w.min(u16::MAX as u32) as u16,
            src_h.min(u16::MAX as u32) as u16,
        );

        let bo = if let Some(ref db) = c.db {
            if let Some(rid) = db.virtio_rid {
                if db.is_3d {
                    let m = crate::drivers::drm::virtgpu::VIRTGPU_MANAGER.lock();
                    m.resources.get(&db.id).map(crate::drivers::drm::virtgpu_plane::PlaneBo::from_res3d)
                } else {
                    Some(crate::drivers::drm::virtgpu_plane::PlaneBo::from_dumb(db, rid))
                }
            } else {
                None
            }
        } else {
            None
        };

        if trace() {
            crate::dprintln!(
                "[ATOMIC_COMMIT] plane_id={} fb_id={:?} db={:?} bo={:?}",
                c.plane_id,
                c.new_state.fb,
                c.db.as_ref().map(|d| (d.id, d.virtio_rid, d.is_3d)),
                bo.as_ref().map(|b| (b.res_id, b.is_3d, b.is_blob))
            );
        }

        if let Some(mut g) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock().as_mut() {
            let mut plane_st = c.new_state;
            if plane_st.src_w == 0 { plane_st.src_w = src_w; }
            if plane_st.src_h == 0 { plane_st.src_h = src_h; }
            let ok = crate::drivers::drm::virtgpu_plane::primary_plane_update(
                &plane_st,
                &c.old_state,
                &mut output,
                damage,
                bo,
                None,
                g,
            );
            if trace() {
                crate::dprintln!("[ATOMIC_COMMIT] primary_plane_update ok={}", ok);
            }
        }

        if let Some(ref db) = c.db {
            if !db.is_3d {
                let (mode_w, mode_h) = {
                    let dev = GLOBAL_DRM.lock();
                    dev.as_ref()
                        .and_then(|d| d.crtcs.iter().find(|crtc| crtc.id == c.crtc_id))
                        .and_then(|crtc| crtc.mode)
                        .map(|m| (m.width, m.height))
                        .unwrap_or((c.fb_w, c.fb_h))
                };
                let width = (if c.new_state.crtc_w > 0 { c.new_state.crtc_w } else { c.fb_w }).min(mode_w);
                let height = (if c.new_state.crtc_h > 0 { c.new_state.crtc_h } else { c.fb_h }).min(mode_h);
                let pitch = if db.pitch > 0 { db.pitch } else { width * 4 };
                if let Some(backend) = kms::KMS_BACKEND.lock().as_ref() {
                    backend.set_scanout(db.phys.as_u64(), width, height, pitch);
                }
            }
        }
    }

    mark_scanout_dirty();
    LAST_FLIP_TICK.store(
        crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64,
        core::sync::atomic::Ordering::Relaxed,
    );

    if (req.flags & DRM_MODE_PAGE_FLIP_EVENT) != 0 {
        let crtc_id = commits.first().map(|c| c.crtc_id).unwrap_or(1);
        queue_flip_event(crtc_id, req.user_data);
    }

    0
}

/// Fill the caller's `drm_version` struct.
/// libdrm sends this first to fingerprint the driver.
fn handle_version(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmVersion>()) {
        return EFAULT;
    }
    let v = unsafe { &mut *(argp as *mut DrmVersion) };

    // ponytail: upstream virtio_gpu driver version is 0.1.0 — Mesa Venus
    // requires major == 0 ("unknown DRM driver" otherwise); GBM/EGL don't
    // gate on it (regressions would show in mesarepro/virglprobe).
    v.version_major = 0;
    v.version_minor = 1;
    v.version_patchlevel = 0;

    // Copy driver name / date / desc into the caller-supplied buffers.
    // The caller sets *_len to buffer capacity; we set it to actual length.
    write_user_str(v.name, &mut v.name_len, b"virtio_gpu");
    write_user_str(v.date, &mut v.date_len, b"20260901");
    write_user_str(v.desc, &mut v.desc_len, b"virtio_gpu");
    0
}

/// Helper: copy `src` bytes into a userspace `*mut i8` buffer whose
/// max capacity is `*len`.  Update `*len` to the actual byte count written.
fn write_user_str(buf: *mut i8, len: &mut usize, src: &[u8]) {
    let cap = *len;
    *len = src.len();
    if buf.is_null() || cap == 0 {
        return; // first call: caller just queries length
    }
    let to_copy = src.len().min(cap);
    if vr(buf as u64, to_copy) {
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), buf as *mut u8, to_copy);
        }
    }
}

// ── DRM_IOCTL_GET_CAP ────────────────────────────────────────────────────────

fn handle_get_cap(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmGetCap>()) {
        return EFAULT;
    }
    let cap = unsafe { &mut *(argp as *mut DrmGetCap) };
    cap.value = match cap.capability {
        DRM_CAP_DUMB_BUFFER => 1, // we support dumb buffers
        DRM_CAP_DUMB_PREFERRED_DEPTH => 32,
        DRM_CAP_DUMB_PREFER_SHADOW => 0, // no shadow needed
        DRM_CAP_VBLANK_HIGH_CRTC => 0,
        DRM_CAP_CRTC_IN_VBLANK_EVENT => 1,
        DRM_CAP_SYNCOBJ => 1, // binary syncobjs (no timelines)
        // ponytail: virgl needs PRIME import+export for buffer sharing;
        // Linux 6.6+ always advertises both. Our synthetic fd registry
        // covers the single-process case.
        DRM_CAP_PRIME => DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT,
        _ => 0,
    };
    0
}

// ── DRM_IOCTL_MODE_GETRESOURCES ───────────────────────────────────────────────

/// Return counts and ID arrays for CRTCs / encoders / connectors / FBs.
/// libdrm calls this twice: once with null pointers to get counts, then
/// again with allocated arrays to get the actual IDs.
fn handle_get_resources(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeCardRes>()) {
        return EFAULT;
    }
    let res = unsafe { &mut *(argp as *mut DrmModeCardRes) };

    let drm = GLOBAL_DRM.lock();
    let dev = match drm.as_ref() {
        Some(d) => d,
        None => return EINVAL,
    };

    // Counts
    let n_crtc = dev.crtcs.len() as u32;
    let n_encoder = dev.encoders.len() as u32;
    let n_connector = dev.connectors.len() as u32;
    let n_fb = dev.framebuffers.len() as u32;

    // Resolution bounds (from GOP)
    let (max_w, max_h) = unsafe {
        crate::graphics::FRAMEBUFFER_INFO
            .map(|fb| (fb.width as u32, fb.height as u32))
            .unwrap_or((4096, 4096))
    };
    res.min_width = 64;
    res.max_width = max_w;
    res.min_height = 64;
    res.max_height = max_h;

    // Fill CRTC id array if the caller provided a buffer large enough
    copy_u32_ids(
        res.crtc_id_ptr,
        res.count_crtcs,
        n_crtc,
        dev.crtcs.iter().map(|c| c.id),
    );
    res.count_crtcs = n_crtc;

    copy_u32_ids(
        res.encoder_id_ptr,
        res.count_encoders,
        n_encoder,
        dev.encoders.iter().map(|e| e.id),
    );
    res.count_encoders = n_encoder;

    copy_u32_ids(
        res.connector_id_ptr,
        res.count_connectors,
        n_connector,
        dev.connectors.iter().map(|c| c.id),
    );
    res.count_connectors = n_connector;

    copy_u32_ids(
        res.fb_id_ptr,
        res.count_fbs,
        n_fb,
        dev.framebuffers.keys().copied(),
    );
    res.count_fbs = n_fb;

    0
}

/// Copy up to `cap` IDs from `iter` into the userspace array at `ptr`.
fn copy_u32_ids(ptr: u64, cap: u32, count: u32, iter: impl Iterator<Item = u32>) {
    if ptr == 0 || cap == 0 {
        return;
    }
    let to_write = (cap as usize).min(count as usize);
    let byte_len = to_write * 4;
    if !vr(ptr, byte_len) {
        return;
    }
    let slice = unsafe { core::slice::from_raw_parts_mut(ptr as *mut u32, to_write) };
    for (dst, id) in slice.iter_mut().zip(iter) {
        *dst = id;
    }
}

// ── DRM_IOCTL_MODE_GETCRTC ───────────────────────────────────────────────────

fn handle_get_crtc(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeCrtc>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmModeCrtc) };

    let drm = GLOBAL_DRM.lock();
    let dev = match drm.as_ref() {
        Some(d) => d,
        None => return EINVAL,
    };

    let crtc = match dev.crtcs.iter().find(|c| c.id == req.crtc_id) {
        Some(c) => c,
        None => return ENOENT,
    };

    req.fb_id = crtc.fb_id;
    req.x = crtc.x;
    req.y = crtc.y;
    req.gamma_size = 256;
    req.mode_valid = crtc.mode.is_some() as u32;
    if let Some(mode) = &crtc.mode {
        req.mode = build_mode_info(mode);
    }
    0
}

// ── DRM_IOCTL_MODE_SETCRTC ───────────────────────────────────────────────────

/// Apply a mode-set: wire the given connector(s), framebuffer and display mode
/// to the CRTC, then push the scanout to the KMS backend (GOP bridge).
fn handle_set_crtc(argp: u64) -> u64 {
    if !vr_read(argp, core::mem::size_of::<DrmModeSetCrtc>()) {
        return EFAULT;
    }
    let req = unsafe { &*(argp as *const DrmModeSetCrtc) };

    let mut drm = GLOBAL_DRM.lock();
    let dev = match drm.as_mut() {
        Some(d) => d,
        None => return EINVAL,
    };

    let crtc = match dev.crtcs.iter_mut().find(|c| c.id == req.crtc_id) {
        Some(c) => c,
        None => return ENOENT,
    };

    // Item 3: CRTC disable blanks the output.
    //
    // virtio_gpu_crtc_atomic_disable (virtgpu_display.c:109-120):
    //
    //   drm_crtc_vblank_off(crtc);
    //   virtio_gpu_cmd_set_scanout(vgdev, output->index, 0, 0, 0, 0, 0);
    //   virtio_gpu_notify(vgdev);
    //
    // Six zeroes: scanout_id, resource_id 0, and the four geometry components --
    // i.e. point the display at NO resource. Same idea as the "nofb" arm of
    // virtio_gpu_primary_plane_update, which also does
    // cmd_set_scanout(vgdev, output->index, 0, ...).
    //
    // Without this, a client that drops its framebuffer (or a modeset with
    // fb_id == 0) leaves the *previous* buffer still scanning out, so the
    // display shows stale content instead of blanking.
    if req.fb_id == 0 {
        let mut g = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock();
        if let Some(g) = g.as_mut() {
            let blanked = g.set_scanout(
                0,
                0,
                crate::drivers::virtio::gpu::VirtioGpuRect { x: 0, y: 0, width: 0, height: 0 },
            );
            crate::dprintln!(
                "[DRM] set_crtc: CRTC disable -> scanout resource 0 ({})",
                if blanked { "sent" } else { "FAILED" }
            );
        }
        crtc.fb_id = 0;
        HOST_SCANOUT.store(0, Ordering::Relaxed);
        return 0;
    }

    crtc.fb_id = req.fb_id;
    crtc.x = req.x;
    crtc.y = req.y;

    if req.mode_valid != 0 {
        let req_w = req.mode.hdisplay as u32;
        let req_h = req.mode.vdisplay as u32;
        // Validate against connector modes
        let mode_valid = dev.connectors.iter().any(|conn| {
            conn.modes.iter().any(|m| m.width == req_w && m.height == req_h)
        });
        if !mode_valid && !dev.connectors.is_empty() {
            crate::dprintln!("[DRM] set_crtc: mode {}x{} not found in connector modes", req_w, req_h);
            return EINVAL;
        }

        // Item 4: virtio_gpu_conn_mode_valid (virtgpu_display.c:213-234).
        //
        //   if (!(mode->type & DRM_MODE_TYPE_PREFERRED))
        //           return MODE_OK;
        //   if (mode->hdisplay == XRES_DEF && mode->vdisplay == YRES_DEF)
        //           return MODE_OK;
        //   if (mode->hdisplay <= width  && mode->hdisplay >= width - 16 &&
        //       mode->vdisplay <= height && mode->vdisplay >= height - 16)
        //           return MODE_OK;
        //   return MODE_BAD;
        //
        // Only a PREFERRED mode is checked, and only against the *native*
        // geometry with a +/-16px tolerance. The check exists solely to stop
        // something displacing the native mode; without it a hardcoded
        // 1280x800 PREFERRED survives on a device that is something else, which
        // is precisely how our fabricated mode list went unnoticed.
        if req.mode.type_ & DRM_MODE_TYPE_PREFERRED != 0 {
            if let Some(native) = dev.connectors.iter().find_map(|c| {
                c.modes.iter().find(|m| m.preferred).map(|m| (m.width, m.height))
            }) {
                const TOL: u32 = 16;
                let w_lo = native.0.saturating_sub(TOL);
                let h_lo = native.1.saturating_sub(TOL);
                let near_native = req_w <= native.0 && req_w >= w_lo
                    && req_h <= native.1 && req_h >= h_lo;
                if !near_native {
                    crate::dprintln!(
                        "[DRM] set_crtc: rejecting PREFERRED {}x{}, native is {}x{} (+/-{}px)",
                        req_w, req_h, native.0, native.1, TOL
                    );
                    return EINVAL;
                }
            }
        }

        crtc.mode = Some(DrmModeResolution {
            width: req_w,
            height: req_h,
            refresh_hz: req.mode.vrefresh,
            clock_khz: req.mode.clock,
            preferred: req.mode.type_ & DRM_MODE_TYPE_PREFERRED != 0,
        });

        // Update FRAMEBUFFER_INFO dimensions if GOP/boot fb is active
        unsafe {
            if let Some(mut fb_info) = crate::graphics::FRAMEBUFFER_INFO {
                fb_info.width = req_w as usize;
                fb_info.height = req_h as usize;
                // If stride is less than new width, adjust stride
                if fb_info.stride < fb_info.width {
                    fb_info.stride = fb_info.width;
                }
                crate::graphics::FRAMEBUFFER_INFO = Some(fb_info);
            }
        }
    } else {
        // ponytail: Linux keeps the current mode on a modeset without new
        // mode info (fb-only flip). Clearing it drops the scanout clamp in
        // the flip paths back to raw FB dims (Mesa pads 800->832) and the
        // black bar returns.
    }
    // ponytail: scanout is the CRTC mode size, never the FB size (see
    // handle_page_flip — Mesa pads GBM BOs and scanning the padded size
    // leaves a black bar under the painted rows). Capture before drop(drm).
    let mode_size = crtc.mode.map(|m| (m.width, m.height));

    // Locate the dumb buffer backing this FB so we can blit it to GOP.
    // (If no FB is set, the display is blanked – we just return OK.)
    if req.fb_id == 0 {
        return 0;
    }
    let fb = match dev.framebuffers.get(&req.fb_id) {
        Some(f) => *f,
        None => return ENOENT,
    };
    let db = match dev.find_dumb(fb.handle) {
        Some(d) => *d,
        None => return ENOENT,
    };
    drop(drm);

    // Proper KMS: if VirtIO-GPU backing exists, program scanout + flush via virtqueue
    let (scan_w, scan_h) = match mode_size {
        Some((w, h)) => (fb.width.min(w), fb.height.min(h)),
        None => (fb.width, fb.height),
    };
    let mut virtio_done = false;
    if let Some(rid) = db.virtio_rid {
        if let Some(mut g) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock().as_mut() {
            let prev_key = HOST_SCANOUT.load(Ordering::Relaxed);
            let old_plane = crate::drivers::drm::virtgpu_plane::PlaneState {
                fb: Some((prev_key >> 32) as u32),
                src_w: ((prev_key >> 16) & 0xFFFF) as u32,
                src_h: (prev_key & 0xFFFF) as u32,
                ..Default::default()
            };
            let new_plane = crate::drivers::drm::virtgpu_plane::PlaneState {
                fb: Some(rid),
                src_w: scan_w,
                src_h: scan_h,
                ..Default::default()
            };
            let mut output = crate::drivers::drm::virtgpu_plane::OutputState {
                index: 0,
                active: true,
                needs_modeset: true,
            };
            let _ = take_flip_damage(scan_w, scan_h);
            let damage = crate::drivers::drm::virtgpu_plane::DamageRect::full_frame(
                scan_w.min(u16::MAX as u32) as u16,
                scan_h.min(u16::MAX as u32) as u16,
            );

            let bo = if db.is_3d {
                let m = crate::drivers::drm::virtgpu::VIRTGPU_MANAGER.lock();
                m.resources.get(&db.id).map(crate::drivers::drm::virtgpu_plane::PlaneBo::from_res3d)
            } else {
                Some(crate::drivers::drm::virtgpu_plane::PlaneBo::from_dumb(&db, rid))
            };

            let ok = crate::drivers::drm::virtgpu_plane::primary_plane_update(
                &new_plane,
                &old_plane,
                &mut output,
                damage,
                bo,
                None,
                g,
            );
            if ok {
                let want = ((rid as u64) << 32)
                    | ((scan_w as u64) << 16)
                    | (scan_h as u64);
                HOST_SCANOUT.store(want, Ordering::Relaxed);
            }
            virtio_done = true;
        }
    }
    // ponytail: GOP shadow blit only with no virtio backing (GOP-only
    // fallback); with a live scanout resource the host never reads GOP.
    // Damage rows narrow the blit the same way clips narrow transfers.
    if !virtio_done {
        match take_flip_damage(scan_w, scan_h) {
            FlipDamage::Clips { y0, y1, .. } => {
                blit_dumb_rows_to_gop(db.phys, scan_w, y0, y1, db.pitch)
            }
            FlipDamage::Full => blit_dumb_to_gop(db.phys, scan_w, scan_h, db.pitch),
        }
    }
    mark_scanout_dirty();
    LAST_FLIP_TICK.store(
        crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64,
        core::sync::atomic::Ordering::Relaxed,
    );
    0
}

/// Copy pixels from a dumb buffer (physical) to the GOP framebuffer.
/// Both are 32 bpp BGRA; stride may differ.
/// Uses row-wise copy_nonoverlapping for performance (avoids scalar pixel loop VMEXITs).
fn blit_dumb_to_gop(src_phys: PhysAddr, width: u32, height: u32, src_pitch: u32) {
    blit_dumb_rows_to_gop(src_phys, width, 0, height, src_pitch);
}

/// Row-range variant for damage-clipped blits (union bbox rows of the
/// transfer clips). Same semantics as the full blit on the covered rows.
fn blit_dumb_rows_to_gop(src_phys: PhysAddr, width: u32, y0: u32, y1: u32, src_pitch: u32) {
    let fb_info = unsafe { crate::graphics::FRAMEBUFFER_INFO };
    let fb = match fb_info {
        Some(f) => f,
        None => return,
    };

    let src_virt = (HHDM_BASE.0 + src_phys.0) as *const u8;
    let dst_virt = (HHDM_BASE.0 + fb.address.0) as *mut u8;

    let blit_w = width.min(fb.width as u32) as usize;
    let dst_pitch = (fb.stride * 4) as usize;
    let src_pitch = src_pitch as usize;
    let row_bytes = blit_w * 4;
    // Clamp the row range into the framebuffer (defensive; callers clamp).
    let y_lo = (y0 as usize).min(fb.height as usize);
    let y_hi = (y1 as usize).min(fb.height as usize).max(y_lo);
    let nrows = y_hi - y_lo;
    if nrows == 0 || row_bytes == 0 {
        return;
    }

    // Fast row-wise copy: avoids per-pixel scalar overhead and debugcon spam.
    if src_pitch == dst_pitch && src_pitch == row_bytes {
        // Pitches match: single contiguous copy at the row offset.
        unsafe {
            core::ptr::copy_nonoverlapping(
                src_virt.add(y_lo * src_pitch),
                dst_virt.add(y_lo * dst_pitch),
                nrows * row_bytes,
            );
        }
    } else {
        for row in y_lo..y_hi {
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src_virt.add(row * src_pitch),
                    dst_virt.add(row * dst_pitch),
                    row_bytes,
                );
            }
        }
    }
}

// ── DRM_IOCTL_MODE_GETENCODER ────────────────────────────────────────────────

fn handle_get_encoder(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeEncoder>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmModeEncoder) };

    let drm = GLOBAL_DRM.lock();
    let dev = match drm.as_ref() {
        Some(d) => d,
        None => return EINVAL,
    };

    let enc = match dev.encoders.iter().find(|e| e.id == req.encoder_id) {
        Some(e) => e,
        None => return ENOENT,
    };

    req.encoder_type = enc.encoder_type;
    req.crtc_id = enc.crtc_id;
    req.possible_crtcs = enc.possible_crtcs;
    req.possible_clones = 0;
    0
}

// ── DRM_IOCTL_MODE_GETCONNECTOR ───────────────────────────────────────────────

/// Return connector state including supported modes.
/// libdrm calls this twice: first with count_modes=0 to get the count,
/// then with a pre-allocated modes_ptr to read the mode list.
fn handle_get_connector(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeGetConnector>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmModeGetConnector) };

    let drm = GLOBAL_DRM.lock();
    let dev = match drm.as_ref() {
        Some(d) => d,
        None => return EINVAL,
    };

    let conn = match dev.connectors.iter().find(|c| c.id == req.connector_id) {
        Some(c) => c,
        None => return ENOENT,
    };

    req.encoder_id = conn.encoder_id;
    req.connector_type = conn.connector_type;
    req.connector_type_id = 1;
    req.connection = if conn.connected {
        DRM_MODE_CONNECTED
    } else {
        DRM_MODE_DISCONNECTED
    };
    req.mm_width = 530; // 530mm × 300mm ≈ 24" 16:9
    req.mm_height = 300;
    req.subpixel = 1; // SubPixelHorizontalRGB
    req.count_props = 0;
    req.count_encoders = 1;

    // Write encoder id array
    copy_u32_ids(
        req.encoders_ptr,
        req.count_encoders,
        1,
        core::iter::once(conn.encoder_id),
    );

    // Write mode list
    let n_modes = conn.modes.len() as u32;
    if req.modes_ptr != 0 && req.count_modes >= n_modes {
        let byte_sz = n_modes as usize * core::mem::size_of::<DrmModeInfo>();
        if vr(req.modes_ptr, byte_sz) {
            let dst = unsafe {
                core::slice::from_raw_parts_mut(req.modes_ptr as *mut DrmModeInfo, n_modes as usize)
            };
            for (i, mode) in conn.modes.iter().enumerate() {
                dst[i] = build_mode_info(mode);
            }
        }
    }
    req.count_modes = n_modes;

    0
}

/// Build a `DrmModeInfo` from our internal resolution descriptor.
fn build_mode_info(mode: &DrmModeResolution) -> DrmModeInfo {
    let w = mode.width as u16;
    let h = mode.height as u16;
    let mut info = DrmModeInfo {
        clock: mode.clock_khz,
        hdisplay: w,
        hsync_start: w + 88,
        hsync_end: w + 88 + 44,
        htotal: w + 88 + 44 + 148,
        hskew: 0,
        vdisplay: h,
        vsync_start: h + 4,
        vsync_end: h + 4 + 5,
        vtotal: h + 4 + 5 + 36,
        vscan: 0,
        vrefresh: mode.refresh_hz,
        flags: DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_PVSYNC,
        type_: if mode.preferred { DRM_MODE_TYPE_PREFERRED } else { 0 },
        name: [0u8; 32],
    };
    // Format mode name as e.g. "1920x1080"
    let name = alloc::format!("{}x{}", mode.width, mode.height);
    let nb = name.as_bytes();
    let n = nb.len().min(31);
    info.name[..n].copy_from_slice(&nb[..n]);
    info
}

// ── DRM_IOCTL_MODE_ADDFB ─────────────────────────────────────────────────────

/// Validate a framebuffer description against the buffer it names.
///
/// Upstream `framebuffer_check()` (drm_framebuffer.c) rejects, before the fb
/// exists:
///
/// ```c
/// if (r->width == 0)  return -EINVAL;
/// if (r->height == 0) return -EINVAL;
/// if (!r->handles[i]) return -EINVAL;
/// if ((uint64_t) height * r->pitches[i] + r->offsets[i] > UINT_MAX) return -ERANGE;
/// if (r->pitches[i] < min_pitch) return -EINVAL;      // min_pitch = width*4
/// ```
///
/// and `virtio_gpu_user_framebuffer_create()` adds a pixel-format check
/// (`virtgpu_display.c`). We checked only "is this a known handle", which let a
/// client register a 65535x65535 framebuffer over a 64x64 dumb buffer. Nothing
/// downstream caught it: with no mode programmed, `scan_w/scan_h` fall back to
/// the fb's own dimensions (`mode_size == None`), and
/// `blit_dumb_rows_to_gop` then reads `(h-1)*src_pitch + w*4` bytes through
/// HHDM from a 16 KiB allocation -- a guest-physical out-of-bounds read whose
/// contents reach both the GOP framebuffer and the virtio scanout.
///
/// The final check is ours rather than upstream's: the framebuffer must fit the
/// buffer it names. Upstream gets this from the GEM object's own size at attach
/// time.
fn validate_fb(handle: u32, width: u32, height: u32, pitch: u32, bpp: u32) -> u64 {
    if width == 0 || height == 0 {
        return EINVAL;
    }
    if handle == 0 {
        return EINVAL;
    }
    // bpp must be a whole number of bytes; 32 is the only depth the scanout
    // paths below assume (they compute byte offsets as x * 4).
    if bpp != 32 {
        return EINVAL;
    }
    // min_pitch = width * (bpp/8), saturating rather than wrapping.
    let min_pitch = width.saturating_mul(4);
    if pitch < min_pitch {
        return EINVAL;
    }
    // height * pitch must be representable, or the byte-offset arithmetic in
    // the blit/transfer paths wraps (drm_framebuffer.c uses ERANGE).
    let bytes = (height as u64).saturating_mul(pitch as u64);
    if bytes > u32::MAX as u64 {
        return ERANGE;
    }
    {
        let drm = GLOBAL_DRM.lock();
        let Some(dev) = drm.as_ref() else {
            return EINVAL;
        };
        let Some(db) = dev.find_dumb(handle) else {
            return ENOENT;
        };
        if bytes > db.size {
            crate::dprintln!(
                "[drm] addfb {}x{} pitch={} needs {} bytes, buffer has {}",
                width, height, pitch, bytes, db.size
            );
            return EINVAL;
        }
    }
    0
}

/// Register a dumb-buffer handle as a displayable framebuffer.
fn handle_add_fb(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeFbCmd>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmModeFbCmd) };

    // Validate before taking GLOBAL_DRM for writing; `validate_fb` takes it
    // itself to resolve the buffer.
    let rc = validate_fb(req.handle, req.width, req.height, req.pitch, req.bpp);
    if rc != 0 {
        return rc;
    }

    let mut drm = GLOBAL_DRM.lock();
    let dev = match drm.as_mut() {
        Some(d) => d,
        None => return EINVAL,
    };

    // The handle must refer to a known dumb buffer
    if !dev.has_dumb(req.handle) {
        return ENOENT;
    }

    let fb_id = dev.alloc_id();
    dev.framebuffers.insert(
        fb_id,
        DrmFramebuffer {
            id: fb_id,
            width: req.width,
            height: req.height,
            pitch: req.pitch,
            bpp: req.bpp,
            depth: req.depth,
            handle: req.handle,
            rect: VirtioGpuRect::default(),
        },
    );
    crate::dprintln!("[drm] addfb fb_id={} {}x{} pitch={} handle={}", fb_id, req.width, req.height, req.pitch, req.handle);
    req.fb_id = fb_id;
    0
}

// ── DRM_IOCTL_MODE_ADDFB2 ────────────────────────────────────────────────────

/// Register a dumb-buffer handle as a displayable framebuffer.
fn handle_add_fb2(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeFbCmd2>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmModeFbCmd2) };

    // Same validation as ADDFB (see `validate_fb`): ADDFB2 has the identical
    // exposure, since `mode_size == None` still makes the flip path trust the
    // framebuffer's own dimensions.
    let rc = validate_fb(req.handles[0], req.width, req.height, req.pitches[0], 32);
    if rc != 0 {
        return rc;
    }

    let mut drm = GLOBAL_DRM.lock();
    let dev = match drm.as_mut() {
        Some(d) => d,
        None => return EINVAL,
    };

    let handle = req.handles[0];
    if !dev.has_dumb(handle) {
        return ENOENT;
    }

    let fb_id = dev.alloc_id();

    // If VirtIO-GPU is the active KMS backend, route resource creation through
    // the VirtIO-GPU command queue instead of inserting directly into the framebuffer map.
    if crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU
        .lock()
        .as_ref()
        .is_some()
    {
        // TODO: Route through VQ - CMD_RESOURCE_CREATE_2D + CMD_RESOURCE_ATTACH_BACKING
        // For now, fall through to direct framebuffer registration
    }

    dev.framebuffers.insert(
        fb_id,
        DrmFramebuffer {
            id: fb_id,
            width: req.width,
            height: req.height,
            pitch: req.pitches[0],
            bpp: 32,
            depth: 24,
            handle,
            rect: VirtioGpuRect::default(),
        },
    );
    req.fb_id = fb_id;
    0
}

// ── DRM_IOCTL_MODE_RMFB ──────────────────────────────────────────────────────

fn handle_rm_fb(argp: u64) -> u64 {
    if !vr_read(argp, 4) {
        return EFAULT;
    }
    let fb_id = unsafe { *(argp as *const u32) };

    let mut drm = GLOBAL_DRM.lock();
    let dev = match drm.as_mut() {
        Some(d) => d,
        None => return EINVAL,
    };
    dev.framebuffers.remove(&fb_id);
    0
}

// ── DRM_IOCTL_MODE_PAGE_FLIP ─────────────────────────────────────────────────

/// Asynchronous page flip: swap the CRTC's active framebuffer.
/// For DragOS we do it synchronously by blitting the new FB to the KMS backend.
fn handle_page_flip(argp: u64) -> u64 {
    if !vr_read(argp, core::mem::size_of::<DrmModePageFlip>()) {
        return EFAULT;
    }
    let req = unsafe { &*(argp as *const DrmModePageFlip) };

    let mut drm = GLOBAL_DRM.lock();
    let dev = match drm.as_mut() { Some(d) => d, None => return EINVAL };

    // Update CRTC fb_id
    let crtc = match dev.crtcs.iter_mut().find(|c| c.id == req.crtc_id) {
        Some(c) => c,
        None => return ENOENT,
    };
    // Capture the *previous* fb before overwriting. Linux compares the new
    // framebuffer against the old one to decide whether damage clips can be
    // trusted at all (see the `fb_changed` use in the 2D transfer below).
    let prev_fb_id = crtc.fb_id;
    crtc.fb_id = req.fb_id;
    let fb_changed = prev_fb_id != req.fb_id;
    // ponytail: scanout is the CRTC mode size, never the FB size — Mesa
    // pads GBM BOs (800->832 rows) and scanning the padded size leaves a
    // black bar under the painted 800 rows. Linux crops via plane CRTC_W/H.
    let mode_size = crtc.mode.map(|m| (m.width, m.height));

    // Resolve FB → dumb buffer
    let fb = match dev.framebuffers.get(&req.fb_id) {
        Some(f) => *f,
        None => return ENOENT,
    };
    let db = match dev.find_dumb(fb.handle) {
        Some(d) => *d,
        None => return ENOENT,
    };
    drop(drm);
    let (scan_w, scan_h) = match mode_size {
        Some((w, h)) => (fb.width.min(w), fb.height.min(h)),
        None => (fb.width, fb.height),
    };
    // One line per flip naming the buffer kind and the path taken. The 2D, blob
    // and 3D resources have genuinely different presentation requirements, and
    // "black screen / sheared frame" is impossible to diagnose without knowing
    // which one a given flip resolved to.
    {
        static N: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        // The tick is included so a host-side screen sample can be paired with
        // the rid that was scanned out at that moment: the black-frame symptom
        // is per-buffer, so correlating sample class against rid is what
        // identifies which buffer is at fault (see FPS-FLICKER-HANDOFF.md).
        if n < 8 || n % 30 == 0 {
            let kind = match (db.virtio_rid, db.blob, db.is_3d) {
                (None, _, _) => "gop-blit",
                (Some(_), true, _) => "blob",
                (Some(_), false, true) => "3d-virtio",
                (Some(_), false, false) => "2d-virtio",
            };
            crate::dprintln!(
                "[flip] #{} tick={} fb={} handle={} {} {}x{} pitch={} rid={:?}",
                n,
                crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed),
                fb.id, db.id, kind, db.width, db.height, db.pitch, db.virtio_rid
            );
        }
    }
    // ponytail: flip applies synchronously; completion reported via event
    // queue (real vsync-deferred flip when a vblank IRQ path exists).
    // TEMP-DIAG: pull the rendered 3D texture into guest memory, then look at
    // it. The host is rendering correctly (we see sheared frames), but we
    // cannot see the host texture because it lives in host memory and the
    // guest backing is never written. TRANSFER_FROM_HOST_3D is the only
    // virtio-gpu command that copies a host 3D resource into guest memory, and
    // The TEMP-DIAG readback that used to live here (TRANSFER_FROM_HOST_3D
    // of the live scanout resource plus an ASCII dump of it) has been
    // removed on purpose. It was not a passive probe: issuing a full-frame
    // readback against the resource currently being scanned out perturbs
    // the host's rendering, and a buffer that read back full then read back
    // empty purely as a result of the probe. It cannot be used to decide
    // whether a buffer has content.
    //
    // The forced 1x1 readback in the 3D flip path below is kept: it is 4
    // bytes, it is on the ordering path rather than the diagnostic path, and
    // it is what makes the host finish the render before the scanout is
    // programmed.

    let ok = if let Some(rid) = db.virtio_rid {
        if let Some(mut g) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock().as_mut() {
            let rect = VirtioGpuRect { x: 0, y: 0, width: scan_w, height: scan_h };
            if db.blob {
                // ponytail: blob resources must ONLY get SET_SCANOUT_BLOB; the legacy
                // 2D SET_SCANOUT on a blob rid confuses the host scanout state.
                g.set_scanout_blob(0, rid, rect, scan_w, scan_h,
                    crate::drivers::virtio::gpu::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM, db.pitch);
                // No transfer/flush for blobs: the host reads the backing
                // directly. The blob set_scanout_blob has no result yet, so a
                // blob flip cannot be reported as failed here.
                true
            } else if db.is_3d {
                // 3D + scanout: a virgl 3D resource already holds the rendered
                // pixels, so no TRANSFER_TO_HOST_2D is needed -- there is no
                // guest memory for the host to copy from.
                //
                // But SET_SCANOUT alone puts *nothing* on screen. In QEMU's
                // built-in virgl path (hw/display/virtio-gpu-virgl.c):
                //
                //   virgl_cmd_set_scanout     -> qemu_console_gl_scanout_texture
                //       only *registers* info.tex_id as the scanout texture.
                //   virgl_cmd_resource_flush  -> virtio_gpu_rect_update
                //       -> qemu_console_gl_update -- the only caller that
                //          actually blits that texture to the display.
                //
                // and the flush is skipped unless it names the current scanout:
                //
                //   if (g->parent_obj.scanout[i].resource_id != rf.resource_id)
                //           continue;
                //
                // so the flush must carry the rid just set as scanout. Linux
                // does exactly this: virtio_gpu_primary_plane_update sends
                // cmd_set_scanout when the fb changes and then calls
                // virtio_gpu_resource_flush unconditionally on every update.
                // Omitting it left the window black with no error reported
                // anywhere, because both commands returned OK -- the host was
                // simply never asked to draw.
                //
                // Damage clips are consumed so the pending set does not grow
                // unbounded across a run that never has anything to transfer.
                let _ = take_flip_damage(scan_w, scan_h);

                // TEMP-DIAG: log the *actual* geometry sent to the host. The
                // `[flip] #N` line above prints db.width/db.height (the dumb
                // buffer), which is NOT what goes on the wire -- the rect is
                // built from scan_w/scan_h = min(fb, mode). Those have never
                // been logged, and the reported corruption is a hard 16px
                // (64-byte) horizontal tile repeat with the top ~30 rows
                // intact, which is a geometry symptom, not a colour one.
                {
                    static GEOM: core::sync::atomic::AtomicU32 =
                        core::sync::atomic::AtomicU32::new(0);
                    let n = GEOM.fetch_add(1, Ordering::Relaxed);
                    if n < 4 || n % 40 == 0 {
                        crate::dprintln!(
                            "[geom3d] rid={} fb={}x{} mode={:?} -> scan={}x{} RECT x={} y={} w={} h={} | BO phys={:#x} pages={} size={:#x} pitch={}",
                            rid,
                            fb.width,
                            fb.height,
                            mode_size,
                            scan_w,
                            scan_h,
                            rect.x,
                            rect.y,
                            rect.width,
                            rect.height,
                            db.phys.0,
                            db.pages,
                            db.size,
                            db.pitch
                        );
                    }
                }

                // Always send SET_SCANOUT before RESOURCE_FLUSH on every
                // 3D flip — no caching.
                //
                // The old HOST_SCANOUT cache was designed for single-buffered
                // compositors where rid never changes between flips, so the
                // cache always hit and SET_SCANOUT was sent only once (at
                // SETCRTC). With double-buffering the rid alternates every
                // frame (e.g. 102 ↔ 111), the cache always misses, and
                // SET_SCANOUT is sent on every flip anyway — but the
                // conditional path left one of the two buffers permanently
                // black (measured: Buffer A visible, Buffer B 100% black,
                // strict 1 Hz alternation matching the clock tick).
                //
                // Linux unconditionally sends SET_SCANOUT + RESOURCE_FLUSH
                // on every plane update (virtio_gpu_primary_plane_update in
                // drivers/gpu/drm/virtio/virtgpu_plane.c). We match that.
                //
                // The old comment warned that SET_SCANOUT per frame was
                // "destructive" because qemu_console_resize resets the
                // display surface. In practice, qemu_console_resize with
                // unchanged dimensions is benign on modern QEMU, and the
                // evidence proves it: rid=102 displays correctly even after
                // a round-trip through SET_SCANOUT on every even flip (#2,
                // #4, #6). The real issue was that the conditional cache
                // path somehow prevented rid=111 from establishing a stable
                // GL scanout on its first appearance.
                //
                // No forced GPU→CPU synchronisation: TRANSFER_FROM_HOST_3D
                // was tried and made the screen go black by changing the
                // host's tex_id. The fix for tearing is a dedicated fence
                // virtqueue — a real piece of work, not a stopgap.
                //
                // EXPERIMENT (docs/virgl-vs-linux.md §5.1): reproduce Linux's
                // conditional set_scanout gate.
                //
                // Linux's virtio_gpu_primary_plane_update sends cmd_set_scanout
                // only when the framebuffer changed or a modeset is pending:
                //
                //   if (plane->state->fb != old_state->fb || ... ||
                //       output->needs_modeset) { ... cmd_set_scanout ... }
                //   virtio_gpu_resource_flush(plane, rect...);
                //
                // i.e. Linux tracks the currently-scanned-out resource and skips
                // the redundant resend. We had no such state: HOST_SCANOUT was
                // written at two sites and never read (0 loads in the tree), so
                // the "which resource is currently scanout" fact did not exist
                // here at all.
                //
                // That is the fact, restored. The previous comment claimed the
                // cache "always missed" with double buffering and so was
                // equivalent to unconditional -- self-contradictory, and the leak
                // theory behind it was already disproven (the scanout set was
                // always just {102,111}). Testing real Linux behaviour is the
                // only way to settle it.
                // Linux, virtgpu_primary_plane_update (virtgpu_plane.c):
                //
                //   if (plane->state->fb != old_state->fb ||
                //       plane->state->src_w != old_state->src_w ||
                //       plane->state->src_h != old_state->src_h ||
                //       plane->state->src_x != old_state->src_x ||
                //       plane->state->src_y != old_state->src_y ||
                //       output->needs_modeset) {
                //           output->needs_modeset = false;
                //           virtio_gpu_cmd_set_scanout(vgdev, output->index,
                //               bo->hw_res_handle, src_w >> 16, src_h >> 16, 0, 0);
                //   }
                //   virtio_gpu_resource_flush(plane, rect...);
                //
                // Every buffer is set as scanout DIRECTLY. There is no pinned
                // scanout resource and no resource-to-resource copy.
                //
                // The pinned-scanout design was ours alone: render into buffer B,
                // blit it host-side into a permanently-pinned buffer A with
                // RESOURCE_COPY_REGION, and present A. Linux has ZERO
                // references to copy_region / COPY_REGION / TRANSFER3D across
                // all of virtgpu_{drv,display,plane,submit,vq,object,ioctl,gem}.
                //
                // That host-side blit is the prime suspect for the wallpaper
                // shear. It is a virglrenderer blit between two resources whose
                // layouts need not agree -- the destination is a scanout
                // resource whose VIRGL_BIND_SCANOUT we strip at creation
                // (drm/virtgpu.rs), and the source receives its pixels through a
                // different mechanism entirely (Mesa's staging -> texture
                // COPY_TRANSFER3D, stride 4096000 vs 5120). A layout mismatch
                // there yields exactly what is observed: uniform, whole-screen,
                // present from frame one, fixed small horizontal period.
                //
                // It also explains why every previous flip-path fix failed: the
                // damage is not done by the flip path, it is done *before* it.
                //
                // `scanout_changed` is now exactly Linux's (C) gate. With
                // double buffering the fb changes every frame, so this is true
                // every frame -- which is correct and is what Linux does.
                // Routed through the line-by-line translation of
                // virtio_gpu_primary_plane_update (virtgpu_plane.rs), which is
                // now the live implementation rather than a specification.
                //
                // Behaviour is deliberately unchanged from the inline version it
                // replaces: Linux's (C) gate keys on fb / src_* / needs_modeset,
                // and because the rid alternates every frame with double
                // buffering the gate is true every frame -- which is what the
                // old HOST_SCANOUT cache also did in practice. The one
                // difference is that the gate now *reads* `old_state` rather
                // than inferring it from a packed key, so the intent matches
                // upstream instead of coinciding with it.
                static FLUSH_FENCE_ID: core::sync::atomic::AtomicU64 =
                    core::sync::atomic::AtomicU64::new(0xF100_0000);
                let flush_fence = FLUSH_FENCE_ID.fetch_add(1, Ordering::Relaxed);

                // `old_state` for the gate: the previously-scanned-out resource
                // and geometry, from the same packed HOST_SCANOUT key the old
                // code compared against.
                let prev_key = HOST_SCANOUT.load(Ordering::Relaxed);
                let old_plane = crate::drivers::drm::virtgpu_plane::PlaneState {
                    fb: Some((prev_key >> 32) as u32),
                    src_w: ((prev_key >> 16) & 0xFFFF) as u32,
                    src_h: (prev_key & 0xFFFF) as u32,
                    ..Default::default()
                };
                let new_plane = crate::drivers::drm::virtgpu_plane::PlaneState {
                    fb: Some(rid),
                    src_w: scan_w,
                    src_h: scan_h,
                    ..Default::default()
                };
                let mut output = crate::drivers::drm::virtgpu_plane::OutputState {
                    index: 0,
                    active: true,
                    needs_modeset: false,
                };

                // Damage: the 3D path has no per-clip upload, so the whole
                // scanned-out area is what gets flushed. Resolving this through
                // take_flip_damage keeps the pending set from growing without
                // bound, and Full is the correct DamageRect for it.
                let _ = take_flip_damage(scan_w, scan_h);
                let damage = crate::drivers::drm::virtgpu_plane::DamageRect::full_frame(
                    scan_w.min(u16::MAX as u32) as u16,
                    scan_h.min(u16::MAX as u32) as u16,
                );

                let bo = {
                    let m = crate::drivers::drm::virtgpu::VIRTGPU_MANAGER.lock();
                    m.resources.get(&db.id).map(crate::drivers::drm::virtgpu_plane::PlaneBo::from_res3d)
                };

                let ok = crate::drivers::drm::virtgpu_plane::primary_plane_update(
                    &new_plane,
                    &old_plane,
                    &mut output,
                    damage,
                    bo,
                    Some(flush_fence),
                    // `g` is the GLOBAL_VIRTIO_GPU guard the caller already
                    // holds; the module borrows it rather than re-acquiring,
                    // which would self-deadlock on a non-reentrant SpinLock.
                    g,
                );

                // Keep HOST_SCANOUT coherent so the next flip's `old_state` is
                // accurate -- it is now the *input* to Linux's gate.
                if ok {
                    let want = ((rid as u64) << 32)
                        | ((scan_w as u64) << 16)
                        | (scan_h as u64);
                    HOST_SCANOUT.store(want, Ordering::Relaxed);
                    crate::dprintln!(
                        "[flip] rid={} presented (set_scanout + fenced flush)",
                        rid
                    );

                    // TEMP-DIAG: ordered readback of the resource we just put on
                    // screen. Two frames only (#3, #30) -- the previous
                    // full-frame probe was removed because it perturbed the host
                    // and blacked the screen, so this is deliberately minimal.
                    //
                    // Reads four whole ROWS, one command each, and reports
                    // (a) the first bytes of the row and (b) whether the row
                    // repeats with a 16-pixel stride. The reported artifact is
                    // a hard ~16px / 64-byte horizontal column repeat, so
                    // `row[i] == row[i+16]` across a long span is the signature.
                    // A row of zeroes means the host has nothing at all, which
                    // is a different answer again.
                    {
                        static PROBE: core::sync::atomic::AtomicU32 =
                            core::sync::atomic::AtomicU32::new(0);
                        let pn = PROBE.fetch_add(1, Ordering::Relaxed);
                        if pn == 3 || pn == 30 {
                            for (pi, py) in [8u32, 120u32, 360u32, 640u32].iter().enumerate() {
                                if let Some((virt, _len)) =
                                    g.probe_rows(rid, 0, *py, scan_w, 1)
                                {
                                    let base = virt as *const u8;
                                    let px = |i: usize| unsafe {
                                        let p = base.add(i * 4);
                                        (core::ptr::read_volatile(p) as u32)
                                            | ((core::ptr::read_volatile(p.add(1)) as u32) << 8)
                                            | ((core::ptr::read_volatile(p.add(2)) as u32) << 16)
                                            | ((core::ptr::read_volatile(p.add(3)) as u32) << 24)
                                    };
                                    let first: [u32; 8] = core::array::from_fn(|k| px(k));
                                    // Count positions where the row repeats at a
                                    // 16-pixel offset, over the first 256 pixels.
                                    let mut rep16 = 0u32;
                                    let mut nonzero = 0u32;
                                    for i in 0..256usize {
                                        if px(i) != 0 {
                                            nonzero += 1;
                                        }
                                        if i + 16 < 256 && px(i) == px(i + 16) {
                                            rep16 += 1;
                                        }
                                    }
                                    crate::dprintln!(
                                        "[probe] f#{} rid={} row{}(y={}) first8={:08x?} nonzero={}/256 rep16={}/240",
                                        pn, rid, pi, py, first, nonzero, rep16
                                    );
                                } else {
                                    // Not a failure: `probe_rows` now always
                                    // declines. TRANSFER_FROM_HOST_3D writes into
                                    // the resource's own attached iov, so there
                                    // is no way to aim it at a scratch buffer
                                    // without detaching the scanout's backing.
                                    // Say so once, instead of logging a
                                    // "READBACK FAILED" that reads like a
                                    // driver bug.
                                    if pi == 0 {
                                        crate::dprintln!(
                                            "[probe] unavailable: host texture readback has no guest-visible \
                                             destination (see gpu.rs probe_rows); prior rep16 numbers were \
                                             uninitialised DMA, not evidence"
                                        );
                                    }
                                }
                            }
                        }
                    }
                } else {
                    crate::dprintln!("[flip] rid={} FAILED in primary_plane_update", rid);
                }
                ok
            } else {
                // ponytail: damage-clipped transfer (DIRTYFB); full fallback.
                // (Offset: see page_flip — host reads src at backing+offset.)
                //
                // Propagate the host's verdict instead of assuming success. A
                // rejected SET_SCANOUT (e.g. a rid the host does not know)
                // previously still returned 0 and queued a FLIP_COMPLETE, so
                // userspace believed a frame was on screen that never was.
                if !g.set_scanout(0, rid, rect) {
                    crate::dprintln!("[2dflip] rid={} set_scanout FAILED", rid);
                    false
                } else {
                    // Linux's rule (virtgpu_plane.c:113-121, virtio_gpu_plane_atomic_check):
                    //
                    //   if (old_plane_state->fb != new_plane_state->fb)
                    //           new_plane_state->ignore_damage_clips = true;
                    //
                    // with the comment "Ignore damage clips if the framebuffer
                    // attached to the plane's state has changed since the last
                    // plane update (page-flip). In this case, a full plane update
                    // should happen because uploads are done per-buffer."
                    //
                    // That last clause is the whole point. Damage clips describe
                    // what changed *relative to the buffer's own previous
                    // contents*. The host's copy of a buffer is only ever as
                    // current as the transfers we sent, so when the compositor
                    // flips to a buffer whose host-side copy we cannot prove is
                    // current, the clips do not describe the difference between
                    // guest memory and the host -- and transferring only them
                    // leaves the rest of the host copy stale.
                    //
                    // Measured consequence of ignoring this: the compositor
                    // correctly reports only the clock's damage --
                    // DIRTYFB fb=11 clips=1 (0,770 1280x30) -- and we faithfully
                    // transferred 30 of 800 rows every frame, so the rest of the
                    // host's buffer was never refreshed. Forcing a full
                    // transfer made the display correct, which is what identified
                    // this.
                    //
                    // With double buffering the fb changes on *every* flip, so
                    // this means a full update per flip -- which is exactly what
                    // Linux does, and why it has the rule at all.
                    let dmg = if fb_changed {
                        // Still consume, so the filed clips do not accumulate.
                        let _ = take_flip_damage_for(fb.id, scan_w, scan_h);
                        crate::dprintln!(
                            "[2dflip] rid={} fb={} fb_changed -> FULL (Linux ignore_damage_clips)",
                            rid, fb.id
                        );
                        FlipDamage::Full
                    } else {
                        take_flip_damage_for(fb.id, scan_w, scan_h)
                    };
                    match &dmg {
                        FlipDamage::Clips { rects, y0, y1 } => {
                            crate::dprintln!(
                                "[2dflip] rid={} fb={} {}x{} pitch={} CLIPS n={} yspan={}..{}",
                                rid, fb.id, db.width, db.height, db.pitch,
                                rects.len(), y0, y1
                            );
                        }
                        FlipDamage::Full => {
                            crate::dprintln!(
                                "[2dflip] rid={} fb={} {}x{} pitch={} FULL",
                                rid, fb.id, db.width, db.height, db.pitch
                            );
                        }
                    }
                    let (xfer_ok, flush_ok) = match dmg {
                        FlipDamage::Clips { rects, .. } => {
                            let mut xok = true;
                            let mut fok = true;
                            for r in rects.iter() {
                                let off = r.x as u64 * 4 + r.y as u64 * db.pitch as u64;
                                let x = g.transfer_to_host_2d(rid, *r, off);
                                if !x {
                                    xok = false;
                                    crate::dprintln!(
                                        "[2dflip] rid={} TRANSFER FAILED rect={}x{}+{}+{} off={:#x}",
                                        rid, r.width, r.height, r.x, r.y, off
                                    );
                                    // Deliberately still attempt the flush so the
                                    // host is not left believing the old content
                                    // is current.
                                    fok &= g.flush_resource(rid, *r);
                                } else {
                                    fok &= g.flush_resource(rid, *r);
                                }
                            }
                            (xok, fok)
                        }
                        FlipDamage::Full => {
                            // TEMP-DIAG: log the geometry actually put on the
                            // wire, and check the offset arithmetic against the
                            // real backing layout. The 3D path's geometry was
                            // verified correct (geom3d: fb/mode/scan/RECT all
                            // 1280x800, pitch 5120), so the remaining suspect for
                            // a wrong-looking 2D transfer is the offset, not the
                            // rect. `offset` in TRANSFER_TO_HOST_2D is BOTH the
                            // destination offset and the source offset within
                            // the attached backing, so it must be computed from
                            // the same pitch the backing was mapped with.
                            {
                                use core::sync::atomic::AtomicU32;
                                static G: AtomicU32 = AtomicU32::new(0);
                                let n = G.fetch_add(1, Ordering::Relaxed);
                                if n < 4 || n % 60 == 0 {
                                    let expect_last_row = (db.height as u64 - 1)
                                        * db.pitch as u64;
                                    crate::dprintln!(
                                        "[xfer-geom] rid={} rect={}x{}+{}+{} off=0 pitch={} fb={}x{} bpp={} size={:#x} phys={:#x} pages={} | last_row_end={:#x} vs size={:#x} {}",
                                        rid, rect.width, rect.height, rect.x, rect.y,
                                        db.pitch, db.width, db.height, db.bpp,
                                        db.size, db.phys.0, db.pages,
                                        expect_last_row + db.pitch as u64,
                                        db.size,
                                        if expect_last_row + db.pitch as u64 == db.size { "OK" } else { "MISMATCH" }
                                    );
                                }
                            }
                            let x = g.transfer_to_host_2d(rid, rect, 0);
                            if !x {
                                crate::dprintln!("[2dflip] rid={} TRANSFER FAILED (full)", rid);
                            }
                            let f = g.flush_resource(rid, rect);
                            (x, f)
                        }
                    };
                    if !xfer_ok || !flush_ok {
                        crate::dprintln!(
                            "[2dflip] rid={} host verdict transfer={} flush={} -- frame NOT presented",
                            rid, xfer_ok, flush_ok
                        );
                    }
                    xfer_ok && flush_ok
                }
            }
            // ponytail: no GOP shadow blit — the host scans the virtio
            // resource; the 4MB memcpy only runs on the GOP-only path below.
        } else {
            false
        }
    } else {
        // ponytail: GOP fallback (no virtio rid) blits the damage rows when
        // clips are pending, else the full frame as before.
        match take_flip_damage(scan_w, scan_h) {
            FlipDamage::Clips { y0, y1, .. } => {
                blit_dumb_rows_to_gop(db.phys, scan_w, y0, y1, db.pitch)
            }
            FlipDamage::Full => blit_dumb_to_gop(db.phys, scan_w, scan_h, db.pitch),
        }
        true
    };
    if ok {
        mark_scanout_dirty();
        // DIAGNOSTIC: record which resource this flip presents, so
        // diag_report_flip can print `targets=` instead of permanently
        // `<none>`. That field existed and was read on every flip, but nothing
        // ever populated the table -- a diagnostic that looked instrumented and
        // collected nothing.
        //
        // Only for a virtio-backed flip: on the GOP-only path there is no rid,
        // and noting rid 0 would record a resource that does not exist.
        if let Some(rid) = db.virtio_rid {
            diag_note_target(rid, dims_are_scanout_sized(db.width, db.height));
        }
        // DIAGNOSTIC: report how many 3D submits landed since the previous flip,
        // so a black frame can be attributed to either the compositor not
        // drawing or the host not presenting (see DIAG_SUBMITS).
        diag_report_flip(db.virtio_rid.unwrap_or(0));
        LAST_FLIP_TICK.store(
            crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64,
            core::sync::atomic::Ordering::Relaxed,
        );
        queue_flip_event(req.crtc_id, req.user_data);
        0
    } else {
        EINVAL
    }
}

// ── DRM_IOCTL_MODE_DIRTYFB ───────────────────────────────────────────────────

/// Damage clips from userspace: appended to the fb's pending set, consumed
/// by the next flip (which transfers only the clipped rects). num_clips==0
/// with no flags is a no-op; FULL flag, insane counts, or overflow mark the
/// fb for a full-frame transfer. Never nests locks (GLOBAL_DRM dropped first).
fn handle_dirtyfb(argp: u64) -> u64 {
    if !vr_read(argp, core::mem::size_of::<DrmModeFbDirtyCmd>()) {
        return EFAULT;
    }
    let req = unsafe { &*(argp as *const DrmModeFbDirtyCmd) };
    // Validate the fb exists; snapshot dims for clamping (drop the lock first).
    let (fb_w, fb_h) = {
        let drm = GLOBAL_DRM.lock();
        match drm.as_ref().and_then(|d| d.framebuffers.get(&req.fb_id)) {
            Some(f) => (f.width, f.height),
            None => return ENOENT,
        }
    };
    // Upstream: a dirty command with no clips and no ANNOTATE_COPY is a no-op.
    //
    // This used to read `req.flags & DRM_FB_DIRTY_FULL`, but bit 0 of
    // `drm_mode_fb_dirty_cmd.flags` is DRM_MODE_FB_DIRTY_ANNOTATE_COPY upstream
    // -- "the client already damaged these regions" -- NOT "transfer
    // everything". Testing it as "full" discarded a real client's clip list.
    // Our own full-update decision is expressed by `num_clips == 0` alone.
    let flags = req.flags & DRM_MODE_FB_DIRTY_FLAGS;
    let annotate_copy = flags & DRM_FB_DIRTY_ANNOTATE_COPY != 0;
    if req.num_clips == 0 && !annotate_copy {
        return 0;
    }
    // When ANNOTATE_COPY is set, clips must come in pairs (src/dst rectangles)
    if annotate_copy && (req.num_clips % 2 != 0) {
        crate::dprintln!("[dmg] DIRTYFB fb={} REJECTED: annotate_copy set with odd nclips={}", req.fb_id, req.num_clips);
        return EINVAL;
    }
    // fb_id only validates the target exists; clips attach to the global
    // pending set (see FB_DAMAGE: per-fb filing always misses under
    // swapchain alternation). Never nest locks (GLOBAL_DRM dropped above).
    let mut table = FB_DAMAGE.lock();
    // Find or claim a slot for this fb_id.
    let slot = {
        let mut s = None;
        for i in 0..MAX_PENDING_FBS {
            if table.fb_ids[i] == req.fb_id {
                s = Some(i);
                break;
            }
        }
        if s.is_none() {
            for i in 0..MAX_PENDING_FBS {
                if table.fb_ids[i] == 0 {
                    table.fb_ids[i] = req.fb_id;
                    s = Some(i);
                    break;
                }
            }
        }
        s
    };
    let Some(slot) = slot else {
        // More distinct framebuffers filed damage than the table holds. Force a
        // full transfer on the next flip rather than dropping damage on the
        // floor, which would leave stale pixels with nothing to correct them.
        table.overflow_full = true;
        drop(table);
        crate::dprintln!("[dmg] DIRTYFB fb={} table full -> forcing full transfer", req.fb_id);
        mark_scanout_dirty();
        return 0;
    };
    let pending = table.entries[slot].get_or_insert_with(|| PendingDamage {
        rects: alloc::vec::Vec::new(),
        full: false,
    });
    // Full update when we have no usable clip list. `num_clips == 0` is the
    // only such signal now; bit 0 means ANNOTATE_COPY and must not be read as
    // "full". The clip cap is upstream's 256, not our old 64 -- a legal 65..=256
    // clip request was being silently downgraded to a full-frame transfer here.
    if req.num_clips == 0 || req.num_clips as usize > MAX_DIRTY_CLIPS {
        pending.full = true;
        pending.rects.clear();
        // DIAGNOSTIC: the client said "damage unknown, transfer everything".
        // Logged because on the 2D path a permanent stream of FULL would mean
        // the compositor's clip computation is not reaching us at all, which
        // looks identical on screen to "damage works but is wrong".
        crate::dprintln!(
            "[dmg] DIRTYFB fb={} FULL (flags={:#x} nclips={}) -> discard pending clips",
            req.fb_id, req.flags, req.num_clips
        );
        drop(table);
        // DIRTYFB is the only damage notification a compositor that skips
        // PAGE_FLIP (e.g. while it has no GPU fence) sends. Without marking
        // the scanout dirty, periodic_flush's `is_dirty` gate stays false and
        // this damage is never transferred at all.
        mark_scanout_dirty();
        return 0;
    }
    let n = req.num_clips as usize;
    if !vr_read(req.clips_ptr, n * core::mem::size_of::<DrmClipRect>()) {
        // DIAGNOSTIC: a client whose clips live in unmapped memory fails here
        // and would otherwise look like "damage never arrives".
        crate::dprintln!(
            "[dmg] DIRTYFB fb={} clips_ptr={:#x} n={} UNREADABLE -> EFAULT",
            req.fb_id, req.clips_ptr, n
        );
        return EFAULT;
    }
    for i in 0..n {
        let c = unsafe { &*(req.clips_ptr as *const DrmClipRect).add(i) };
        let x1 = (c.x1 as u32).min(fb_w);
        let y1 = (c.y1 as u32).min(fb_h);
        let x2 = (c.x2 as u32).min(fb_w);
        let y2 = (c.y2 as u32).min(fb_h);
        if x2 > x1 && y2 > y1 {
            pending.rects.push(VirtioGpuRect {
                x: x1,
                y: y1,
                width: x2 - x1,
                height: y2 - y1,
            });
        }
        if pending.rects.len() > MAX_PENDING_CLIPS {
            pending.full = true;
            pending.rects.clear();
            break;
        }
    }
    drop(table);
    // DIAGNOSTIC: the clips the client actually submitted, before clamping
    // dropped any. Needed to tell "the client sent a small clip" from "the
    // client sent a large clip and we clamped it away" -- indistinguishable in
    // the [2dflip] output, which only shows what survived.
    {
        let p = FB_DAMAGE.lock();
        let mut s = alloc::string::String::new();
        for r in p.entries[slot].iter().flat_map(|e| e.rects.iter()) {
            s.push_str(&alloc::format!(" ({},{} {}x{})", r.x, r.y, r.width, r.height));
        }
        crate::dprintln!(
            "[dmg] DIRTYFB fb={} clips={}{} pending_total={} full={}",
            req.fb_id, n, s,
            p.entries[slot].as_ref().map(|e| e.rects.len()).unwrap_or(0),
            p.entries[slot].as_ref().map(|e| e.full).unwrap_or(false)
        );
    }
    mark_scanout_dirty();
    0
}

// ── DRM_IOCTL_MODE_CURSOR ─────────────────────────────────────────────────────

/// Hardware cursor plane via the VirtIO-GPU cursor virtqueue.
/// MOVE repositions a bound cursor; BO binds (or with handle=0, hides) a
/// dumb buffer as the cursor image. Falls back to EINVAL when no GPU
/// cursor queue exists (GOP-only display).
fn handle_mode_cursor(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeCursor>()) {
        return EFAULT;
    }
    let req = unsafe { &*(argp as *const DrmModeCursor) };

    // Resolve dumb buffer (BO) before touching the driver lock.
    let db = if req.flags & DRM_MODE_CURSOR_BO != 0 && req.handle != 0 {
        let drm = GLOBAL_DRM.lock();
        match drm.as_ref().and_then(|d| d.find_dumb(req.handle)) {
            Some(d) => Some(*d),
            None => return ENOENT,
        }
    } else {
        None
    };

    let mut gpu_guard = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock();
    let mut g = match gpu_guard.as_mut() {
        Some(g) => g,
        None => return ENOSYS,
    };
    let x = req.x.max(0) as u32;
    let y = req.y.max(0) as u32;

    if req.flags & DRM_MODE_CURSOR_BO != 0 {
        // Bind/hide. Cursor images are 64×64 per the VirtIO-GPU spec.
        let rid = match db {
            Some(d) => {
                // Cursor images are 64×64 max per the VirtIO-GPU spec.
                if req.width == 0 || req.height == 0 || req.width > 64 || req.height > 64 {
                    return EINVAL;
                }
                match d.virtio_rid {
                    Some(rid) => {
                        if !d.blob {
                            let rect = VirtioGpuRect { x: 0, y: 0, width: d.width, height: d.height };
                            g.transfer_to_host_2d(rid, rect, 0);
                            g.flush_resource(rid, rect);
                        }
                        rid
                    }
                    None => return ENOSYS,
                }
            }
            _ => 0, // handle=0 → hide
        };
        if g.update_cursor(rid, 0, x, y) { 0 } else { EINVAL }
    } else if req.flags & DRM_MODE_CURSOR_MOVE != 0 {
        if g.move_cursor(0, x, y) { 0 } else { EINVAL }
    } else {
        EINVAL
    }
}

pub(crate) fn handle_set_master(_argp: u64) -> u64 {
    let pid = crate::tasks::get_current_pid();
    let mut dev = GLOBAL_DRM.lock();
    if let Some(ref mut device) = *dev {
        let prev_vt = device.prev_vt_mode;
        let prev_kb = device.prev_kb_mode;
        if device.master_pid.is_none() {
            device.master_pid = Some(pid);
            device.prev_vt_mode = prev_vt;
            device.prev_kb_mode = prev_kb;
        }
    }
    drop(dev);
    0
}

pub(crate) fn handle_drop_master(_argp: u64) -> u64 {
    let pid = crate::tasks::get_current_pid();
    // Release GLOBAL_DRM before touching the KMS backend: `set_scanout`
    // re-acquires it to resolve the framebuffer, and SpinLock is
    // non-reentrant, so holding both would be a same-CPU reentry panic. It
    // would also invert the AGENTS.md order (device lock outside backend).
    let restore_scanout = {
        let mut dev = GLOBAL_DRM.lock();
        match dev.as_mut() {
            Some(device) if device.master_pid == Some(pid) => {
                device.master_pid = None;
                // The DRM master going away is the other way a graphical client
                // hands the console back, so drop the binding it took at
                // VT_ACTIVATE. Without this, a compositor that exits leaves
                // GRAPHICAL_VT set and the next Ctrl+Alt+F<n> "reclaims" a
                // console nobody holds. release_graphical takes only atomics,
                // so calling it under GLOBAL_DRM adds no lock edge.
                let active = crate::graphics::vt::active_vt_number();
                crate::graphics::vt::release_graphical(active);
                crate::arch::x86_64::syscalls::fsio::CONSOLE_KD_MODE.store(0, core::sync::atomic::Ordering::SeqCst);
                crate::arch::x86_64::syscalls::fsio::CONSOLE_KB_MODE.store(2, core::sync::atomic::Ordering::SeqCst);
                crate::graphics::terminal::WRITER.lock().clear();
                // SAFETY: set once at boot, read-only afterwards.
                unsafe { crate::graphics::FRAMEBUFFER_INFO }.map(|fb| fb.address.0)
            }
            _ => None,
        }
    }; // GLOBAL_DRM released here
    if let Some(fb_phys) = restore_scanout {
        if let Some(backend) = crate::drivers::drm::kms::KMS_BACKEND.lock().as_ref() {
            backend.page_flip(fb_phys);
        }
    }
    0
}

// ── DRM_IOCTL_MODE_CREATE_DUMB ───────────────────────────────────────────────
/// Allocate a CPU-accessible dumb render target.
/// The caller supplies width × height × bpp; we allocate physical pages,
/// assign a handle, and return pitch + size.
fn handle_create_dumb(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeCreateDumb>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmModeCreateDumb) };

    if req.width == 0 || req.height == 0 || req.bpp == 0 {
        return EINVAL;
    }
    if req.bpp != 32 {
        // Only 32 bpp dumb buffers supported for now
        return EINVAL;
    }

    // Pitch and size come from the translated `virtgpu_gem::dumb_create_ours`,
    // so the 64-byte row alignment (divergence G1) is stated once next to the
    // upstream arithmetic it differs from (`dumb_create_upstream` uses
    // `width * 4` unaligned), rather than being an inline expression with a
    // comment. The module also decides `blob`, which is upstream's
    // `has_resource_blob && !has_virgl_3d` -- previously not consulted here at
    // all.
    let (has_blob, has_virgl3d) = {
        let g = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock();
        match g.as_ref() {
            Some(dev) => (dev.has_resource_blob, dev.has_virgl),
            None => (false, false),
        }
    };
    let Ok(dc) = crate::drivers::drm::virtgpu_gem::dumb_create_ours(
        crate::drivers::drm::virtgpu_gem::DumbCreateParams {
            width: req.width,
            height: req.height,
            bpp: req.bpp,
            has_resource_blob: has_blob,
            has_virgl_3d: has_virgl3d,
        },
    ) else {
        return EINVAL;
    };
    let pitch = dc.pitch;
    let size = dc.size;
    crate::dprintln!(
        "[dumb] {}x{} pitch={} size={:#x} blob={}",
        req.width, req.height, pitch, size, dc.blob
    );
    let pages = ((size as usize) + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize;

    let phys = match FRAME_ALLOCATOR.lock().alloc_pages(pages, PAGE_SIZE) {
        Ok(p) => p,
        Err(_) => return ENOMEM,
    };

    // Zero the buffer so the display starts clean
    unsafe {
        let virt = (HHDM_BASE.0 + phys.0) as *mut u8;
        core::ptr::write_bytes(virt, 0, pages * PAGE_SIZE as usize);
    }

    // If VirtIO-GPU present, create host resource + backing for proper KMS.
    // ponytail: classic 2D resources ONLY for display buffers — never guest
    // blobs here. Mesa pads the dumb BO (800->832 rows) while our scanout
    // metadata says 800; virglrenderer misplaces the 800-row window inside
    // the 832-row blob backing (32px shift-up + black bar at bottom). A 2D
    // resource carries explicit WxH so the host allocates exactly the
    // scanned size. Costs one TRANSFER+FLUSH per flip; flips are
    // render-gated (~1/s idle). (3D/virpipe contexts keep blobs via the
    // VIRTGPU_RESOURCE_CREATE ioctl path, untouched.)
    let (virtio_rid, blob) = {
        let mut g = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock();
        if let Some(gpu) = g.as_mut() {
            let rid = gpu.alloc_resource_id();
            // Only advertise a host resource if the host actually made one.
            // Previously both results were ignored, so a rejected create
            // still handed back a rid: every later SET_SCANOUT/TRANSFER for
            // that buffer failed on the host and the flip silently showed
            // nothing, with no error anywhere to explain it.
            let made = gpu.create_resource_2d(
                rid, req.width, req.height,
                crate::drivers::virtio::gpu::VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM,
            ) && gpu.attach_backing(rid, &[(phys.0, size as u32)]);
            if made {
                crate::drivers::drm::virtgpu::note_rid_created(rid, crate::drivers::drm::virtgpu::RID_KIND_2D);
                (Some(rid), false)
            } else {
                // Tell the host to drop the half-made resource, then fall
                // back to the GOP blit path for this buffer.
                //
                // Note: if `create_resource_2d` itself failed, the host never
                // had this rid, so the UNREF below is guaranteed to be rejected
                // with INVALID_RESOURCE_ID. It is still the right thing to
                // attempt for the case where only the attach failed. Tagged so
                // it can be told apart from the other release sites.
                crate::drivers::drm::virtgpu::note_rid_unref(
                    rid, crate::drivers::drm::virtgpu::RID_KIND_2D, "2d-create-fail");
                gpu.detach_backing_and_unref(rid);
                (None, false)
            }
        } else { (None, false) }
    };

    let mut drm = GLOBAL_DRM.lock();
    let dev = match drm.as_mut() { Some(d) => d, None => return ENOMEM };

    // Allocate from fixed pool (Linux-style) to avoid Vec::push reallocation crash
    let slot = match dev.alloc_dumb_slot() {
        Some(s) => s,
        None => return ENOMEM,
    };
    let handle = dev.alloc_id();
    dev.dumb_buffers[slot] = Some(DumbBuffer {
        id: handle,
        width:  req.width,
        height: req.height,
        bpp:    req.bpp,
        pitch,
        size,
        phys,
        pages,
        virtio_rid,
        blob,
        is_3d: false,
    });

    crate::dprintln!("[drm] create_dumb handle={} {}x{} pitch={} size={}", handle, req.width, req.height, pitch, size);

    req.handle = handle;
    req.pitch  = pitch;
    req.size   = size;
    0
}

// ── DRM_IOCTL_MODE_MAP_DUMB ──────────────────────────────────────────────────

/// Return an mmap offset for the dumb buffer.
///
/// We encode the physical address directly as the offset.  When userspace
/// calls `mmap(0, size, PROT_RW, MAP_SHARED, fd, offset)` on the DRM fd,
/// `sys_mmap` detects the `MAP_DEVICE` flag and calls `mmap_device(offset, …)`
/// which maps the physical pages into the user address space.
///
/// The Linux DRM ABI says the offset is only meaningful as an argument to
/// `mmap(2)`; we use the physical address directly, which works because
/// DragOS's `mmap_device` expects a physical address.
fn handle_map_dumb(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmModeMapDumb>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmModeMapDumb) };

    let drm = GLOBAL_DRM.lock();
    let dev = match drm.as_ref() {
        Some(d) => d,
        None => return EINVAL,
    };

    // Look up in fixed pool array (Linux-style pre-allocated)
    let db = match dev.find_dumb(req.handle) {
        Some(d) => *d,
        None => return ENOENT,
    };

    // Encode the physical address as the mmap offset.
    // Userspace must mmap with MAP_SHARED | MAP_DEVICE (flag 0x100000 in our
    // kernel) so that sys_mmap routes to mmap_device().
    req.offset = db.phys.0;
    crate::dprintln!("[map dumb] handle={} -> offset={:#x}", req.handle, req.offset);
    mark_scanout_dirty();
    0
}

// ── DRM_IOCTL_MODE_DESTROY_DUMB ──────────────────────────────────────────────

fn handle_destroy_dumb(argp: u64) -> u64 {
    // Set when the destroyed buffer still has a host resource to release. The
    // GPU lock is taken only after `drm` is dropped, never while it is held:
    // GLOBAL_DRM -> GLOBAL_VIRTIO_GPU is the reverse of what the execbuffer
    // path used to do, and both are cli()-taking SpinLocks, so the pair can
    // deadlock (AGENTS.md §1).
    let mut pending_rid_unref: Option<u32> = None;
    if !vr_read(argp, core::mem::size_of::<DrmModeDestroyDumb>()) {
        return EFAULT;
    }
    let req = unsafe { &*(argp as *const DrmModeDestroyDumb) };

    let mut drm = GLOBAL_DRM.lock();
    let dev = match drm.as_mut() {
        Some(d) => d,
        None => return EINVAL,
    };

    // Find and remove from fixed pool (Linux-style pre-allocation)
    let slot = match dev.dumb_buffers.iter()
        .enumerate()
        .find(|(_, buf)| buf.as_ref().map(|b| b.id == req.handle).unwrap_or(false))
    {
        Some((slot, _)) => slot,
        None => return ENOENT,
    };

    let freed = dev.dumb_buffers[slot].take();
    // Release GLOBAL_DRM before taking the allocator: two unrelated locks,
    // neither of which may be nested under the device lock.
    let (phys, pages) = match freed {
        Some(db) => {
            if let Some(rid) = db.virtio_rid {
                // Diagnostic only. The ledger is a leaf lock: it is taken under
                // GLOBAL_DRM here and nothing ever takes GLOBAL_DRM while
                // holding it, so this cannot form a cycle.
                crate::drivers::drm::virtgpu::note_rid_unref(
                    rid, crate::drivers::drm::virtgpu::RID_KIND_2D, "dumb-destroy");
                // Take the GPU lock only after GLOBAL_DRM is released (below).
                // Holding GLOBAL_DRM across GLOBAL_VIRTIO_GPU here is the
                // reverse of the execbuffer path's historical order, and both
                // are cli()-taking SpinLocks -- an ABBA cycle that deadlocks
                // the loser with interrupts off (AGENTS.md §1). The rid is a
                // plain copy, so nothing here needs the DRM lock held.
                pending_rid_unref = Some(rid);
            }
            (db.phys.0, db.pages)
        }
        None => return ENOENT,
    };
    drop(drm);
    if let Some(rid) = pending_rid_unref {
        if let Some(mut g) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock().as_mut() {
            g.detach_backing_and_unref(rid);
        }
    }

    // Return the backing frames to the allocator. The dumb pool has a fixed
    // number of slots but the frames behind it were never reclaimed, so a
    // client that creates and destroys dumb buffers in a loop (Mesa does this
    // on every resize) leaked every frame until the machine wedged.
    if pages > 0 {
        // ponytail: TEMP-DIAG. This reclaim is unconditional, so a buffer that
        // is still mapped has its frames handed straight back to the allocator
        // while the mapping's PTEs keep pointing at them -- a use-after-free
        // that a later allocation will silently reuse. Correlate these lines
        // with `[mmap dev]` to confirm.
        crate::dprintln!(
            "[dumb-destroy] phys={:#x} pages={} bytes={:#x}  RECLAIMED (any live mapping now dangles)",
            phys,
            pages,
            pages * 4096
        );
        let mut alloc = crate::memory::FRAME_ALLOCATOR.lock();
        alloc.dealloc_pages(shared::memory::address::PhysAddr(phys), pages);
    }
    0
}

fn handle_gem_close(argp: u64) -> u64 {
    if !vr_read(argp, 8) {
        return EFAULT;
    }
    let handle = unsafe { *(argp as *const u32) };
    // A 3D or blob resource is registered in BOTH tables, and both records carry
    // the SAME host resource id: the pool entry exists so mmap and ADDFB can
    // find the pages, and the manager entry exists so submits can find the
    // virgl bookkeeping. So take both records, then release the host copy
    // exactly ONCE.
    //
    // Releasing from both -- which this did -- sent a second RESOURCE_UNREF for
    // an id the host had already dropped, and the host rejected it:
    //   virtio_gpu_virgl_process_cmd: ctrl 0x102, error 0x1203
    // (0x1203 is RESP_ERR_INVALID_RESOURCE_ID). ~470 of those per 90 s run, one
    // per 3D close. The guest-side ledger named the cause exactly: every one was
    // "rid created as 3D, unref'd via the dumb path, then unref'd again as 3D".
    //
    // Note the ordering: GLOBAL_DRM is scoped and released before the manager
    // and the virtio device are locked. The previous code held GLOBAL_DRM across
    // detach_backing_and_unref, nesting GLOBAL_DRM -> GLOBAL_VIRTIO_GPU for no
    // reason.
    let pool_rid = {
        let mut drm = GLOBAL_DRM.lock();
        let dev = match drm.as_mut() {
            Some(d) => d,
            None => return EINVAL,
        };
        dev.dumb_buffers
            .iter()
            .position(|b| b.as_ref().map(|x| x.id == handle).unwrap_or(false))
            .and_then(|slot| dev.dumb_buffers[slot].take())
            .and_then(|db| db.virtio_rid)
    };

    // GEM_CLOSE is the only path that can release a 3D resource, and it used to
    // do the opposite: it dropped the bookkeeping entry and returned success, so
    // the client believed the buffer was freed while its guest backing pages
    // stayed consumed by the allocator *and* the record of them was gone.
    // There is no VIRTGPU_RESOURCE_UNREF ioctl in upstream Linux (the ioctl
    // table ends at CONTEXT_INIT = 0x0b), so GEM_CLOSE is the sole reclaim
    // path. A compositor churning buffers leaked 4 MB per resource, ~200
    // resources per 90 s. So the page release below stays.
    let mgr_res = crate::drivers::drm::virtgpu::VIRTGPU_MANAGER
        .lock()
        .resources
        .remove(&handle);

    // One host release for this close, whichever table the id came from.
    let rid = mgr_res.as_ref().map(|r| r.res_id).or(pool_rid);
    if let Some(rid) = rid {
        crate::drivers::drm::virtgpu::note_rid_unref(
            rid,
            if mgr_res.is_some() {
                crate::drivers::drm::virtgpu::RID_KIND_3D
            } else {
                crate::drivers::drm::virtgpu::RID_KIND_2D
            },
            "gem-close",
        );
        if let Some(mut g) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.lock().as_mut() {
            g.detach_backing_and_unref(rid);
        }
    }

    if let Some(res) = mgr_res {
        // Host-allocated (HOST3D) blobs have no guest backing: phys/pages are
        // unused and the memory lives in the hostmem window.
        if !res.is_host_blob && res.pages > 0 {
            unsafe {
                crate::memory::FRAME_ALLOCATOR
                    .lock()
                    .dealloc_pages(shared::memory::address::PhysAddr(res.phys.0), res.pages);
            }
        }
        crate::dprintln!(
            "[DRM] gem_close handle={} released 3D rid={} pages={}",
            handle, res.res_id, res.pages
        );
        return 0;
    }
    if let Some(rid) = pool_rid {
        crate::dprintln!("[DRM] gem_close handle={} released 2D rid={}", handle, rid);
        return 0;
    }
    crate::dprintln!("[DRM] gem_close handle={} (no record)", handle);
    0
}

// ── DRM_IOCTL_PRIME_HANDLE_TO_FD ─────────────────────────────────────────────

/// Export a dumb-buffer or blob-resource handle as a PRIME dma-buf fd.
///
/// Allocates a *real* VFS file descriptor in the calling process's fd table,
/// backed by a `VNode::DmaBuf`. This is required because libwayland-client
/// calls `fcntl(fd, F_DUPFD_CLOEXEC, 0)` when marshalling
/// `zwp_linux_buffer_params_v1.add`, and that fcntl looks the fd up in the
/// VFS fd_table — a synthetic number stored only in a HashMap yields EBADF.
/// DRM_IOCTL_PRIME_HANDLE_TO_FD (0xC00C642D).
fn handle_prime_handle_to_fd(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmPrimeHandle>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmPrimeHandle) };

    let mut drm = GLOBAL_DRM.lock();
    let dev = match drm.as_mut() {
        Some(d) => d,
        None => return EINVAL,
    };

    if !dev.has_dumb(req.handle) {
        return ENOENT;
    }

    // Copy out dumb buffer info before releasing the DRM lock.
    let (dumb_handle, dumb_res_id, dumb_size, dumb_is_blob) = {
        let dumb = match dev.find_dumb(req.handle) {
            Some(d) => d,
            None => return ENOENT,
        };
        (dumb.id, dumb.virtio_rid, dumb.size, dumb.blob)
    };
    let dma_buf = DmaBufFile {
        handle: dumb_handle,
        res_id: dumb_res_id,
        size: dumb_size,
        is_blob: dumb_is_blob,
    };

    drop(drm);

    // Allocate a real VFS fd in the calling process's fd table.
    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EINVAL,
    };
    let mut p = t.process.lock();
    let db_size = dma_buf.size;
    let db_is_blob = dma_buf.is_blob;
    let dma_buf_vnode = VNode::DmaBuf(Arc::new(SpinLock::new(dma_buf)));
    let open_file = Arc::new(SpinLock::new(crate::fs::vfs::OpenFile {
        vnode: dma_buf_vnode,
        flags: 2, // O_RDWR
        path: alloc::string::String::from("/dev/dri/prime"),
    }));
    let fd = p.fd_table.iter().position(|s| s.is_none()).unwrap_or_else(|| {
        p.fd_table.push(None);
        p.fd_table.len() - 1
    });
    p.fd_table[fd] = Some(open_file);
    let fd_num = fd as i32;
    drop(p);
    crate::tasks::set_current_thread(Some(t));

    req.fd = fd_num;
    // Register the prime fd with the global handle registry so that
    // mmap() (which maps via GLOBAL_DRM.prime_handles) can resolve it
    // to the underlying dumb-buffer physical memory.  Without this, the
    // VFS fd is readable (as a VNode::DmaBuf) but unmappable.
    GLOBAL_DRM.lock().as_mut().map(|d| {
        d.prime_handles.insert(fd_num as u32, req.handle);
    });
    crate::dprintln!("[PRIME] handle_to_fd: handle={} -> vfs fd={} (size={} blob={})",
                     req.handle, fd_num, db_size, db_is_blob);
    0
}

// ── DRM_IOCTL_PRIME_FD_TO_HANDLE ─────────────────────────────────────────────

/// Import a PRIME dma-buf fd back into a dumb-buffer handle.
/// DRM_IOCTL_PRIME_FD_TO_HANDLE (0xC00C642E).
fn handle_prime_fd_to_handle(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmPrimeHandle>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmPrimeHandle) };

    let t = match crate::tasks::current_thread() {
        Some(t) => t,
        None => return EINVAL,
    };
    let p = t.process.lock();

    let fd = req.fd as usize;
    if fd >= p.fd_table.len() {
        return ENOENT;
    }
    let Some(vnode) = &p.fd_table[fd] else {
        return ENOENT;
    };
    let (handle, res_id) = match &vnode.lock().vnode {
        VNode::DmaBuf(dma_buf) => {
            let db = dma_buf.lock();
            (db.handle, db.res_id)
        }
        _ => return EINVAL,
    };
    drop(p);
    crate::tasks::set_current_thread(Some(t));

    // Attach the underlying host resource to the caller's context so that
    // the caller's GL/EGL context can access the blob memory. Without this,
    // virglrenderer on the host rejects the resource because it is not
    // associated with the caller's context.
    if let Some(res_id) = res_id {
        let me = crate::drivers::drm::virtgpu::calling_pid();
        let ctx_id = {
            let mgr = VIRTGPU_MANAGER.lock();
            crate::drivers::drm::virtgpu::caller_owns_ctx(&mgr, me, false)
        };
        if let Some(ctx_id) = ctx_id {
            let mut gpu = GLOBAL_VIRTIO_GPU.lock();
            if let Some(g) = gpu.as_mut() {
                if !g.ctx_attach_resource(ctx_id, res_id) {
                    crate::dprintln!(
                        "[PRIME] ctx_attach_resource FAILED ctx={} res={}",
                        ctx_id, res_id
                    );
                } else {
                    crate::dprintln!(
                        "[PRIME] ctx_attach_resource OK ctx={} res={}",
                        ctx_id, res_id
                    );
                }
            }
        }
    }

    req.handle = handle;
    crate::dprintln!("[PRIME] fd_to_handle: vfs fd={} -> handle={} res_id={:?}", fd, handle, res_id);
    0
}

// ── DRM_IOCTL_SYNCOBJ_* ──────────────────────────────────────────────────────
//
// ponytail: binary syncobjs only. Each handle owns one DmaFence; SIGNAL
// signals it and wakes SYNCOBJ_WAIT_QUEUE via wake_syncoff_waiters (lock
// released before the wake, per the data-queue/SCHEDULER invariant). WAIT
// fast-paths already-signaled handles, otherwise parks with a tick deadline
// (ETIMEDOUT on expiry). No timelines, no fd export yet (EINVAL).

fn handle_syncoff_create(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmSyncobjCreate>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmSyncobjCreate) };
    // flag bit 0 = DRM_SYNCOBJ_CREATE_SIGNALED
    let signaled = req.flags & 1 != 0;
    let id = syncobj::alloc_id();
    syncobj::SYNCOBJS
        .lock()
        .insert(id, Arc::new(SpinLock::new(syncobj::SyncObj::new(signaled))));
    req.handle = id;
    crate::dprintln!("[DRM] syncobj create handle={} signaled={}", id, signaled);
    0
}

fn handle_syncoff_destroy(argp: u64) -> u64 {
    if !vr_read(argp, core::mem::size_of::<DrmSyncobjDestroy>()) {
        return EFAULT;
    }
    let req = unsafe { &*(argp as *const DrmSyncobjDestroy) };
    if syncobj::SYNCOBJS.lock().remove(&req.handle).is_none() {
        return EINVAL;
    }
    // Waiters parked on this handle can never be signalled again, and an
    // untimed wait has no deadline the tick sweep can expire. Wake them so
    // they re-check and take the "handle vanished" branch instead of hanging.
    crate::tasks::wake_syncoff_waiters(req.handle, 0);
    crate::dprintln!("[DRM] syncobj destroy handle={}", req.handle);
    0
}

fn syncobj_copy_handles(ptr: u64, count: u32) -> Option<alloc::vec::Vec<u32>> {
    if count == 0 || count > 64 || ptr == 0 {
        return None;
    }
    let bytes = count as usize * 4;
    if !crate::memory::space::validate_user_read_range(ptr, bytes) {
        return None;
    }
    let mut out = alloc::vec![0u32; count as usize];
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u32, out.as_mut_ptr(), count as usize);
    }
    Some(out)
}

fn handle_syncoff_wait(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmSyncobjWait>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmSyncobjWait) };
    if req.count_handles == 0 {
        // Mesa's vn_syncobj_wait() may call this with count=0; return success
        // rather than EINVAL so Mesa doesn't report VK_ERROR_DEVICE_LOST.
        return 0;
    }
    let handles = match syncobj_copy_handles(req.handles, req.count_handles) {
        Some(h) => h,
        None => return EINVAL,
    };
    // flag bit 0 = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL
    let wait_all = req.flags & 1 != 0;
    let pid = crate::tasks::get_current_pid();
    let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
    let deadline = if req.timeout_nsec < 0 {
        u64::MAX
    } else {
        now + (req.timeout_nsec.max(0) as u64) / 10_000_000 + 1 // 10ms ticks
    };

    let satisfied = |handles: &[u32], first: &mut u32| -> bool {
        let map = syncobj::SYNCOBJS.lock();
        if wait_all {
            let mut ok = true;
            for &h in handles {
                match map.get(&h) {
                    Some(s) if s.lock().is_signaled() => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            ok
        } else {
            for (i, &h) in handles.iter().enumerate() {
                if let Some(s) = map.get(&h) {
                    if s.lock().is_signaled() {
                        *first = i as u32;
                        return true;
                    }
                }
            }
            false
        }
    };

    let mut first = 0u32;
    if satisfied(&handles, &mut first) {
        req.first_signaled = first;
        return 0;
    }
    crate::dprintln!("[SYNCOBJ_WAIT] count={} timeout_nsec={} handles={:?} -> ETIMEDOUT", req.count_handles, req.timeout_nsec, handles);
    if req.timeout_nsec == 0 {
        return ETIMEDOUT;
    }
    // Park on the first unsignaled handle; the smp park path re-checks for
    // a lost wakeup, and signal/deadline wakes resume us here for a rescan.
    let park_handle = handles
        .iter()
        .find(|&&h| {
            syncobj::SYNCOBJS
                .lock()
                .get(&h)
                .map(|s| !s.lock().is_signaled())
                .unwrap_or(false)
        })
        .copied()
        .unwrap_or(handles[0]);
    crate::tasks::block_current_thread(crate::tasks::PendingBlock::Syncobj {
        handle: park_handle,
        deadline,
        pid,
    });
    if satisfied(&handles, &mut first) {
        req.first_signaled = first;
        return 0;
    }
    // Woken but not satisfied: either the deadline passed (tick sweep) or
    // the handle vanished (destroyed while parked).
    let now2 = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
    if now2 >= deadline {
        return ETIMEDOUT;
    }
    EINVAL
}

fn handle_syncoff_signal(argp: u64) -> u64 {
    crate::dprintln!("[SYNCOBJ_SIGNAL] ENTRY argp={:x}", argp);
    if !vr_read(argp, core::mem::size_of::<DrmSyncobjArray>()) {
        return EFAULT;
    }
    let req = unsafe { &*(argp as *const DrmSyncobjArray) };
    let handles = match syncobj_copy_handles(req.handles, req.count_handles) {
        Some(h) => h,
        None => return EINVAL,
    };
    let pid = crate::tasks::get_current_pid();
    for &h in &handles {
        if let Some(s) = syncobj::SYNCOBJS.lock().get(&h) {
            s.lock().signal();
            crate::tasks::wake_syncoff_waiters(h, pid);
        }
    }
    crate::dprintln!("[DRM] syncobj signal count={}", handles.len());
    0
}

fn handle_syncoff_reset(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmSyncobjArray>()) {
        return EFAULT;
    }
    let req = unsafe { &*(argp as *const DrmSyncobjArray) };
    let handles = match syncobj_copy_handles(req.handles, req.count_handles) {
        Some(h) => h,
        None => return EINVAL,
    };
    let mut map = syncobj::SYNCOBJS.lock();
    let mut reset: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
    for &h in &handles {
        if map.contains_key(&h) {
            // Replacing the SyncObj drops the fence the waiters parked on, so
            // they would block forever. Collect and wake them after releasing
            // the map lock (see the data-queue/SCHEDULER ordering rule).
            map.insert(h, Arc::new(SpinLock::new(syncobj::SyncObj::new(false))));
            reset.push(h);
        }
    }
    drop(map);
    for h in reset {
        crate::tasks::wake_syncoff_waiters(h, 0);
    }
    crate::dprintln!("[DRM] syncobj reset count={}", handles.len());
    0
}



// ── DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD / FD_TO_HANDLE ─────────────────────
//
// ponytail: synthetic mapping identical to PRIME — syncobj handle ↔ fd
// lives in the same registry as dumb-buffer handles (dev.syncobj_handles).
// Not a real dma-buf, but sufficient for virpipe when only one process
// uses the buffer (no cross-process sharing needed yet).

fn handle_syncoff_handle_to_fd(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmSyncobjHandle>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmSyncobjHandle) };

    let efd = crate::arch::x86_64::syscalls::epoll::sys_eventfd(1, 0);
    if efd > 0x8000_0000_0000_0000 {
        return efd;
    }
    req.fd = efd as i32;
    crate::dprintln!("[DRM] syncobj handle to fd: handle={} -> fd={}", req.handle, req.fd);
    0
}

fn handle_syncoff_fd_to_handle(argp: u64) -> u64 {
    if !vr(argp, core::mem::size_of::<DrmSyncobjHandle>()) {
        return EFAULT;
    }
    let req = unsafe { &mut *(argp as *mut DrmSyncobjHandle) };

    let id = crate::drivers::drm::syncobj::alloc_id();
    crate::drivers::drm::syncobj::SYNCOBJS
        .lock()
        .insert(id, alloc::sync::Arc::new(crate::arch::sync::spinlock::SpinLock::new(
            crate::drivers::drm::syncobj::SyncObj::new(true),
        )));
    req.handle = id;
    crate::dprintln!("[DRM] syncobj fd to handle: fd={} -> handle={}", req.fd, req.handle);
    0
}

// ── Utilities ─────────────────────────────────────────────────────────────────

#[inline]
fn align_up_32(val: u32, align: u32) -> u32 {
    (val + align - 1) & !(align - 1)
}

/// Get the current thread's PID (for syncobj ownership checks and VirtIO-GPU
/// context routing).
pub(crate) fn current_pid() -> u64 {
    crate::tasks::current_thread()
        .map(|t| t.process.lock().pid)
        .unwrap_or(0)
}

// ── Periodic scanout flush ───────────────────────────────────────────────────

/// Push the current scanout framebuffer to the host display.
///
/// Called ~60 Hz from the timer tick. Doom-style clients mmap a dumb buffer
/// and draw without further ioctls; real hardware scans out guest memory
/// directly, but virtio-gpu needs an explicit transfer+flush per frame.
pub fn periodic_flush() {
    // ponytail: a flip <2s ago already presented everything newer than any
    // pending damage; transferring again here (~16ms after every flip)
    // duplicates each present with a full-frame copy. This path exists only
    // for silent renderers that never flip (doom flips per frame itself, so
    // in practice this fires solely as a dead-compositor safety net).
    // (GOP branch below shares the gate: page_flip already blitted.)
    let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
    if now.saturating_sub(LAST_FLIP_TICK.load(core::sync::atomic::Ordering::Relaxed)) <= 2000 {
        return;
    }
    // If the scanout hasn't been marked dirty by page-flips, mode setting, or DIRTYFB,
    // skip the full-frame VirtIO transfer and GOP memcpy.
    // Provide a slow heartbeat (~5 Hz, every 12 calls) so dumb mmap renderers without
    // DIRTYFB still progress smoothly without burning CPU at 60 Hz.
    // ponytail: ...unless a real flip landed recently — a live compositor's
    // flips already present everything, so the heartbeat only runs for
    // silent renderers (>2s since last flip). Doom flips per frame itself.
    static HB: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let now = crate::time::TICKS.load(core::sync::atomic::Ordering::Relaxed) as u64;
    let since_flip = now.saturating_sub(LAST_FLIP_TICK.load(core::sync::atomic::Ordering::Relaxed));
    let heartbeat = HB.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 12 == 0
        && since_flip > 2000;

    // Test the flag *without* clearing it. Clearing up front loses the damage
    // whenever we then bail out below — a failed try_lock (a compositor
    // hammering PAGE_FLIP/DIRTYFB, i.e. exactly while dragging a window)
    // would discard the pending damage with nothing presented in its place,
    // and the next flip would find SCANOUT_DIRTY already false.
    if !SCANOUT_DIRTY.load(core::sync::atomic::Ordering::Acquire) && !heartbeat {
        return;
    }

    let mut drm = match GLOBAL_DRM.try_lock() {
        Some(d) => d,
        // Could not get the device: leave the flag set so the damage is
        // retried on the next tick instead of being dropped.
        None => return,
    };
    // We hold the device lock, so it is now safe to consume the flag.
    SCANOUT_DIRTY.store(false, core::sync::atomic::Ordering::Release);
    let dev = match drm.as_mut() {
        Some(d) => d,
        None => return,
    };
    // Find the active CRTC (fb wired by SETCRTC / PAGE_FLIP).
    let crtc = match dev.crtcs.iter().find(|c| c.fb_id != 0) {
        Some(c) => c,
        None => return,
    };
    let fb = match dev.framebuffers.get(&crtc.fb_id) {
        Some(f) => *f,
        None => return,
    };
    let db = match dev.find_dumb(fb.handle) {
        Some(d) => *d,
        None => return,
    };
    // ponytail: scanout is the CRTC mode size, never the FB size — Mesa
    // pads GBM BOs (800->832 rows); this 60Hz path re-programs scanout
    // every pass and would re-open the black bar the flip paths closed.
    let (scan_w, scan_h) = match crtc.mode {
        Some(m) => (fb.width.min(m.width), fb.height.min(m.height)),
        None => (fb.width, fb.height),
    };
    if db.virtio_rid.is_none() {
        // GOP-backed dumb buffer: blit at reduced rate (every 4th call).
        static SLOW: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
        if SLOW.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 4 != 0 {
            return;
        }
        blit_dumb_to_gop(db.phys, scan_w, scan_h, fb.pitch);
        return;
    }
    if db.blob {
        // Guest-blob resource: host scans backing memory directly, nothing to do.
        // (page_flip programs SET_SCANOUT_BLOB every flip; no legacy SET_SCANOUT here —
        // that 2D command on a blob rid corrupts host scanout state.)
        return;
    }
    if db.is_3d {
        // 3D virgl resource: the rendered content lives in the host's GL
        // texture, not in guest memory.  TRANSFER_TO_HOST_2D reads the
        // zero-filled guest backing and pushes it over the rendered texture,
        // producing black frames.  The compositor's own PAGE_FLIP already
        // presents 3D resources correctly (SET_SCANOUT + RESOURCE_FLUSH), so
        // the heartbeat has nothing useful to contribute.
        return;
    }
    let rid = db.virtio_rid.unwrap();
    drop(drm);
    if let Some(mut guard) = crate::drivers::virtio::gpu::GLOBAL_VIRTIO_GPU.try_lock() {
        if let Some(g) = guard.as_mut() {
            let rect = VirtioGpuRect { x: 0, y: 0, width: scan_w, height: scan_h };
            g.set_scanout(0, rid, rect);
            // ponytail: unconditional full transfer — this path serves silent
            // mmap renderers with no damage info. It must NOT consume the
            // pending damage set (that belongs to the next real flip);
            // the flip gate above already suppresses this while a live
            // compositor presents.
            g.transfer_to_host_2d(rid, rect, 0);
            g.flush_resource(rid, rect);
        }
    }
    // ponytail: no GOP shadow blit — host scans the virtio resource above.
    // (GOP-only dumb buffers return early at the top of this function.)
}
