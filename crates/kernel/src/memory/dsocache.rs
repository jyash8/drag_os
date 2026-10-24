//! Kernel-global cache of shared-library / file-backed pages.
//!
//! One physical frame per (inode, page_offset). PTEs pointing at a cached
//! frame carry `PageTableFlags::FILE_CACHE`; the frame is refcounted here
//! rather than freed by the COW accounting in `space.rs`.
//!
//! ponytail: no eviction. Frames are held for the kernel's lifetime and
//! recycled only when refs drops to 0 on last unmap. Add an LRU/aging policy
//! if/when kernel memory pressure is a real problem.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use crate::arch::sync::spinlock::SpinLock;
use shared::memory::address::PhysAddr;

pub struct CachedPage {
    pub frame: PhysAddr,
    pub refs: u32,
    pub access_count: u64,     // incremented on every lookup/retain; used for LRU eviction
    pub dirty: bool,
    pub node: Option<alloc::sync::Arc<dyn crate::fs::vfs::FileNode>>,
}

pub static DSO_CACHE: SpinLock<BTreeMap<(u64, u64), CachedPage>> = SpinLock::new(BTreeMap::new());

/// Reverse map: frame number (phys >> 12) -> cache key (inode, page_offset).
/// Lets destroy/unmap resolve the cache key from a frame alone when the
/// covering process/VMA is already gone.
pub static FRAME_TO_KEY: SpinLock<BTreeMap<u64, (u64, u64)>> = SpinLock::new(BTreeMap::new());

/// Temporary observability counter for the DSO cache; removed in Task 10.
pub static DSO_HITS: SpinLock<u64> = SpinLock::new(0);

/// Returns the cached frame for `(inode, page_offset)` and takes a reference
/// to it, or None on miss.
///
/// There is deliberately no plain `lookup`: a caller that looks a page up and
/// then takes its reference in a second call leaves a window in which the
/// entry can go away.
///
/// The lookup and the reference have to happen under one lock. Split apart,
/// two things can happen in the window: a concurrent `release` can drop the
/// last reference, remove the entry and hand the frame back to the allocator;
/// or an OOM `evict_lru` can do the same. The caller then re-inserts and maps
/// a frame that has already been reused, and the page silently becomes
/// somebody else's data. That is the shape of the intermittent loader crash:
/// a library's dynamic section read as garbage, so the loader dereferences a
/// null pointer.
pub fn lookup_and_retain(inode: u64, page_offset: u64) -> Option<PhysAddr> {
    let mut cache = DSO_CACHE.lock();
    match cache.get_mut(&(inode, page_offset)) {
        Some(c) => {
            c.refs += 1;
            c.access_count = c.access_count.saturating_add(1);
            *DSO_HITS.lock() += 1;
            Some(c.frame)
        }
        None => None,
    }
}

/// Inserts a freshly-read frame, or bumps refs if already present.
pub fn retain(inode: u64, page_offset: u64, frame: PhysAddr, node: Option<alloc::sync::Arc<dyn crate::fs::vfs::FileNode>>) -> PhysAddr {
    let mut cache = DSO_CACHE.lock();
    if let Some(entry) = cache.get_mut(&(inode, page_offset)) {
        entry.refs += 1;
        entry.access_count = entry.access_count.saturating_add(1);
        entry.frame
    } else {
        cache.insert(
            (inode, page_offset),
            CachedPage { frame, refs: 1, access_count: 1, dirty: false, node },
        );
        FRAME_TO_KEY.lock().insert(frame.0 >> 12, (inode, page_offset));
        frame
    }
}

/// Bumps the refcount of the cached page whose frame is `frame` (frame number,
/// i.e. physical address >> 12). Used by fork to keep shared pages shared.
pub fn retain_from_frame(frame: u64) {
    let mut cache = DSO_CACHE.lock();
    for entry in cache.values_mut() {
        if entry.frame.0 >> 12 == frame {
            entry.refs += 1;
            return;
        }
    }
}

/// Drops one reference. Returns `Some(frame)` (and the caller must dealloc it)
/// when refs reaches 0 and the entry is being removed. Dirty pages are assumed
/// already written back by the caller before calling this.
pub fn release(inode: u64, page_offset: u64) -> Option<PhysAddr> {
    let mut cache = DSO_CACHE.lock();
    if let Some(entry) = cache.get_mut(&(inode, page_offset)) {
        entry.refs = entry.refs.saturating_sub(1);
        if entry.refs == 0 {
            let frame = entry.frame;
            cache.remove(&(inode, page_offset));
            FRAME_TO_KEY.lock().remove(&(frame.0 >> 12));
            return Some(frame);
        }
    }
    None
}

/// Returns the number of entries currently in the DSO cache.
pub fn cache_len() -> usize {
    DSO_CACHE.lock().len()
}

/// Evicts up to `n` cache entries that have `refs == 0` (not currently mapped
/// by any process), choosing the least-recently-accessed first (LRU by
/// `access_count`). Returns the evicted frames; the caller must dealloc them
/// with `FRAME_ALLOCATOR`. Entries with `refs > 0` are skipped — they are
/// actively mapped and must not be reclaimed.
pub fn evict_lru(n: usize) -> Vec<PhysAddr> {
    if n == 0 {
        return Vec::new();
    }
    let mut cache = DSO_CACHE.lock();
    // Collect (key, access_count) for all entries with refs == 0.
    let candidates: Vec<((u64, u64), u64)> = cache
        .iter()
        .filter(|(_, e)| e.refs == 0)
        .map(|(k, e)| ((k.0, k.1), e.access_count))
        .collect();
    // Sort ascending by access_count (LRU first), take `n`.
    let mut candidates: Vec<_> = candidates;
    candidates.sort_by_key(|(_, ac)| *ac);
    let to_evict: Vec<(u64, u64)> = candidates
        .into_iter()
        .take(n)
        .map(|(k, _)| k)
        .collect();
    let mut evicted: Vec<PhysAddr> = Vec::new();
    for (inode, page_offset) in to_evict {
        if let Some(entry) = cache.get(&(inode, page_offset)) {
            // Re-check refs == 0 under the lock (it hasn't changed since we hold
            // the lock the whole time).
            if entry.refs == 0 {
                let frame = entry.frame;
                cache.remove(&(inode, page_offset));
                FRAME_TO_KEY.lock().remove(&(frame.0 >> 12));
                evicted.push(frame);
            }
        }
    }
    evicted
}