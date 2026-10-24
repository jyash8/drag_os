//! Work queue: deferred task execution for GPU driver bottom-halves.
//! Mirrors Linux workqueue (include/linux/workqueue.h).

#![allow(unused)]

use alloc::{boxed::Box, collections::VecDeque};
use crate::arch::sync::spinlock::SpinLock;

pub type WorkFn = Box<dyn FnOnce() + Send + 'static>;

/// A work item that can be queued for deferred execution.
pub struct Work {
    pub func: Option<WorkFn>,
}

impl Work {
    pub fn new(f: impl FnOnce() + Send + 'static) -> Self {
        Self {
            func: Some(Box::new(f)),
        }
    }
}

/// A simple FIFO work queue.
pub struct WorkQueue {
    pending: SpinLock<VecDeque<Work>>,
}

impl WorkQueue {
    pub const fn new() -> Self {
        Self {
            pending: SpinLock::new(VecDeque::new()),
        }
    }

    /// Queue a work item for deferred execution.
    pub fn queue_work(&self, work: Work) {
        self.pending.lock().push_back(work);
    }

    /// Drain and execute all pending work items synchronously.
    /// Called from a kernel worker thread context.
    pub fn flush(&self) {
        loop {
            let item = self.pending.lock().pop_front();
            match item {
                Some(mut w) => {
                    if let Some(f) = w.func.take() {
                        f();
                    }
                }
                None => break,
            }
        }
    }
}

impl Default for WorkQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// The kernel-wide DRM work queue for GPU driver deferred tasks.
pub static DRM_WORKQUEUE: WorkQueue = WorkQueue::new();
