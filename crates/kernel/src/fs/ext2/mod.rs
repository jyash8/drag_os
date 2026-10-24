pub mod cache;
pub mod structs;

use crate::arch::sync::spinlock::SpinLock;
use crate::arch::x86_64::syscalls::{EIO, ENOENT, ENOTDIR, Stat};
use crate::fs::reader::SectorReader;
use crate::fs::vfs::FileNode;
use alloc::sync::Arc;
use alloc::vec::Vec;
use cache::BlockCache;
use core::mem::size_of;
use structs::*;

pub struct Ext2Fs {
    pub superblock: SpinLock<Superblock>,
    pub block_size: u32,
    pub cache: Arc<BlockCache>,
    /// An id that is unique among the mounted filesystems, so that the DSO
    /// cache identities of this filesystem cannot collide with another's.
    pub fs_id: u64,
}

impl Ext2Fs {
    pub fn new() -> Option<Arc<Self>> {
        Self::open(Arc::new(crate::fs::reader::GlobalReader))
    }

    /// Open an ext2 filesystem from a `SectorReader`. Used for loopback mounts
    /// where the filesystem lives in a file rather than on the block device.
    pub fn open(reader: Arc<dyn SectorReader>) -> Option<Arc<Self>> {
        let mut buf = alloc::vec::Vec::new();
        buf.resize(1024, 0);

        // Superblock is always at offset 1024 (sector 2)
        reader.read_sector(2, (&mut buf[0..512]).try_into().unwrap())
            .ok()?;
        reader.read_sector(3, (&mut buf[512..1024]).try_into().unwrap())
            .ok()?;

        let ptr = buf.as_ptr() as *const Superblock;
        let superblock = unsafe { core::ptr::read_unaligned(ptr) };

        if superblock.magic != EXT2_SUPER_MAGIC {
            return None;
        }

        let block_size = 1024 << superblock.log_block_size;
        let cache = Arc::new(BlockCache::new_with_reader(block_size, reader.clone()));

        // --- BlockCache Sanity Test ---
        let mut test_buf = alloc::vec::Vec::new();
        test_buf.resize(block_size as usize, 0);
        test_buf[0..4].copy_from_slice(b"DRGO");
        cache.write_block(9999, &test_buf).unwrap();
        let mut read_buf = alloc::vec::Vec::new();
        read_buf.resize(block_size as usize, 0);
        cache.read_block(9999, &mut read_buf).unwrap();
        assert_eq!(&read_buf[0..4], b"DRGO");
        crate::println!("BlockCache sanity test passed!");
        // ------------------------------

        Some(Arc::new(Self {
            superblock: SpinLock::new(superblock),
            block_size,
            cache,
            fs_id: crate::fs::vfs::next_dso_id(),
        }))
    }

    pub fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), ()> {
        self.cache.read_block(block, buf)
    }

    pub fn write_block(&self, block: u32, buf: &[u8]) -> Result<(), ()> {
        self.cache.write_block(block, buf)
    }

    pub fn update_superblock(&self) -> Result<(), ()> {
        let sb_block = if self.block_size == 1024 { 1 } else { 0 };
        let sb_offset = if self.block_size == 1024 { 0 } else { 1024 };

        let mut buf = Vec::new();
        buf.resize(self.block_size as usize, 0);
        self.read_block(sb_block, &mut buf)?;

        unsafe {
            core::ptr::write_unaligned(
                buf[sb_offset..].as_mut_ptr() as *mut Superblock,
                *self.superblock.lock(),
            );
        }
        self.write_block(sb_block, &buf)?;
        Ok(())
    }

    pub fn alloc_block(&self) -> Result<u32, ()> {
        let mut sb = self.superblock.lock();
        if sb.free_blocks_count == 0 {
            return Err(());
        }
        let blocks_per_group = sb.blocks_per_group;
        let total_groups = (sb.blocks_count + blocks_per_group - 1) / blocks_per_group;
        let bg_desc_block = if self.block_size == 1024 { 2 } else { 1 };

        let mut bg_buf = Vec::new();
        bg_buf.resize(self.block_size as usize, 0);
        self.read_block(bg_desc_block, &mut bg_buf)?;

        let bg_desc_size = core::mem::size_of::<BlockGroupDescriptor>();

        for group in 0..total_groups {
            let bg_offset = group as usize * bg_desc_size;
            let bg_ptr = bg_buf[bg_offset..].as_ptr() as *const BlockGroupDescriptor;
            let mut bg = unsafe { core::ptr::read_unaligned(bg_ptr) };

            if bg.free_blocks_count > 0 {
                let mut bitmap_buf = Vec::new();
                bitmap_buf.resize(self.block_size as usize, 0);
                self.read_block(bg.block_bitmap, &mut bitmap_buf)?;

                for byte_idx in 0..bitmap_buf.len() {
                    if bitmap_buf[byte_idx] != 0xFF {
                        for bit_idx in 0..8 {
                            if (bitmap_buf[byte_idx] & (1 << bit_idx)) == 0 {
                                bitmap_buf[byte_idx] |= 1 << bit_idx;
                                self.write_block(bg.block_bitmap, &bitmap_buf)?;

                                bg.free_blocks_count -= 1;
                                unsafe {
                                    core::ptr::write_unaligned(
                                        bg_buf[bg_offset..].as_mut_ptr()
                                            as *mut BlockGroupDescriptor,
                                        bg,
                                    );
                                }
                                self.write_block(bg_desc_block, &bg_buf)?;

                                sb.free_blocks_count -= 1;
                                drop(sb);
                                self.update_superblock()?;

                                let block = group * blocks_per_group
                                    + (byte_idx as u32 * 8)
                                    + bit_idx
                                    + self.superblock.lock().first_data_block;

                                let mut zero_buf = Vec::new();
                                zero_buf.resize(self.block_size as usize, 0);
                                self.write_block(block, &zero_buf)?;

                                return Ok(block);
                            }
                        }
                    }
                }
            }
        }
        Err(())
    }

    pub fn alloc_inode(&self) -> Result<u32, ()> {
        let mut sb = self.superblock.lock();
        if sb.free_inodes_count == 0 {
            return Err(());
        }
        let inodes_per_group = sb.inodes_per_group;
        let total_groups = (sb.inodes_count + inodes_per_group - 1) / inodes_per_group;
        let bg_desc_block = if self.block_size == 1024 { 2 } else { 1 };

        let mut bg_buf = Vec::new();
        bg_buf.resize(self.block_size as usize, 0);
        self.read_block(bg_desc_block, &mut bg_buf)?;

        let bg_desc_size = core::mem::size_of::<BlockGroupDescriptor>();

        for group in 0..total_groups {
            let bg_offset = group as usize * bg_desc_size;
            let bg_ptr = bg_buf[bg_offset..].as_ptr() as *const BlockGroupDescriptor;
            let mut bg = unsafe { core::ptr::read_unaligned(bg_ptr) };

            if bg.free_inodes_count > 0 {
                let mut bitmap_buf = Vec::new();
                bitmap_buf.resize(self.block_size as usize, 0);
                self.read_block(bg.inode_bitmap, &mut bitmap_buf)?;

                for byte_idx in 0..bitmap_buf.len() {
                    if bitmap_buf[byte_idx] != 0xFF {
                        for bit_idx in 0..8 {
                            if (bitmap_buf[byte_idx] & (1 << bit_idx)) == 0 {
                                bitmap_buf[byte_idx] |= 1 << bit_idx;
                                self.write_block(bg.inode_bitmap, &bitmap_buf)?;

                                bg.free_inodes_count -= 1;
                                unsafe {
                                    core::ptr::write_unaligned(
                                        bg_buf[bg_offset..].as_mut_ptr()
                                            as *mut BlockGroupDescriptor,
                                        bg,
                                    );
                                }
                                self.write_block(bg_desc_block, &bg_buf)?;

                                sb.free_inodes_count -= 1;
                                drop(sb);
                                self.update_superblock()?;

                                // Inodes are 1-indexed
                                let inode_num =
                                    group * inodes_per_group + (byte_idx as u32 * 8) + bit_idx + 1;
                                return Ok(inode_num);
                            }
                        }
                    }
                }
            }
        }
        Err(())
    }

    pub fn get_inode(&self, inode_num: u32) -> Result<Inode, ()> {
        let inodes_per_group = self.superblock.lock().inodes_per_group;
        let inode_size = self.superblock.lock().inode_size as usize;

        let group = (inode_num - 1) / inodes_per_group;
        let index = (inode_num - 1) % inodes_per_group;

        let bg_desc_block = if self.block_size == 1024 { 2 } else { 1 };

        let mut bg_buf = Vec::new();
        bg_buf.resize(self.block_size as usize, 0);
        if self.read_block(bg_desc_block, &mut bg_buf).is_err() {
            crate::println!("get_inode: failed to read bg_desc_block {}", bg_desc_block);
            return Err(());
        }

        let bg_desc_size = size_of::<BlockGroupDescriptor>();
        let bg_offset = group as usize * bg_desc_size;

        let bg_ptr = bg_buf[bg_offset..].as_ptr() as *const BlockGroupDescriptor;
        let bg = unsafe { core::ptr::read_unaligned(bg_ptr) };

        let inode_table_block = bg.inode_table;
        let inode_block = inode_table_block + (index as u32 * inode_size as u32) / self.block_size;

        let mut inode_buf = Vec::new();
        inode_buf.resize(self.block_size as usize, 0);
        if self.read_block(inode_block, &mut inode_buf).is_err() {
            crate::println!("get_inode: failed to read inode_block {}", inode_block);
            return Err(());
        }

        let offset_in_block = (index as usize * inode_size) % self.block_size as usize;
        let inode_ptr = inode_buf[offset_in_block..].as_ptr() as *const Inode;
        let inode = unsafe { core::ptr::read_unaligned(inode_ptr) };

        Ok(inode)
    }

    pub fn write_inode(&self, inode_num: u32, inode: &Inode) -> Result<(), ()> {
        let inodes_per_group = self.superblock.lock().inodes_per_group;
        let inode_size = self.superblock.lock().inode_size as usize;

        let group = (inode_num - 1) / inodes_per_group;
        let index = (inode_num - 1) % inodes_per_group;

        let bg_desc_block = if self.block_size == 1024 { 2 } else { 1 };

        let mut bg_buf = Vec::new();
        bg_buf.resize(self.block_size as usize, 0);
        self.read_block(bg_desc_block, &mut bg_buf)?;

        let bg_desc_size = core::mem::size_of::<BlockGroupDescriptor>();
        let bg_offset = group as usize * bg_desc_size;

        let bg_ptr = bg_buf[bg_offset..].as_ptr() as *const BlockGroupDescriptor;
        let bg = unsafe { core::ptr::read_unaligned(bg_ptr) };

        let inode_table_block = bg.inode_table;
        let inode_block = inode_table_block + (index as u32 * inode_size as u32) / self.block_size;
        let inode_offset = (index as u32 * inode_size as u32) % self.block_size;

        let mut inode_buf = Vec::new();
        inode_buf.resize(self.block_size as usize, 0);
        self.read_block(inode_block, &mut inode_buf)?;

        unsafe {
            core::ptr::write_unaligned(
                inode_buf[inode_offset as usize..].as_mut_ptr() as *mut Inode,
                *inode,
            );
        }

        self.write_block(inode_block, &inode_buf)?;
        Ok(())
    }
}

pub struct Ext2Node {
    pub fs: Arc<Ext2Fs>,
    pub inode_num: u32,
    pub inode: SpinLock<Inode>,
}

impl Ext2Node {
    pub fn new(fs: Arc<Ext2Fs>, inode_num: u32) -> Result<Self, ()> {
        let inode = fs.get_inode(inode_num)?;
        Ok(Self {
            fs,
            inode_num,
            inode: SpinLock::new(inode),
        })
    }

    fn get_data_block(&self, block_idx: u32) -> Result<u32, ()> {
        let inode = self.inode.lock();
        let p = self.fs.block_size / 4; // Pointers per block

        // 1. Direct blocks
        if block_idx < 12 {
            return Ok(inode.block[block_idx as usize]);
        }

        let mut rel_idx = block_idx - 12;

        let read_ptr = |block: u32, index: u32| -> Result<u32, ()> {
            if block == 0 {
                return Ok(0);
            } // Sparse
            let mut buf = Vec::new();
            buf.resize(self.fs.block_size as usize, 0);
            self.fs.read_block(block, &mut buf)?;
            let offset = index as usize * 4;
            Ok(u32::from_le_bytes(
                buf[offset..offset + 4].try_into().unwrap(),
            ))
        };

        // 2. Single indirect
        if rel_idx < p {
            return read_ptr(inode.block[12], rel_idx);
        }
        rel_idx -= p;

        // 3. Double indirect
        let p_sq = p * p;
        if rel_idx < p_sq {
            let l1_idx = rel_idx / p;
            let l2_idx = rel_idx % p;

            let l1_block = read_ptr(inode.block[13], l1_idx)?;
            return read_ptr(l1_block, l2_idx);
        }
        rel_idx -= p_sq;

        // 4. Triple indirect
        let p_cb = p_sq * p;
        if rel_idx < p_cb {
            let l1_idx = rel_idx / p_sq;
            let l2_idx = (rel_idx % p_sq) / p;
            let l3_idx = rel_idx % p;

            let l1_block = read_ptr(inode.block[14], l1_idx)?;
            let l2_block = read_ptr(l1_block, l2_idx)?;
            return read_ptr(l2_block, l3_idx);
        }

        Err(())
    }

    fn get_or_alloc_data_block(&self, block_idx: u32) -> Result<u32, ()> {
        let mut inode = self.inode.lock();
        let p = self.fs.block_size / 4; // Pointers per block

        // We will need a helper to read/write indirect block arrays.
        // To avoid deadlocks, we will just allocate and write blocks directly.
        // For simplicity, we currently only support writing up to the single indirect limit,
        // as double/triple indirect allocation is highly complex and rare for small OS files.
        // Direct limit: 12 blocks * 1KB = 12KB
        // Single limit: 12 + 256 = 268 blocks * 1KB = 268KB
        // This is plenty for our simple OS needs.

        if block_idx < 12 {
            if inode.block[block_idx as usize] == 0 {
                let new_block = self.fs.alloc_block()?;
                inode.block[block_idx as usize] = new_block;
                inode.blocks += (self.fs.block_size / 512) as u32;
                self.fs.write_inode(self.inode_num, &inode)?;
            }
            return Ok(inode.block[block_idx as usize]);
        }

        let rel_idx = block_idx - 12;
        if rel_idx < p {
            if inode.block[12] == 0 {
                let new_indirect = self.fs.alloc_block()?;
                inode.block[12] = new_indirect;
                inode.blocks += (self.fs.block_size / 512) as u32;
                self.fs.write_inode(self.inode_num, &inode)?;
            }
            let indirect_block = inode.block[12];

            let mut buf = alloc::vec::Vec::new();
            buf.resize(self.fs.block_size as usize, 0);
            self.fs.read_block(indirect_block, &mut buf)?;

            let offset = rel_idx as usize * 4;
            let mut ptr = u32::from_le_bytes(buf[offset..offset + 4].try_into().unwrap());

            if ptr == 0 {
                ptr = self.fs.alloc_block()?;
                buf[offset..offset + 4].copy_from_slice(&ptr.to_le_bytes());
                self.fs.write_block(indirect_block, &buf)?;

                inode.blocks += (self.fs.block_size / 512) as u32;
                self.fs.write_inode(self.inode_num, &inode)?;
            }

            return Ok(ptr);
        }

        // Double and Triple indirect allocation are unsupported in this basic implementation for now.
        // We can add them later if 268KB is exceeded.
        Err(())
    }

    /// Walks a directory, returning `(inode, name, file_type)` for each entry.
    /// `file_type`: 1=regular, 2=directory, per ext2.
    fn dir_entries(&self) -> Result<alloc::vec::Vec<(u32, alloc::string::String, u8)>, u64> {
        if !self.is_dir() {
            return Err(ENOENT);
        }

        let mut entries = alloc::vec::Vec::new();
        let mut offset = 0;
        let size = self.size();
        let mut buf = alloc::vec::Vec::new();
        buf.resize(self.fs.block_size as usize, 0);

        // Fixed header size: inode(4) + rec_len(2) + name_len(1) + file_type(1) = 8
        const DIR_ENTRY_HEADER_SIZE: usize = 8;

        while offset < size {
            let block_idx = (offset / self.fs.block_size as usize) as u32;
            let phys_block = self.get_data_block(block_idx).map_err(|_| EIO)?;

            if phys_block == 0 {
                offset += self.fs.block_size as usize;
                continue;
            }

            self.fs.read_block(phys_block, &mut buf).map_err(|_| EIO)?;

            let mut block_offset = 0;
            while block_offset < self.fs.block_size as usize {
                if block_offset + DIR_ENTRY_HEADER_SIZE > self.fs.block_size as usize {
                    break;
                }

                // Read just the fixed header fields
                let inode =
                    u32::from_le_bytes(buf[block_offset..block_offset + 4].try_into().unwrap());
                let rec_len =
                    u16::from_le_bytes(buf[block_offset + 4..block_offset + 6].try_into().unwrap());
                let name_len = buf[block_offset + 6] as usize;
                let file_type = buf[block_offset + 7];

                if inode != 0 && name_len > 0 {
                    let name_start = block_offset + DIR_ENTRY_HEADER_SIZE;
                    let name_end = name_start + name_len;
                    if name_end <= self.fs.block_size as usize {
                        if let Ok(entry_name) = core::str::from_utf8(&buf[name_start..name_end]) {
                            entries.push((inode, alloc::string::String::from(entry_name), file_type));
                        }
                    }
                }

                if rec_len == 0 {
                    break;
                }
                block_offset += rec_len as usize;
            }

            offset += self.fs.block_size as usize;
        }

        Ok(entries)
    }
}

impl FileNode for Ext2Node {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, u64> {
        let mut inode = self.inode.lock();
        if offset >= inode.size as usize {
            return Ok(0);
        }
        let remaining = inode.size as usize - offset;
        let to_read = core::cmp::min(remaining, buf.len());
        let mut bytes_read = 0;

        let mut block_buf = Vec::new();
        block_buf.resize(self.fs.block_size as usize, 0);

        while bytes_read < to_read {
            let current_offset = offset + bytes_read;
            let block_idx = (current_offset / self.fs.block_size as usize) as u32;
            let offset_in_block = current_offset % self.fs.block_size as usize;
            let chunk = core::cmp::min(
                self.fs.block_size as usize - offset_in_block,
                to_read - bytes_read,
            );

            // Need to drop inode lock before calling get_data_block which locks it again
            drop(inode);
            let phys_block = self.get_data_block(block_idx).map_err(|_| EIO)?;
            inode = self.inode.lock();

            if phys_block == 0 {
                // Sparse block
                buf[bytes_read..bytes_read + chunk].fill(0);
            } else {
                self.fs
                    .read_block(phys_block, &mut block_buf)
                    .map_err(|_| EIO)?;
                buf[bytes_read..bytes_read + chunk]
                    .copy_from_slice(&block_buf[offset_in_block..offset_in_block + chunk]);
            }

            bytes_read += chunk;
        }

        Ok(bytes_read)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, u64> {
        let mut inode = self.inode.lock();

        let to_write = buf.len();
        if to_write == 0 {
            return Ok(0);
        }

        let new_size = core::cmp::max(inode.size as usize, offset + to_write);

        let mut current_offset = offset;
        let mut bytes_written = 0;

        let mut block_buf = alloc::vec::Vec::new();
        block_buf.resize(self.fs.block_size as usize, 0);

        while bytes_written < to_write {
            let block_idx = (current_offset / self.fs.block_size as usize) as u32;
            let offset_in_block = current_offset % self.fs.block_size as usize;
            let chunk = core::cmp::min(
                self.fs.block_size as usize - offset_in_block,
                to_write - bytes_written,
            );

            // Need to drop inode lock before calling get_or_alloc_data_block which locks it again
            drop(inode);
            let phys_block = self.get_or_alloc_data_block(block_idx).map_err(|_| EIO)?;
            inode = self.inode.lock();

            if phys_block == 0 {
                // This shouldn't happen since we just allocated it, but just in case
                return Err(EIO);
            }

            if chunk == self.fs.block_size as usize {
                // Full block write
                self.fs
                    .write_block(phys_block, &buf[bytes_written..bytes_written + chunk])
                    .map_err(|_| EIO)?;
            } else {
                // Partial block write - read modify write
                self.fs
                    .read_block(phys_block, &mut block_buf)
                    .map_err(|_| EIO)?;
                block_buf[offset_in_block..offset_in_block + chunk]
                    .copy_from_slice(&buf[bytes_written..bytes_written + chunk]);
                self.fs
                    .write_block(phys_block, &block_buf)
                    .map_err(|_| EIO)?;
            }

            current_offset += chunk;
            bytes_written += chunk;
        }

        if new_size > inode.size as usize {
            inode.size = new_size as u32;
        }

        // Write the inode back to disk
        self.fs
            .write_inode(self.inode_num, &inode)
            .map_err(|_| EIO)?;

        Ok(bytes_written)
    }

    fn create(&self, name: &str, is_dir: bool, mode: u32, uid: u32, gid: u32) -> Result<Arc<dyn FileNode>, u64> {
        let mut inode = self.inode.lock();
        if (inode.mode & EXT2_S_IFMT) != EXT2_S_IFDIR {
            crate::println!("Ext2Node::create: ENOTDIR");
            return Err(ENOTDIR as u64); // Need to use standard errno values
        }

        let name_len = name.len();
        if name_len > 255 || name_len == 0 {
            crate::println!("Ext2Node::create: ENOENT (name_len={})", name_len);
            return Err(ENOENT);
        }
        crate::println!("Ext2Node::create: allocating inode for {}", name);

        // 1. Allocate new inode
        let new_inode_num = self.fs.alloc_inode().map_err(|_| EIO)?;
        let mut new_inode = Inode::default();
        let type_bits = if is_dir { EXT2_S_IFDIR } else { EXT2_S_IFREG };
        new_inode.mode = type_bits | mode as u16;
        new_inode.uid = uid as u16;
        new_inode.gid = gid as u16;
        new_inode.links_count = if is_dir { 2 } else { 1 };
        new_inode.size = 0;
        new_inode.blocks = 0;
        self.fs
            .write_inode(new_inode_num, &new_inode)
            .map_err(|_| EIO)?;

        // 2. Find empty slot in current directory
        let mut offset = 0;
        let mut buf = alloc::vec::Vec::new();
        buf.resize(self.fs.block_size as usize, 0);

        let required_rec_len = (8 + name_len + 3) & !3; // 8 bytes header + name + padding
        let mut inserted = false;

        // Fixed header size: inode(4) + rec_len(2) + name_len(1) + file_type(1) = 8
        const DIR_ENTRY_HEADER_SIZE: usize = 8;

        while offset < inode.size as usize {
            let block_idx = (offset / self.fs.block_size as usize) as u32;

            // Drop lock before calling get_or_alloc_data_block
            drop(inode);
            let phys_block = self.get_or_alloc_data_block(block_idx).map_err(|_| EIO)?;
            inode = self.inode.lock();

            self.fs.read_block(phys_block, &mut buf).map_err(|_| EIO)?;

            let mut block_offset = 0;
            while block_offset < self.fs.block_size as usize {
                if block_offset + DIR_ENTRY_HEADER_SIZE > self.fs.block_size as usize {
                    break;
                }

                let entry_inode =
                    u32::from_le_bytes(buf[block_offset..block_offset + 4].try_into().unwrap());
                let entry_rec_len =
                    u16::from_le_bytes(buf[block_offset + 4..block_offset + 6].try_into().unwrap());
                let entry_name_len = buf[block_offset + 6] as usize;

                let actual_rec_len = if entry_inode != 0 {
                    (8 + entry_name_len + 3) & !3
                } else {
                    0
                };

                let free_space = entry_rec_len as usize - actual_rec_len;

                if entry_inode == 0 || free_space >= required_rec_len {
                    // We can insert here!
                    if entry_inode != 0 {
                        // Shrink the current entry's rec_len
                        buf[block_offset + 4..block_offset + 6]
                            .copy_from_slice(&(actual_rec_len as u16).to_le_bytes());

                        // Create the new entry in the freed space
                        block_offset += actual_rec_len;
                        buf[block_offset..block_offset + 4]
                            .copy_from_slice(&new_inode_num.to_le_bytes());
                        buf[block_offset + 4..block_offset + 6]
                            .copy_from_slice(&(free_space as u16).to_le_bytes());
                        buf[block_offset + 6] = name_len as u8;
                        buf[block_offset + 7] = if is_dir { 2 } else { 1 };
                        buf[block_offset + 8..block_offset + 8 + name_len]
                            .copy_from_slice(name.as_bytes());
                    } else {
                        // Completely empty entry (e.g. deleted file or end of block)
                        buf[block_offset..block_offset + 4]
                            .copy_from_slice(&new_inode_num.to_le_bytes());
                        // Keep the original rec_len
                        buf[block_offset + 6] = name_len as u8;
                        buf[block_offset + 7] = if is_dir { 2 } else { 1 };
                        buf[block_offset + 8..block_offset + 8 + name_len]
                            .copy_from_slice(name.as_bytes());
                    }

                    self.fs.write_block(phys_block, &buf).map_err(|_| EIO)?;
                    inserted = true;
                    break;
                }

                if entry_rec_len == 0 {
                    break;
                } // Corrupted directory
                block_offset += entry_rec_len as usize;
            }

            if inserted {
                break;
            }
            offset += self.fs.block_size as usize;
        }

        if !inserted {
            // Need to allocate a new block for the directory
            let block_idx = (inode.size as usize / self.fs.block_size as usize) as u32;

            drop(inode);
            let phys_block = self.get_or_alloc_data_block(block_idx).map_err(|_| EIO)?;
            inode = self.inode.lock();

            buf.fill(0);
            buf[0..4].copy_from_slice(&new_inode_num.to_le_bytes());
            buf[4..6].copy_from_slice(&(self.fs.block_size as u16).to_le_bytes());
            buf[6] = name_len as u8;
            buf[7] = if is_dir { 2 } else { 1 };
            buf[8..8 + name_len].copy_from_slice(name.as_bytes());

            self.fs.write_block(phys_block, &buf).map_err(|_| EIO)?;

            inode.size += self.fs.block_size;
            self.fs
                .write_inode(self.inode_num, &inode)
                .map_err(|_| EIO)?;
        }

        Ok(Arc::new(
            Ext2Node::new(self.fs.clone(), new_inode_num).map_err(|_| EIO)?,
        ))
    }

    fn truncate(&self, _size: usize) -> Result<(), u64> {
        Err(EIO)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        if !self.is_dir() {
            return Err(ENOENT);
        }

        for (inode, entry_name, _file_type) in self.dir_entries()? {
            if entry_name == name {
                return Ok(Arc::new(Ext2Node::new(self.fs.clone(), inode).map_err(|_| EIO)?));
            }
        }

        Err(ENOENT)
    }

    fn list_dir(&self) -> Result<alloc::vec::Vec<(alloc::string::String, bool)>, u64> {
        if !self.is_dir() {
            return Err(ENOTDIR);
        }
        Ok(self
            .dir_entries()?
            .into_iter()
            .map(|(_inode, name, file_type)| (name, (file_type & 2) != 0))
            .collect())
    }

    fn stat(&self) -> Stat {
        let st = Stat::default();
        // diagnostic: all zero to test if 0x100000000 comes from stat
        st
    }

    fn is_dir(&self) -> bool {
        let inode = self.inode.lock();
        (inode.mode & EXT2_S_IFMT) == EXT2_S_IFDIR
    }

    fn size(&self) -> usize {
        let inode = self.inode.lock();
        inode.size as usize
    }

    fn cache_id(&self) -> u64 {
        // Unique across filesystems; the inode number is unique within one.
        (self.fs.fs_id << 32) | self.inode_num as u64
    }
}
