//! DRM sync objects — kernel-side binary signal tracking for GPU fences.
//!
//! ponytail: binary syncobjs only (no timelines). Each syncobj owns one
//! DmaFence; signaling the syncobj signals the fence and wakes parked
//! waiters via wake_syncoff_waiters. Handles are small u32 ids in a global
//! registry, mirroring the PRIME fd registry shape.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::sync::spinlock::SpinLock;
use crate::drivers::linuxkpi::fence::DmaFence;

pub struct SyncObj {
    pub fence: Arc<DmaFence>,
}

impl SyncObj {
    pub fn new(signaled: bool) -> Self {
        let fence = Arc::new(DmaFence::new());
        if signaled {
            fence.signal();
        }
        SyncObj { fence }
    }

    pub fn signal(&self) {
        self.fence.signal();
    }

    pub fn reset(&self) -> Self {
        Self::new(false)
    }

    pub fn is_signaled(&self) -> bool {
        self.fence.is_signaled()
    }
}

static NEXT_SYNCOBJ_ID: AtomicU32 = AtomicU32::new(1);

pub static SYNCOBJS: SpinLock<BTreeMap<u32, Arc<SpinLock<SyncObj>>>> =
    SpinLock::new(BTreeMap::new());

pub fn alloc_id() -> u32 {
    // ponytail: linear probe for a free id; registry is tiny (compositor
    // holds a handful). Wraps safely on overflow via compare_exchange loop.
    loop {
        let id = NEXT_SYNCOBJ_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 && !SYNCOBJS.lock().contains_key(&id) {
            return id;
        }
    }
}
