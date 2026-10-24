//! AP trampoline: copies the real-mode stub (see `trampoline.asm`) to a low
//! physical page (0x8000) and exposes the slots the BSP patches before SIPI.

use shared::memory::address::{PAGE_SIZE, PhysAddr, VirtAddr};

use crate::arch::x86_64::apic;

pub const TRAMPOLINE_ADDR: u64 = 0x8000;

// Offsets of the patch slots within the assembled blob.
//
// These come from `build.rs`, which assembles `trampoline.asm` and reads the
// slot offsets out of nasm's listing. They were previously hand-written here
// as 0xA8/0xB0/0xB8, which made the Rust constants and the assembly layout two
// independent copies of one fact: inserting a single byte above the slots in
// the .asm would have left these stale, and the BSP would then patch the wrong
// three bytes and every AP would jump to garbage after INIT-SIPI. The values
// are unchanged (0xA8/0xB0/0xB8) — only the source of truth moved.
//
// `build.rs` also asserts each offset is 8-aligned, lies inside the blob, and
// that the three slots are adjacent, so a rename or a layout change in the
// .asm is a build failure rather than a boot hang.
pub const BLOB_ENTRY_OFF: usize = trampoline_offsets::ENTRY;
pub const BLOB_CR3_OFF: usize = trampoline_offsets::CR3;
pub const BLOB_STACK_OFF: usize = trampoline_offsets::STACK;

/// Offsets emitted by `build.rs` from nasm's listing of `trampoline.asm`.
mod trampoline_offsets {
    include!(concat!(env!("OUT_DIR"), "/trampoline_offsets.rs"));
}

static TRAMPOLINE_BIN: &[u8] =
    include_bytes!(concat!(env!("TRAMPOLINE_BIN"), ""));

fn blob_pages() -> usize {
    (TRAMPOLINE_BIN.len() + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize
}

/// Copies the trampoline blob into the low page at `TRAMPOLINE_ADDR`.
pub fn install() {
    unsafe {
        core::ptr::copy_nonoverlapping(
            TRAMPOLINE_BIN.as_ptr(),
            TRAMPOLINE_ADDR as *mut u8,
            TRAMPOLINE_BIN.len(),
        );
    }
}

/// Patches the trampoline data slots with the BSP-provided values.
pub fn patch(entry: u64, cr3: u64, idle_stack_top: u64) {
    unsafe {
        let base = TRAMPOLINE_ADDR as *mut u8;
        *(base.add(BLOB_ENTRY_OFF) as *mut u64) = entry;
        *(base.add(BLOB_CR3_OFF) as *mut u64) = cr3;
        *(base.add(BLOB_STACK_OFF) as *mut u64) = idle_stack_top;
    }
}

/// Wakes an AP (local APIC ID `lapic_id`) via INIT-SIPI at TRAMPOLINE_ADDR.
pub fn wake_ap(lapic_id: u32) {
    apic::send_init_ipi(lapic_id);
    apic::send_sipi_ipi(lapic_id, TRAMPOLINE_ADDR);
    crate::time::busy_wait_ms(1);
    apic::send_sipi_ipi(lapic_id, TRAMPOLINE_ADDR);
}

/// Identity-maps the trampoline low pages in `cr3` so the AP can execute the
/// stub after it enables paging with that CR3.
pub fn identity_map_trampoline(cr3: PhysAddr) {
    use shared::memory::flags::PageTableFlags;
    let alloc = &mut *crate::memory::FRAME_ALLOCATOR.lock();
    for i in 0..blob_pages() {
        let virt = VirtAddr(TRAMPOLINE_ADDR + (i as u64) * PAGE_SIZE);
        let _ = crate::memory::space::mapper_for(cr3).map_4k(
            alloc,
            virt,
            PhysAddr::new(virt.0),
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE,
        );
    }
}
