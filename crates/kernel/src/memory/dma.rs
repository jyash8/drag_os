//! Scatter-Gather DMA memory manager and DMA-BUF handle sharing.
//!
//! Provides:
//! - `SgTable`: non-contiguous physical page lists for GPU buffer objects
//! - `dma_alloc_coherent`: physically contiguous DMA allocation  
//! - `DmaBufHandle`: prime buffer sharing between userspace processes

#![allow(unused)]

use alloc::{vec, vec::Vec};
use shared::memory::address::{PAGE_SIZE, PhysAddr};
use shared::memory::allocator::FrameAllocator;
use crate::memory::FRAME_ALLOCATOR;

/// One physically-contiguous segment in a scatter-gather list.
#[derive(Debug, Clone, Copy)]
pub struct SgEntry {
    /// Physical base address of this segment.
    pub phys: PhysAddr,
    /// Length in bytes (always a multiple of PAGE_SIZE).
    pub len: usize,
}

impl SgEntry {
    pub fn new(phys: PhysAddr, len: usize) -> Self {
        Self { phys, len }
    }
    /// Bus (DMA) address for this segment. On x86-64 without IOMMU this is
    /// identical to the physical address.
    pub fn dma_addr(&self) -> u64 {
        self.phys.as_u64()
    }
}

/// Scatter-gather table: a list of physically non-contiguous pages that
/// together form a single logical GPU buffer object (texture, vertex, etc.).
#[derive(Debug)]
pub struct SgTable {
    pub entries: Vec<SgEntry>,
    pub total_bytes: usize,
}

impl SgTable {
    /// Allocate `bytes` of memory as a scatter-gather table.
    /// Each entry is one PAGE_SIZE segment.
    pub fn alloc(bytes: usize) -> Option<Self> {
        let pages = bytes.div_ceil(PAGE_SIZE as usize);
        let mut entries = Vec::with_capacity(pages);
        let mut alloc = FRAME_ALLOCATOR.lock();
        for _ in 0..pages {
            let phys = alloc.alloc_pages(1, PAGE_SIZE).ok()?;
            entries.push(SgEntry::new(phys, PAGE_SIZE as usize));
        }
        Some(Self { entries, total_bytes: pages * PAGE_SIZE as usize })
    }

    /// Total number of bytes covered by this scatter-gather table.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Iterator over (bus_address, length) pairs for DMA engine programming.
    pub fn iter(&self) -> impl Iterator<Item = (u64, usize)> + '_ {
        self.entries.iter().map(|e| (e.dma_addr(), e.len))
    }
}

/// Allocate `bytes` of physically-contiguous DMA-coherent memory.
/// Returns the physical address of the allocation.
///
/// Used for command ring buffers and descriptor lists that must be
/// physically contiguous for the GPU's address decoder.
pub fn dma_alloc_coherent(bytes: usize) -> Option<PhysAddr> {
    let pages = bytes.div_ceil(PAGE_SIZE as usize);
    FRAME_ALLOCATOR.lock().alloc_pages(pages, PAGE_SIZE).ok()
}

/// A DMA-BUF handle for prime buffer sharing between processes.
/// Wraps a scatter-gather table with a unique file descriptor handle.
#[derive(Debug)]
pub struct DmaBufHandle {
    pub id: u32,
    pub sg: SgTable,
}

impl DmaBufHandle {
    pub fn new(id: u32, bytes: usize) -> Option<Self> {
        Some(Self { id, sg: SgTable::alloc(bytes)? })
    }
}
