//! GEM — Graphics Execution Manager object lifecycle.
//! Mirrors Linux `struct drm_gem_object` (include/drm/drm_gem.h).

#![allow(unused)]

use alloc::{collections::BTreeMap, sync::Arc};
use crate::arch::sync::spinlock::SpinLock;
use crate::memory::dma::SgTable;

/// A single GEM buffer object.
pub struct GemObject {
    pub handle: u32,
    /// Backing scatter-gather memory pages.
    pub sg: Option<SgTable>,
    /// Size in bytes.
    pub size: usize,
    /// CPU-accessible HHDM virtual address (if pinned).
    pub cpu_addr: Option<*mut u8>,
    /// GPU virtual address (assigned by driver).
    pub gpu_addr: u64,
}

// SAFETY: GEM objects are protected by the GemManager lock.
unsafe impl Send for GemObject {}
unsafe impl Sync for GemObject {}

impl GemObject {
    /// Allocate a new GEM object backed by `size` bytes of scatter-gather RAM.
    pub fn alloc(handle: u32, size: usize) -> Option<Self> {
        let sg = SgTable::alloc(size)?;
        Some(Self {
            handle,
            sg: Some(sg),
            size,
            cpu_addr: None,
            gpu_addr: 0,
        })
    }

    /// Pin the object for CPU access, returning a virtual address.
    /// Uses HHDM mapping for the first SgEntry.
    pub fn pin_cpu(&mut self) -> Option<*mut u8> {
        if let Some(sg) = &self.sg {
            if let Some(entry) = sg.entries.first() {
                let virt = (shared::boot::boot_info::HHDM_BASE.0 + entry.phys.as_u64()) as *mut u8;
                self.cpu_addr = Some(virt);
                return Some(virt);
            }
        }
        None
    }
}

/// Global GEM object registry keyed by handle.
pub struct GemManager {
    objects: BTreeMap<u32, GemObject>,
    next_handle: u32,
}

impl GemManager {
    pub const fn new() -> Self {
        Self {
            objects: BTreeMap::new(),
            next_handle: 1,
        }
    }

    pub fn create(&mut self, size: usize) -> Option<u32> {
        let handle = self.next_handle;
        self.next_handle = self.next_handle.wrapping_add(1);
        let obj = GemObject::alloc(handle, size)?;
        self.objects.insert(handle, obj);
        Some(handle)
    }

    pub fn lookup(&self, handle: u32) -> Option<&GemObject> {
        self.objects.get(&handle)
    }

    pub fn lookup_mut(&mut self, handle: u32) -> Option<&mut GemObject> {
        self.objects.get_mut(&handle)
    }

    pub fn release(&mut self, handle: u32) -> bool {
        self.objects.remove(&handle).is_some()
    }
}

impl Default for GemManager {
    fn default() -> Self {
        Self::new()
    }
}

pub static GLOBAL_GEM: SpinLock<GemManager> = SpinLock::new(GemManager::new());
