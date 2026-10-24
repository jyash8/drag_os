use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, NonNull};
use linked_list_allocator::Heap;
use crate::arch::sync::spinlock::SpinLock;

const BLOCK_SIZES: &[usize] = &[16, 32, 64, 128, 256, 512, 1024, 2048];

struct ListNode {
    next: Option<&'static mut ListNode>,
}

pub struct FixedSizeBlockAllocator {
    list_heads: [Option<&'static mut ListNode>; BLOCK_SIZES.len()],
    fallback_allocator: Heap,
}

impl FixedSizeBlockAllocator {
    pub const fn new() -> Self {
        const EMPTY: Option<&'static mut ListNode> = None;
        FixedSizeBlockAllocator {
            list_heads: [EMPTY; BLOCK_SIZES.len()],
            fallback_allocator: Heap::empty(),
        }
    }

    pub unsafe fn init(&mut self, heap_start: *mut u8, heap_size: usize) {
        unsafe {
            self.fallback_allocator.init(heap_start, heap_size);
        }
    }

    fn fallback_alloc(&mut self, layout: Layout) -> *mut u8 {
        match self.fallback_allocator.allocate_first_fit(layout) {
            Ok(ptr) => ptr.as_ptr(),
            Err(_) => ptr::null_mut(),
        }
    }

    fn fallback_dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        unsafe {
            self.fallback_allocator
                .deallocate(NonNull::new(ptr).unwrap(), layout);
        }
    }
}

fn list_index(layout: &Layout) -> Option<usize> {
    let required_block_size = layout.size().max(layout.align());
    BLOCK_SIZES.iter().position(|&s| s >= required_block_size)
}

/// Live heap bytes (for leak-vs-fragmentation triage; printed periodically).
static HEAP_USED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn heap_used() -> u64 {
    HEAP_USED.load(core::sync::atomic::Ordering::Relaxed)
}

unsafe impl GlobalAlloc for SpinLock<FixedSizeBlockAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Big-alloc tracer: multi-MB heap users (exec file images, etc.).
        // Rare path only; helps attribute OOMs like "3931200 bytes failed".
        if layout.size() >= 1024 * 1024 {
            crate::dprintln!("[heap-big] alloc {:#x} bytes", layout.size());
        }
        let mut allocator = self.lock();
        let ptr = match list_index(&layout) {
            Some(index) => {
                match allocator.list_heads[index].take() {
                    Some(node) => {
                        allocator.list_heads[index] = node.next.take();
                        node as *mut ListNode as *mut u8
                    }
                    None => {
                        let block_size = BLOCK_SIZES[index];
                        // Allocate a chunk from the fallback allocator, at least 4096 bytes or the block size
                        let alloc_size = if block_size > 4096 { block_size } else { 4096 };
                        let alloc_align = alloc_size;
                        let chunk_layout = Layout::from_size_align(alloc_size, alloc_align).unwrap();
                        let chunk_ptr = allocator.fallback_alloc(chunk_layout);
                        
                        if chunk_ptr.is_null() {
                            return ptr::null_mut();
                        }
                        
                        // Slice the chunk into blocks of `block_size` and add them to the free list
                        let num_blocks = alloc_size / block_size;
                        
                        // We will return the first block, and add the rest to the free list
                        let mut current_node = None;
                        
                        // Go backwards so the list is built in order (though order doesn't matter)
                        for i in (1..num_blocks).rev() {
                            let block_ptr = unsafe { chunk_ptr.add(i * block_size) };
                            let new_node = ListNode {
                                next: current_node,
                            };
                            let new_node_ptr = block_ptr as *mut ListNode;
                            unsafe {
                                new_node_ptr.write(new_node);
                            }
                            current_node = Some(unsafe { &mut *new_node_ptr });
                        }
                        
                        allocator.list_heads[index] = current_node;
                        chunk_ptr
                    }
                }
            }
            None => allocator.fallback_alloc(layout),
        };
        if !ptr.is_null() {
            HEAP_USED.fetch_add(layout.size() as u64, core::sync::atomic::Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        HEAP_USED.fetch_sub(layout.size() as u64, core::sync::atomic::Ordering::Relaxed);
        let mut allocator = self.lock();
        match list_index(&layout) {
            Some(index) => {
                let new_node = ListNode {
                    next: allocator.list_heads[index].take(),
                };
                // Verify that block has size and alignment required for storing node
                assert!(core::mem::size_of::<ListNode>() <= BLOCK_SIZES[index]);
                assert!(core::mem::align_of::<ListNode>() <= BLOCK_SIZES[index]);
                let new_node_ptr = ptr as *mut ListNode;
                unsafe {
                    new_node_ptr.write(new_node);
                    allocator.list_heads[index] = Some(&mut *new_node_ptr);
                }
            }
            None => allocator.fallback_dealloc(ptr, layout),
        }
    }
}
