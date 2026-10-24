// ext4 extent tree walker
//
// The extent tree is a B-tree stored in the inode's `block[0..14]` field (60
// bytes). The root node contains an `ExtentHeader` followed by either
// `ExtentIdx` (internal) or `Extent` (leaf) entries.
//
// For files larger than 4 extents the tree spills into separate blocks
// pointed to by `ExtentIdx` entries. Walking the tree is required to
// translate a logical block number to a physical block number.

#![allow(unused)]

use alloc::vec::Vec;
use super::structs::*;

/// Result of looking up a logical block in the extent tree.
#[derive(Debug, Clone, Copy)]
pub enum ExtentLookup {
    /// The logical block is mapped to this physical block.
    Mapped { phys_block: u64, len: u32 },
    /// The logical block falls in a hole (sparse file).
    Hole { len: u32 },
}

/// Read an extent header from a raw byte slice at the given offset.
///
/// # Safety
/// The caller must ensure `data[offset..offset+12]` is valid.
pub fn read_header(data: &[u8], offset: usize) -> ExtentHeader {
    unsafe { core::ptr::read_unaligned(data[offset..].as_ptr() as *const ExtentHeader) }
}

/// Read an extent index entry from raw bytes.
pub fn read_idx(data: &[u8], offset: usize) -> ExtentIdx {
    unsafe { core::ptr::read_unaligned(data[offset..].as_ptr() as *const ExtentIdx) }
}

/// Read a leaf extent entry from raw bytes.
pub fn read_extent(data: &[u8], offset: usize) -> Extent {
    unsafe { core::ptr::read_unaligned(data[offset..].as_ptr() as *const Extent) }
}

pub const HEADER_SIZE: usize = core::mem::size_of::<ExtentHeader>();
pub const ENTRY_SIZE: usize = 12; // both ExtentIdx and Extent are 12 bytes

/// Walk the extent tree rooted in `inode_block_data` (the 60-byte `block`
/// field of the inode) to find the physical block for `logical_block`.
///
/// `read_block_fn` is called when the tree depth > 0 and we need to read
/// an interior/leaf node from disk.
///
/// Returns `None` if the extent tree is corrupt.
pub fn lookup_extent<F>(
    inode_block_data: &[u8; 60],
    logical_block: u64,
    mut read_block_fn: F,
) -> Option<ExtentLookup>
where
    F: FnMut(u64) -> Result<alloc::vec::Vec<u8>, ()>,
{
    let hdr = read_header(inode_block_data, 0);
    if hdr.magic != EXT4_EXT_MAGIC || hdr.entries > hdr.max {
        return None;
    }

    // Start at the root node embedded in the inode.
    let mut depth = hdr.depth;
    let mut node_data: Vec<u8> = inode_block_data.to_vec();

    loop {
        let hdr = read_header(&node_data, 0);
        if hdr.magic != EXT4_EXT_MAGIC || hdr.entries > hdr.max {
            return None;
        }
        let entries = hdr.entries as usize;

        if depth == 0 {
            // Leaf level: search for the extent covering `logical_block`.
            for i in 0..entries {
                let ext = read_extent(&node_data, HEADER_SIZE + i * ENTRY_SIZE);
                let start = ext.block as u64;
                let count = ext.block_count() as u64;
                if logical_block >= start && logical_block < start + count {
                    let offset = logical_block - start;
                    return Some(ExtentLookup::Mapped {
                        phys_block: ext.start() + offset,
                        len: (count - offset) as u32,
                    });
                }
            }
            // Not found — this block is in a hole.
            return Some(ExtentLookup::Hole { len: 1 });
        }

        // Internal level: binary-search for the child covering `logical_block`.
        let mut child_phys: Option<u64> = None;
        for i in (0..entries).rev() {
            let idx = read_idx(&node_data, HEADER_SIZE + i * ENTRY_SIZE);
            if (logical_block as u32) >= idx.block {
                child_phys = Some(idx.leaf());
                break;
            }
        }
        
        let child = match child_phys {
            Some(c) => c,
            None => return Some(ExtentLookup::Hole { len: 1 }), // It's a hole!
        };

        node_data = read_block_fn(child).ok()?;
        depth -= 1;
    }
}

/// Simplified extent insertion.
/// Appends or merges a block into the extent tree.
/// Only supports appending to the current leaf node. Fails with Err(()) if node splitting is required.
pub fn insert_extent<F, W, A>(
    inode_block_data: &mut [u8; 60],
    logical_block: u64,
    physical_block: u64,
    block_size: usize,
    mut read_block_fn: F,
    mut write_block_fn: W,
    mut alloc_block_fn: A,
) -> Result<(), ()>
where
    F: FnMut(u64) -> Result<alloc::vec::Vec<u8>, ()>,
    W: FnMut(u64, &[u8]) -> Result<(), ()>,
    A: FnMut() -> Result<u64, ()>,
{
    let mut hdr = read_header(inode_block_data, 0);
    if hdr.magic != EXT4_EXT_MAGIC || hdr.entries > hdr.max {
        return Err(()); // Corrupt or uninitialized
    }

    let mut depth = hdr.depth;
    let mut node_data: Vec<u8> = inode_block_data.to_vec();
    let mut path = alloc::vec::Vec::new();

    // Traverse down to the leaf node
    while depth > 0 {
        let hdr = read_header(&node_data, 0);
        let entries = hdr.entries as usize;
        let mut child_phys = None;
        for i in (0..entries).rev() {
            let idx = read_idx(&node_data, HEADER_SIZE + i * ENTRY_SIZE);
            if (logical_block as u32) >= idx.block {
                child_phys = Some(idx.leaf());
                break;
            }
        }
        let child = child_phys.unwrap_or_else(|| {
            // Hole before first index. Just take the first child.
            let idx = read_idx(&node_data, HEADER_SIZE);
            idx.leaf()
        });

        path.push((child, node_data.clone()));
        node_data = read_block_fn(child)?;
        depth -= 1;
    }

    // Now we are at the leaf node (`node_data` is either inode block or a physical block)
    let mut leaf_hdr = read_header(&node_data, 0);
    let entries = leaf_hdr.entries as usize;

    // Check if we can merge with the last extent
    if entries > 0 {
        let mut last_ext = read_extent(&node_data, HEADER_SIZE + (entries - 1) * ENTRY_SIZE);
        let last_start = last_ext.block as u64;
        let last_count = last_ext.block_count() as u64;
        let last_phys = ((last_ext.start_hi as u64) << 32) | (last_ext.start_lo as u64);

        if last_start + last_count == logical_block && last_phys + last_count == physical_block && last_count < 32768 {
            // Merge!
            last_ext.len = (last_count + 1) as u16;
            unsafe {
                core::ptr::write_unaligned(
                    node_data[HEADER_SIZE + (entries - 1) * ENTRY_SIZE..].as_mut_ptr() as *mut Extent,
                    last_ext,
                );
            }
            // Write back
            if let Some((phys, _)) = path.last() {
                write_block_fn(*phys, &node_data)?;
            } else {
                inode_block_data.copy_from_slice(&node_data[0..60]);
            }
            return Ok(());
        }
    }

    // Need to append a new extent
    if leaf_hdr.entries == leaf_hdr.max {
        if path.is_empty() {
            // Root node is full. We convert it to an index node pointing to a new leaf block.
            let new_leaf_phys = alloc_block_fn()?;
            let mut new_leaf_data = alloc::vec![0u8; block_size];
            
            // Setup new leaf header
            let max_leaf_entries = (block_size - HEADER_SIZE) / ENTRY_SIZE;
            let mut new_leaf_hdr = ExtentHeader {
                magic: EXT4_EXT_MAGIC,
                entries: leaf_hdr.entries,
                max: max_leaf_entries as u16,
                depth: 0,
                generation: 0,
            };
            unsafe { core::ptr::write_unaligned(new_leaf_data.as_mut_ptr() as *mut ExtentHeader, new_leaf_hdr); }
            
            // Copy entries
            for i in 0..leaf_hdr.entries as usize {
                let ext = read_extent(&node_data, HEADER_SIZE + i * ENTRY_SIZE);
                unsafe { core::ptr::write_unaligned(new_leaf_data[HEADER_SIZE + i * ENTRY_SIZE..].as_mut_ptr() as *mut Extent, ext); }
            }
            
            // Update root node (now an index node)
            hdr.depth = 1;
            hdr.entries = 1;
            unsafe { core::ptr::write_unaligned(inode_block_data.as_mut_ptr() as *mut ExtentHeader, hdr); }
            let first_ext = read_extent(&node_data, HEADER_SIZE); // Get start logical block
            let new_idx = ExtentIdx {
                block: first_ext.block,
                leaf_lo: (new_leaf_phys & 0xFFFFFFFF) as u32,
                leaf_hi: (new_leaf_phys >> 32) as u16,
                unused: 0,
            };
            unsafe { core::ptr::write_unaligned(inode_block_data[HEADER_SIZE..].as_mut_ptr() as *mut ExtentIdx, new_idx); }
            
            // Now the tree has deepened! The new leaf is `new_leaf_data`.
            // We can just append to `new_leaf_data` directly because it has `max_leaf_entries` (e.g. 340).
            node_data = new_leaf_data;
            leaf_hdr = read_header(&node_data, 0); // Re-read updated header
            path.push((new_leaf_phys, inode_block_data.to_vec()));
        } else {
            // A non-root block filled up. B-Tree splitting is required, but not implemented for simplicity.
            crate::println!("ext4: extent tree node splitting not implemented");
            return Err(());
        }
    }

    if leaf_hdr.entries < leaf_hdr.max {
        let cur_entries = leaf_hdr.entries as usize;
        let new_ext = Extent {
            block: logical_block as u32,
            len: 1,
            start_hi: (physical_block >> 32) as u16,
            start_lo: (physical_block & 0xFFFFFFFF) as u32,
        };
        unsafe {
            core::ptr::write_unaligned(
                node_data[HEADER_SIZE + cur_entries * ENTRY_SIZE..].as_mut_ptr() as *mut Extent,
                new_ext,
            );
        }
        leaf_hdr.entries += 1;
        unsafe {
            core::ptr::write_unaligned(
                node_data.as_mut_ptr() as *mut ExtentHeader,
                leaf_hdr,
            );
        }

        if let Some((phys, _)) = path.last() {
            write_block_fn(*phys, &node_data)?;
        } else {
            inode_block_data.copy_from_slice(&node_data[0..60]);
        }
        return Ok(());
    }

    // Leaf is full! Node splitting is not supported in this simplified implementation.
    crate::println!("ext4: insert_extent failed (node splitting not supported)");
    Err(())
}

/// Collect all physical block numbers allocated to interior index or leaf blocks
/// in the extent tree (excluding the root node embedded in the inode).
pub fn collect_structural_blocks<F>(
    inode_block_data: &[u8; 60],
    mut read_block_fn: F,
) -> Vec<u64>
where
    F: FnMut(u64) -> Result<alloc::vec::Vec<u8>, ()>,
{
    let mut structural = Vec::new();
    let hdr = read_header(inode_block_data, 0);
    if hdr.magic != EXT4_EXT_MAGIC || hdr.depth == 0 {
        return structural;
    }

    fn walk<F>(
        node_data: &[u8],
        depth: u16,
        read_block: &mut F,
        structural: &mut Vec<u64>,
    ) where
        F: FnMut(u64) -> Result<alloc::vec::Vec<u8>, ()>,
    {
        let hdr = read_header(node_data, 0);
        if hdr.magic != EXT4_EXT_MAGIC {
            return;
        }
        if depth > 0 {
            for i in 0..hdr.entries as usize {
                let idx = read_idx(node_data, HEADER_SIZE + i * ENTRY_SIZE);
                let child_phys = idx.leaf();
                structural.push(child_phys);
                if depth > 1 {
                    if let Ok(child_data) = read_block(child_phys) {
                        walk(&child_data, depth - 1, read_block, structural);
                    }
                }
            }
        }
    }

    walk(inode_block_data, hdr.depth, &mut read_block_fn, &mut structural);
    structural
}
