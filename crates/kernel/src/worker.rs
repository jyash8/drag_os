//! Kernel Worker Subsystem
//!
//! Provides an asynchronous, deferred execution work queue for kernel-level tasks,
//! background writeback, deferred device cleanups, and driver bottom-half handlers.

use alloc::{boxed::Box, collections::VecDeque};
use crate::arch::sync::spinlock::SpinLock;

type WorkFn = Box<dyn FnOnce() + Send + 'static>;

/// Global FIFO work queue for background kernel tasks.
static WORK_QUEUE: SpinLock<VecDeque<WorkFn>> = SpinLock::new(VecDeque::new());

/// Schedules a unit of work to be executed asynchronously by the kernel worker system.
pub fn schedule<F: FnOnce() + Send + 'static>(work: F) {
    let mut queue = WORK_QUEUE.lock();
    queue.push_back(Box::new(work));
    crate::smp::signal_runnable();
}

/// Drains and runs pending work items.
/// Returns the number of work items executed.
pub fn run_worker_tasks() -> usize {
    let mut count = 0;
    loop {
        let work = {
            let mut queue = WORK_QUEUE.lock();
            queue.pop_front()
        };
        match work {
            Some(task) => {
                task();
                count += 1;
            }
            None => break,
        }
    }
    count
}

/// Returns true if there are pending work items waiting to be processed.
pub fn has_pending_work() -> bool {
    let queue = WORK_QUEUE.lock();
    !queue.is_empty()
}

/// Initializes the kernel worker subsystem.
pub fn init() {
    crate::tprintln!("Worker subsystem initialized.");
}
