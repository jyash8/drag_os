//! DMA fence: asynchronous GPU command completion tracking.
//! Mirrors Linux `struct dma_fence` (include/linux/dma-fence.h).

#![allow(unused)]

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::arch::sync::spinlock::SpinLock;

static FENCE_SEQNO: AtomicU64 = AtomicU64::new(1);

pub type FenceCallback = Box<dyn Fn() + Send + Sync>;

/// Tracks completion of a single GPU command sequence.
pub struct DmaFence {
    pub seqno: u64,
    signaled: AtomicBool,
    callbacks: SpinLock<Vec<FenceCallback>>,
}

impl DmaFence {
    /// Create a new unsignaled fence.
    pub fn new() -> Self {
        Self {
            seqno: FENCE_SEQNO.fetch_add(1, Ordering::Relaxed),
            signaled: AtomicBool::new(false),
            callbacks: SpinLock::new(Vec::new()),
        }
    }

    /// Signal the fence, running all registered callbacks.
    pub fn signal(&self) {
        self.signaled.store(true, Ordering::Release);
        let callbacks = core::mem::take(&mut *self.callbacks.lock());
        for cb in callbacks {
            cb();
        }
    }

    pub fn is_signaled(&self) -> bool {
        self.signaled.load(Ordering::Acquire)
    }

    /// Register a callback to fire when this fence is signaled.
    pub fn add_callback(&self, cb: FenceCallback) {
        if self.is_signaled() {
            cb();
        } else {
            self.callbacks.lock().push(cb);
        }
    }

    /// Busy-wait until signaled (used only during init/flush).
    pub fn wait(&self) {
        while !self.is_signaled() {
            core::hint::spin_loop();
        }
    }
}

impl Default for DmaFence {
    fn default() -> Self {
        Self::new()
    }
}

/// Sync file: exportable fence file descriptor handle.
/// Mirrors Linux `sync_file` (drivers/dma-buf/sync_file.c).
pub struct SyncFile {
    pub fence: Arc<DmaFence>,
    pub fd: i32,
}
