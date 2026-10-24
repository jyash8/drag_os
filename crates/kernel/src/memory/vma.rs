use alloc::{sync::Arc, vec::Vec};
use crate::arch::sync::spinlock::SpinLock;
use crate::fs::vfs::OpenFile;

pub const PROT_READ: u64 = 1;
pub const PROT_WRITE: u64 = 2;
pub const PROT_EXEC: u64 = 4;
pub const MAP_SHARED: u64 = 0x01;
pub const MAP_PRIVATE: u64 = 0x02;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_DEVICE: u64 = 0x40;

#[derive(Clone)]
pub enum VmaKind {
    Anonymous,
    Heap,
    Stack,
    Device { phys: u64 },
    File { file: Arc<SpinLock<OpenFile>>, offset: u64 },
    /// Kernel-mapped file (e.g., dynamic interpreter) identified by inode.
    /// No fd needed; data already loaded by kernel. Used for DSO cache.
    MappedFile { inode: u64, offset: u64 },
}

impl core::fmt::Debug for VmaKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Anonymous => write!(f, "Anonymous"),
            Self::Heap => write!(f, "Heap"),
            Self::Stack => write!(f, "Stack"),
            Self::Device { phys } => f.debug_struct("Device").field("phys", phys).finish(),
            Self::File { offset, .. } => f.debug_struct("File").field("offset", offset).finish(),
            Self::MappedFile { inode, offset } => f.debug_struct("MappedFile").field("inode", inode).field("offset", offset).finish(),
        }
    }
}

impl PartialEq for VmaKind {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (VmaKind::Anonymous, VmaKind::Anonymous) => true,
            (VmaKind::Heap, VmaKind::Heap) => true,
            (VmaKind::Stack, VmaKind::Stack) => true,
            (VmaKind::Device { phys: p1 }, VmaKind::Device { phys: p2 }) => p1 == p2,
            (VmaKind::File { file: f1, offset: o1 }, VmaKind::File { file: f2, offset: o2 }) => {
                Arc::ptr_eq(f1, f2) && o1 == o2
            }
            (VmaKind::MappedFile { inode: i1, offset: o1 }, VmaKind::MappedFile { inode: i2, offset: o2 }) => {
                i1 == i2 && o1 == o2
            }
            _ => false,
        }
    }
}
impl Eq for VmaKind {}

#[derive(Debug, Clone)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: u64,
    pub flags: u64,
    pub kind: VmaKind,
}

impl Vma {
    pub fn contains(&self, addr: u64) -> bool {
        (self.start..self.end).contains(&addr)
    }
}

#[derive(Debug, Default, Clone)]
pub struct VmaList {
    pub vmas: Vec<Vma>,
}

impl VmaList {
    pub fn has_heap(&self) -> bool {
        self.vmas.iter().any(|v| v.kind == VmaKind::Heap)
    }

    pub fn heap_mut(&mut self) -> Option<&mut Vma> {
        self.vmas.iter_mut().find(|v| v.kind == VmaKind::Heap)
    }

    pub fn find(&self, addr: u64) -> Option<&Vma> {
        self.vmas.iter().find(|v| v.contains(addr))
    }

    pub fn insert(&mut self, vma: Vma) -> Result<(), ()> {
        if vma.start >= vma.end {
            return Err(());
        }

        if self
            .vmas
            .iter()
            .any(|v| v.start < vma.end && vma.start < v.end)
        {
            return Err(());
        }

        let pos = self
            .vmas
            .iter()
            .position(|v| v.start > vma.start)
            .unwrap_or(self.vmas.len());
        self.vmas.insert(pos, vma);
        Ok(())
    }

    pub fn mprotect_range(&mut self, start: u64, end: u64, prot: u64) -> bool {
        fn push_merged(vmas: &mut Vec<Vma>, v: Vma) {
            if let Some(last) = vmas.last_mut() {
                if last.kind == v.kind
                    && last.prot == v.prot
                    && last.flags == v.flags
                    && last.end == v.start
                {
                    last.end = v.end;
                    return;
                }
            }
            vmas.push(v);
        }

        let mut new_vmas = Vec::with_capacity(self.vmas.len() + 2);
        let mut changed = false;
        for v in self.vmas.drain(..) {
            if v.end <= start || v.start >= end {
                push_merged(&mut new_vmas, v);
                continue;
            }
            let o_start = v.start.max(start);
            let o_end = v.end.min(end);
            if o_start <= v.start && v.end <= o_end {
                push_merged(&mut new_vmas, Vma { prot, ..v });
            } else {
                if v.start < o_start {
                    push_merged(&mut new_vmas, Vma { end: o_start, ..v.clone() });
                }
                push_merged(&mut new_vmas, Vma { start: o_start, end: o_end, prot, ..v.clone() });
                if v.end > o_end {
                    push_merged(&mut new_vmas, Vma { start: o_end, ..v });
                }
            }
            changed = true;
        }
        self.vmas = new_vmas;
        changed
    }

    pub fn remove_range(&mut self, start: u64, end: u64) -> Vec<Vma> {
        let mut removed = Vec::new();
        let mut keep = Vec::with_capacity(self.vmas.len());
        for v in self.vmas.drain(..) {
            if v.end <= start || v.start >= end {
                keep.push(v);
            } else if start <= v.start && v.end <= end {
                removed.push(v);
            } else {
                if v.start < start {
                    keep.push(Vma { end: start, ..v.clone() });
                }
                if v.end > end {
                    let offset_delta = end - v.start;
                    let kind = match &v.kind {
                        VmaKind::File { file, offset } => VmaKind::File {
                            file: file.clone(),
                            offset: offset.saturating_add(offset_delta),
                        },
                        VmaKind::MappedFile { inode, offset } => VmaKind::MappedFile {
                            inode: *inode,
                            offset: offset.saturating_add(offset_delta),
                        },
                        VmaKind::Device { phys } => VmaKind::Device {
                            phys: phys.saturating_add(offset_delta),
                        },
                        other => other.clone(),
                    };
                    keep.push(Vma {
                        start: end,
                        kind,
                        ..v.clone()
                    });
                }
                removed.push(Vma {
                    start: v.start.max(start),
                    end: v.end.min(end),
                    ..v
                });
            }
        }
        self.vmas = keep;
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn anon(start: u64, end: u64) -> Vma {
        Vma {
            start,
            end,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_PRIVATE,
            kind: VmaKind::Anonymous,
        }
    }

    #[test_case]
    fn insert_rejects_inverted() {
        let mut l = VmaList::default();
        assert!(l.insert(anon(0x2000, 0x1000)).is_err());
    }

    #[test_case]
    fn insert_rejects_overlap() {
        let mut l = VmaList::default();
        l.insert(anon(0x1000, 0x3000)).unwrap();
        assert!(l.insert(anon(0x2000, 0x4000)).is_err());
        assert!(l.insert(anon(0x0, 0x2000)).is_err());
        // Adjacent, non-overlapping is fine.
        assert!(l.insert(anon(0x3000, 0x4000)).is_ok());
    }

    #[test_case]
    fn insert_sorts_by_start() {
        let mut l = VmaList::default();
        l.insert(anon(0x8000, 0x9000)).unwrap();
        l.insert(anon(0x1000, 0x2000)).unwrap();
        l.insert(anon(0x4000, 0x5000)).unwrap();
        assert_eq!(l.vmas.iter().map(|v| v.start).collect::<Vec<_>>(),
                   vec![0x1000, 0x4000, 0x8000]);
    }

    #[test_case]
    fn contains_and_find() {
        let mut l = VmaList::default();
        l.insert(anon(0x1000, 0x5000)).unwrap();
        assert!(l.find(0x1000).is_some());
        assert!(l.find(0x4fff).is_some());
        assert!(l.find(0x5000).is_none(), "end-exclusive");
        // contains() is half-open too
        let v = l.vmas[0].clone();
        assert!(v.contains(0x1000) && !v.contains(0x5000));
    }

    #[test_case]
    fn has_heap_after_heap_insert() {
        let mut l = VmaList::default();
        assert!(!l.has_heap());
        l.insert(Vma { start: 0x1000, end: 0x2000, prot: 0, flags: 0, kind: VmaKind::Heap }).unwrap();
        assert!(l.has_heap());
        assert_eq!(l.heap_mut().unwrap().start, 0x1000);
    }

    #[test_case]
    fn mprotect_partial_split() {
        let mut l = VmaList::default();
        l.insert(anon(0x1000, 0x10000)).unwrap();
        assert!(l.mprotect_range(0x4000, 0x8000, PROT_READ));
        assert_eq!(l.vmas.len(), 3);
        assert_eq!(l.vmas[0].start, 0x1000); assert_eq!(l.vmas[0].end, 0x4000);
        assert_eq!(l.vmas[1].start, 0x4000); assert_eq!(l.vmas[1].end, 0x8000);
        assert_eq!(l.vmas[1].prot, PROT_READ);
        assert_eq!(l.vmas[2].start, 0x8000); assert_eq!(l.vmas[2].end, 0x10000);
    }

    #[test_case]
    fn mprotect_merges_adjacent_same_prot() {
        let mut l = VmaList::default();
        l.insert(anon(0x1000, 0x2000)).unwrap();
        l.insert(anon(0x2000, 0x4000)).unwrap();
        l.insert(anon(0x4000, 0x5000)).unwrap();
        assert_eq!(l.vmas.len(), 3, "all inserted separately");
        // Same prot across adjacent VMAs coalesces into a single merged region.
        l.mprotect_range(0x1000, 0x5000, PROT_READ | PROT_WRITE);
        assert_eq!(l.vmas.len(), 1);
        assert_eq!(l.vmas[0].start, 0x1000);
        assert_eq!(l.vmas[0].end, 0x5000);
    }

    #[test_case]
    fn remove_middle_splits() {
        let mut l = VmaList::default();
        l.insert(anon(0x1000, 0x10000)).unwrap();
        let removed = l.remove_range(0x3000, 0x4000);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].start, 0x3000);
        assert_eq!(removed[0].end, 0x4000);
        assert_eq!(l.vmas.len(), 2);
        assert_eq!(l.vmas[0].end, 0x3000);
        assert_eq!(l.vmas[1].start, 0x4000);
        assert!(l.find(0x3500).is_none());
        assert!(l.find(0x2fff).is_some());
    }

    #[test_case]
    fn remove_whole_vma() {
        let mut l = VmaList::default();
        l.insert(anon(0x1000, 0x2000)).unwrap();
        let removed = l.remove_range(0x1000, 0x2000);
        assert_eq!(removed.len(), 1);
        assert!(l.vmas.is_empty());
    }

    #[test_case]
    fn remove_noop_outside() {
        let mut l = VmaList::default();
        l.insert(anon(0x1000, 0x2000)).unwrap();
        assert!(l.remove_range(0x3000, 0x4000).is_empty());
        assert_eq!(l.vmas.len(), 1);
    }
}
