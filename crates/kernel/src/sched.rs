//! Completely Fair Scheduler (CFS) vruntime-based priority scheduling subsystem.

use alloc::collections::BinaryHeap;
use alloc::vec::Vec;
use core::cmp::Ordering as CmpOrdering;
use crate::tasks::Thread;

/// Linux CFS Weight Table for nice levels -20 .. +19.
/// Nice 0 has weight 1024 (baseline).
/// Negative nice (higher priority) gets exponentially higher weight (~1.25x per level).
/// Positive nice (lower priority) gets exponentially lower weight (/1.25 per level).
pub const WEIGHTS: [u64; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, // nice -20..-11
    9548, 7620, 6100, 4904, 3906, 3121, 2501, 1991, 1586, 1277,           // nice -10..-1
    1024, 820, 655, 526, 423, 335, 272, 215, 172, 137,                    // nice  0..9
    110, 87, 70, 56, 45, 36, 29, 23, 18, 15,                               // nice 10..19
];

/// Returns the scheduling weight for a given nice value in -20..=19.
pub fn weight(nice: i32) -> u64 {
    WEIGHTS[(nice.clamp(-20, 19) + 20) as usize]
}

/// Advance a thread's virtual runtime based on executed ticks and its nice weight.
/// Also accounts raw CPU ticks (user mode) for `times`/`getrusage`.
pub fn sched_account(thread: &mut Thread, ticks: u64) {
    let w = weight(thread.nice);
    let delta = (ticks.max(1) * 1024) / w;
    thread.vruntime = thread.vruntime.saturating_add(delta);
    thread.process.lock().utime += ticks;
}

/// An entry in the scheduler's runqueue, ordered such that the smallest `vruntime`
/// is popped first (min-heap via reversed `Ord`).
#[derive(Clone)]
pub struct RunQueueEntry {
    pub thread: Thread,
    pub vruntime: u64,
}

impl PartialEq for RunQueueEntry {
    fn eq(&self, o: &Self) -> bool {
        self.vruntime == o.vruntime && self.thread.id == o.thread.id
    }
}
impl Eq for RunQueueEntry {}

impl PartialOrd for RunQueueEntry {
    fn partial_cmp(&self, o: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(o))
    }
}

impl Ord for RunQueueEntry {
    // Reversed: smaller vruntime is greater in Ord so BinaryHeap pops smallest vruntime first.
    fn cmp(&self, o: &Self) -> CmpOrdering {
        o.vruntime
            .cmp(&self.vruntime)
            .then_with(|| o.thread.id.cmp(&self.thread.id))
    }
}

/// The CFS min-heap runqueue.
pub struct SchedRunQueue {
    heap: BinaryHeap<RunQueueEntry>,
    pub min_vruntime: u64,
}

impl SchedRunQueue {
    pub const fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            min_vruntime: 0,
        }
    }

    /// Enqueues a thread. Its vruntime is normalized against the queue's `min_vruntime`
    /// so newly created or migrated threads do not starve existing ones.
    pub fn enqueue(&mut self, thread: Thread) {
        let vruntime = thread.vruntime.max(self.min_vruntime);
        self.heap.push(RunQueueEntry { thread, vruntime });
        crate::smp::signal_runnable();
    }

    /// Enqueues a thread with an interactivity boost (for wakeups from I/O or sleep).
    pub fn enqueue_with_boost(&mut self, mut thread: Thread, boost_ticks: u64) {
        let boost = (boost_ticks * 1024) / weight(thread.nice);
        let boosted = self.min_vruntime.saturating_sub(boost);
        thread.vruntime = boosted;
        self.heap.push(RunQueueEntry { thread, vruntime: boosted });
        crate::smp::signal_runnable();
    }

    /// Picks the next thread with the smallest virtual runtime.
    ///
    /// Threads belonging to a stopped process (SIGTSTP/SIGSTOP, job control)
    /// are skipped: they are popped, held aside, and pushed back before
    /// returning, so a stopped process keeps its saved kernel-stack frame
    /// without ever being dispatched. Clearing `Process::stopped` (SIGCONT)
    /// is therefore all that is needed to make it runnable again -- there is
    /// no separate re-enqueue path to keep in sync.
    ///
    /// `min_vruntime` is advanced only for the entry actually returned. A
    /// skipped entry is not "run", so letting it raise the floor would hand
    /// free CPU time to everything behind it.
    pub fn pick_next(&mut self) -> Option<Thread> {
        let mut deferred: Vec<RunQueueEntry> = Vec::new();
        let picked = loop {
            let entry = match self.heap.peek() {
                Some(e) => e.clone(),
                None => break None,
            };
            self.heap.pop();
            if entry
                .thread
                .process
                .lock()
                .stopped
                .load(core::sync::atomic::Ordering::Acquire)
            {
                deferred.push(entry);
                continue;
            }
            self.min_vruntime = self.min_vruntime.max(entry.vruntime);
            break Some(entry.thread);
        };
        for e in deferred {
            self.heap.push(e);
        }
        picked
    }

    // Compatibility aliases for existing queue call sites
    pub fn pop_front(&mut self) -> Option<Thread> {
        self.pick_next()
    }

    pub fn push_back(&mut self, thread: Thread) {
        self.enqueue(thread);
    }

    pub fn push_front(&mut self, thread: Thread) {
        // High priority / immediate return: insert at current min_vruntime
        let vruntime = self.min_vruntime;
        self.heap.push(RunQueueEntry { thread, vruntime });
        crate::smp::signal_runnable();
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Borrowed thread iteration for diagnostics (NMI hang dump): no
    /// allocation and no locks beyond the caller's SCHEDULER guard.
    /// Order is heap-arbitrary — diagnostic only, not scheduling order.
    pub fn iter_threads(&self) -> impl Iterator<Item = &Thread> {
        self.heap.iter().map(|e| &e.thread)
    }

    /// Retains only the elements specified by the predicate.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&Thread) -> bool,
    {
        let entries: Vec<_> = self.heap.drain().filter(|e| f(&e.thread)).collect();
        self.heap = BinaryHeap::from(entries);
    }

    /// Returns a cloned snapshot vector of threads for diagnostic invariants.
    pub fn to_vec(&self) -> Vec<Thread> {
        self.heap.iter().map(|e| e.thread.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::Process;
    use crate::arch::sync::spinlock::SpinLock;
    use alloc::sync::Arc;
    use shared::memory::address::PhysAddr;

    fn mk_thread(id: u64, nice: i32, vruntime: u64) -> Thread {
        let proc = Arc::new(SpinLock::new(Process::new(id, 0, PhysAddr(0x1000), 128)));
        {
            let mut p = proc.lock();
            p.nice = nice;
        }
        Thread::new(id, proc, 0, 0, 0, 128, 0).with_vruntime(vruntime)
    }

    trait WithVruntime {
        fn with_vruntime(self, v: u64) -> Self;
    }
    impl WithVruntime for Thread {
        fn with_vruntime(mut self, v: u64) -> Self {
            self.vruntime = v;
            self
        }
    }

    #[test_case]
    fn test_weights_monotonic() {
        for n in -20..19 {
            assert!(weight(n) >= weight(n + 1), "weight must decrease with increasing nice");
        }
        assert_eq!(weight(0), 1024);
        assert_eq!(weight(-20), 88761);
        assert_eq!(weight(19), 15);
    }

    #[test_case]
    fn test_weights_clamped() {
        assert_eq!(weight(-99), weight(-20));
        assert_eq!(weight(99), weight(19));
    }

    #[test_case]
    fn test_runqueue_minheap_order() {
        let mut rq = SchedRunQueue::new();
        rq.enqueue(mk_thread(1, 0, 500));
        rq.enqueue(mk_thread(2, 0, 100));
        rq.enqueue(mk_thread(3, 0, 300));
        assert_eq!(rq.len(), 3);
        assert_eq!(rq.pick_next().unwrap().id, 2, "smallest vruntime pops first");
        assert_eq!(rq.pick_next().unwrap().id, 3);
        assert_eq!(rq.pick_next().unwrap().id, 1);
        assert!(rq.is_empty());
    }

    #[test_case]
    fn test_runqueue_min_vruntime_drips() {
        let mut rq = SchedRunQueue::new();
        rq.enqueue(mk_thread(1, 0, 0));
        rq.enqueue(mk_thread(2, 0, 200));
        rq.pick_next();
        assert_eq!(rq.min_vruntime, 0);
        rq.pick_next();
        assert_eq!(rq.min_vruntime, 200, "min_vruntime tracks highest vruntime picked");
    }

    #[test_case]
    fn test_enqueue_normalizes_against_min() {
        let mut rq = SchedRunQueue::new();
        rq.enqueue(mk_thread(1, 0, 1000));
        assert_eq!(rq.pick_next().unwrap().id, 1);
        // A new thread with vruntime=0 is promoted to min_vruntime for heap order,
        // so it pops ahead of an existing thread parked at min_vruntime.
        rq.enqueue(mk_thread(2, 0, 0));
        assert_eq!(rq.pick_next().unwrap().id, 2, "fresh thread picks next");
        assert_eq!(rq.min_vruntime, 1000, "min_vruntime stays pinned");
    }

    #[test_case]
    fn test_sched_account_utime() {
        let mut t = mk_thread(3, 0, 0);
        sched_account(&mut t, 10);
        assert_eq!(t.process.lock().utime, 10);
        assert_eq!(t.vruntime, (10 * 1024) / 1024, "nice=0: delta == ticks");
    }

    #[test_case]
    fn test_sched_account_nice_weight() {
        // nice=19 (weight 15): many more vruntime ticks for the same CPU time.
        let mut low = mk_thread(1, 19, 0);
        sched_account(&mut low, 10);
        assert_eq!(low.vruntime, (10 * 1024) / 15);

        // nice=-20 (weight 88761): very few vruntime ticks.
        let mut high = mk_thread(2, -20, 0);
        sched_account(&mut high, 10);
        assert_eq!(high.vruntime, (10 * 1024) / 88761);
    }

    #[test_case]
    fn test_runqueue_entry_break_ties_by_id() {
        let a = RunQueueEntry { thread: mk_thread(5, 0, 42), vruntime: 42 };
        let b = RunQueueEntry { thread: mk_thread(7, 0, 42), vruntime: 42 };
        let mut heap = BinaryHeap::new();
        heap.push(a.clone());
        heap.push(b);
        // Same vruntime: smaller thread id pops first.
        assert_eq!(heap.pop().unwrap().thread.id, 5);
        assert_eq!(heap.pop().unwrap().thread.id, 7);
    }

    #[test_case]
    fn test_runqueue_entry_eq() {
        let a = RunQueueEntry { thread: mk_thread(1, 0, 42), vruntime: 42 };
        let b = RunQueueEntry { thread: mk_thread(1, 0, 42), vruntime: 42 };
        let c = RunQueueEntry { thread: mk_thread(2, 0, 42), vruntime: 42 };
        assert!(a == b, "same id + vruntime must be equal");
        assert!(a != c, "different id must not be equal");
    }
}
