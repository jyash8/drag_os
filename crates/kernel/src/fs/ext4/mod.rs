// ext4 filesystem driver — main module
//
// This module provides read-only ext4 filesystem support. It is structurally
// parallel to the existing ext2 driver (`kernel/src/fs/ext2`) and implements
// the same `FileNode` trait from `kernel/src/fs/vfs.rs`.
//
// # Status
//   Scaffolded — on-disk structures and module skeleton are complete.
//   Implementation of I/O methods is deferred.
//
// # Architecture
//   Ext4Fs     — filesystem-level state: superblock, block cache, geometry.
//   Ext4Node   — per-file/dir handle implementing the VFS `FileNode` trait.
//
// # Key differences from the ext2 driver
//   - Extent trees instead of indirect block maps (i_block reinterpretation).
//   - 64-bit block addresses (INCOMPAT_64BIT + 64-byte group descriptors).
//   - Larger inodes (256+ bytes) with nanosecond timestamps and checksums.
//   - htree-indexed directories (transparent; linear scan works as fallback).
//   - Journal (JBD2) — we skip replay for now (mount read-only after unclean).

#![allow(unused)]

pub mod crc32c;
pub mod extents;
pub mod structs;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use crate::arch::sync::spinlock::SpinLock;
use crate::arch::x86_64::syscalls::{EIO, EISDIR, ENOENT, ENOTDIR, Stat};

/// Write one linear directory entry at `off` in a block buffer.
fn write_dirent(buf: &mut [u8], off: usize, ino: u32, rec_len: u16, file_type: u8, name: &[u8]) {
    buf[off..off + 4].copy_from_slice(&ino.to_le_bytes());
    buf[off + 4..off + 6].copy_from_slice(&rec_len.to_le_bytes());
    buf[off + 6] = name.len() as u8;
    buf[off + 7] = file_type;
    buf[off + 8..off + 8 + name.len()].copy_from_slice(name);
}

fn current_time_sec() -> u32 {
    let nanos = crate::time::realtime_nanos();
    let sec = if nanos > 0 {
        (nanos / 1_000_000_000) as u32
    } else {
        0
    };
    if sec == 0 {
        1725838000
    } else {
        sec
    }
}
use crate::fs::ext2::cache::BlockCache;
use crate::fs::reader::SectorReader;
use crate::fs::vfs::FileNode;

use structs::*;

// ── Filesystem handle ────────────────────────────────────────────────────────

pub struct Ext4Fs {
    pub superblock: SpinLock<Superblock>,
    pub block_size: u32,
    pub inode_size: u32,
    pub groups_count: u32,
    pub desc_size: u32,
    pub is_64bit: bool,
    pub cache: Arc<BlockCache>,
    /// An id that is unique among the mounted filesystems.
    ///
    /// The DSO cache is keyed by (cache id, page offset), and the cache id
    /// used to be the inode number -- which is only unique *within* one
    /// filesystem. The guest mounts two of them (the root image and
    /// disk2.img), so the same (inode, offset) named two different files,
    /// and a page of one could be served for the other. The dynamic loader
    /// reads its libraries through this path, and a page of zeros from the
    /// wrong filesystem is what made it dereference a null pointer.
    pub fs_id: u64,
}

impl Ext4Fs {
    /// Try to mount an ext4 filesystem from the block device.
    ///
    /// Reads the superblock at offset 1024, validates the magic number and
    /// feature flags, and initialises the filesystem handle.
    ///
    /// Returns `None` if the superblock is not ext4 or if required incompat
    /// features are not supported.
    pub fn new() -> Option<Arc<Self>> {
        // Read 2 sectors (1024 bytes) starting at byte offset 1024.
        let mut buf = alloc::vec![0u8; 1024];
        crate::drivers::block::read_sector(2, (&mut buf[0..512]).try_into().unwrap()).ok()?;
        crate::drivers::block::read_sector(3, (&mut buf[512..1024]).try_into().unwrap()).ok()?;

        let sb = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Superblock) };

        if sb.magic != EXT4_SUPER_MAGIC {
            return None;
        }

        if sb.blocks_per_group == 0 || sb.inodes_per_group == 0 {
            crate::dprintln!("[ext4] invalid blocks/inodes per group (0)");
            return None;
        }

        // Reject features we do not understand.
        //
        // The set below covers every INCOMPAT bit a stock `mkfs.ext4` turns
        // on, so a normal host-created ext4 image mounts as-is. On a 4K-block
        // default image that is exactly 0x22C2:
        //   FILETYPE|EXTENTS|64BIT|FLEX_BG|CSUM_SEED
        //
        // Notes on the less obvious bits:
        //
        //   CSUM_SEED (0x2000) — a per-filesystem checksum seed stored in the
        //     superblock rather than one derived from the UUID. Handled by
        //     `Ext4Fs::csum_seed`; see the metadata-checksum section below.
        //   RECOVER (0x0004)  — needs a JBD2 replay. We mount anyway and let
        //     whatever tool touches the image next replay it.
        //
        // Note that htree/dir_index is *not* gated here: it lives in the
        // COMPAT field (EXT4_FEATURE_COMPAT_DIR_INDEX = 0x0020) and does not
        // affect the on-disk layout we walk. `iter_dir` handles indexed
        // directories transparently because it linearly scans blocks and
        // skips the `inode == 0` fake dirent an htree root block begins with.
        //
        // Deliberately still rejected: INLINE_DATA (0x8000), EA_INODE
        // (0x0400) and LARGEDIR (0x4000). These move file data or metadata
        // out of the inode's extent tree and need real support, not a flag.
        const SUPPORTED_INCOMPAT: u32 = EXT4_FEATURE_INCOMPAT_FILETYPE
            | EXT4_FEATURE_INCOMPAT_EXTENTS
            | EXT4_FEATURE_INCOMPAT_64BIT
            | EXT4_FEATURE_INCOMPAT_FLEX_BG
            | EXT4_FEATURE_INCOMPAT_RECOVER
            | EXT4_FEATURE_INCOMPAT_META_BG
            | EXT4_FEATURE_INCOMPAT_CSUM_SEED;

        let unsupported = sb.feature_incompat & !SUPPORTED_INCOMPAT;
        if unsupported != 0 {
            crate::dprintln!(
                "[ext4] unsupported incompat features: {:#x}",
                unsupported
            );
            return None;
        }

        let block_size = sb.block_size();
        let inode_size = if sb.rev_level >= 1 { sb.inode_size as u32 } else { 128 };
        let groups_count = sb.group_count();
        let desc_size = sb.group_desc_size();
        let is_64bit = sb.is_64bit();

        let cache = Arc::new(BlockCache::new(block_size));

        Some(Arc::new(Self {
            superblock: SpinLock::new(sb),
            block_size,
            inode_size,
            groups_count,
            desc_size,
            is_64bit,
            cache,
            fs_id: crate::fs::vfs::next_dso_id(),
        }))
    }

    /// Open an ext4 filesystem from a `SectorReader`. Used for loopback mounts
    /// where the filesystem lives in a file rather than on the block device.
    pub fn open(reader: Arc<dyn SectorReader>) -> Option<Arc<Self>> {
        // Read 2 sectors (1024 bytes) starting at byte offset 1024.
        let mut buf = alloc::vec![0u8; 1024];
        reader.read_sector(2, (&mut buf[0..512]).try_into().unwrap()).ok()?;
        reader.read_sector(3, (&mut buf[512..1024]).try_into().unwrap()).ok()?;

        let sb = unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const Superblock) };

        if sb.magic != EXT4_SUPER_MAGIC {
            return None;
        }

        if sb.blocks_per_group == 0 || sb.inodes_per_group == 0 {
            crate::dprintln!("[ext4] invalid blocks/inodes per group (0)");
            return None;
        }

        const SUPPORTED_INCOMPAT: u32 = EXT4_FEATURE_INCOMPAT_FILETYPE
            | EXT4_FEATURE_INCOMPAT_EXTENTS
            | EXT4_FEATURE_INCOMPAT_64BIT
            | EXT4_FEATURE_INCOMPAT_FLEX_BG
            | EXT4_FEATURE_INCOMPAT_RECOVER
            | EXT4_FEATURE_INCOMPAT_META_BG
            | EXT4_FEATURE_INCOMPAT_CSUM_SEED;

        let unsupported = sb.feature_incompat & !SUPPORTED_INCOMPAT;
        if unsupported != 0 {
            crate::dprintln!(
                "[ext4] unsupported incompat features: {:#x}",
                unsupported
            );
            return None;
        }

        let block_size = sb.block_size();
        let inode_size = if sb.rev_level >= 1 { sb.inode_size as u32 } else { 128 };
        let groups_count = sb.group_count();
        let desc_size = sb.group_desc_size();
        let is_64bit = sb.is_64bit();

        let cache = Arc::new(BlockCache::new_with_reader(block_size, reader.clone()));

        Some(Arc::new(Self {
            superblock: SpinLock::new(sb),
            block_size,
            inode_size,
            groups_count,
            desc_size,
            is_64bit,
            cache,
            fs_id: crate::fs::vfs::next_dso_id(),
        }))
    }

    /// Read a full block into `buf` (which must be `block_size` bytes).
    pub fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<(), ()> {
        self.cache.read_block(block as u32, buf)
    }

    /// Write a full block from `buf`.
    pub fn write_block(&self, block: u64, buf: &[u8]) -> Result<(), ()> {
        self.cache.write_block(block as u32, buf)
    }

    /// Flush dirty blocks and write the in-memory superblock back to disk.
    pub fn sync_all(&self) -> Result<(), ()> {
        let sb_block = if self.block_size == 1024 { 1 } else { 0 };
        let sb_offset = if self.block_size == 1024 { 0 } else { 1024 };
        let mut buf = alloc::vec![0u8; self.block_size as usize];
        self.read_block(sb_block, &mut buf)?;
        unsafe {
            core::ptr::write_unaligned(
                buf[sb_offset..].as_mut_ptr() as *mut Superblock,
                *self.superblock.lock(),
            );
        }
        // s_checksum (0x3FC) covers s_block[0..0x3FC], seeded with ~0 and with
        // no filesystem seed mixed in.
        if self.has_metadata_csum() && sb_offset + 0x400 <= buf.len() {
            let csum = crc32c::checksum(!0u32, &buf[sb_offset..sb_offset + 0x3FC]);
            buf[sb_offset + 0x3FC..sb_offset + 0x400].copy_from_slice(&csum.to_le_bytes());
        }
        self.write_block(sb_block, &buf)?;
        self.cache.sync()
    }

    // ── Metadata checksums ───────────────────────────────────────────────
    //
    // With RO_COMPAT_METADATA_CSUM every metadata object carries a crc32c over
    // its own bytes with the checksum field itself zeroed. Reads do not need to
    // verify these (a mismatch means the image is already damaged and there is
    // nothing useful to do about it here), but *writes* must recompute them or
    // e2fsck will report the group as corrupt.
    //
    // All of the formulas below follow `lib/ext2fs/csum.c` in e2fsprogs, which
    // is the definition the on-disk values are written against. Every one of
    // them starts from the same filesystem-wide `csum_seed`; the per-object
    // variation comes from chaining the object's identity (inode number and
    // generation) into that seed. Note the bitmap checksums do *not* include
    // the group number.

    /// Whether this filesystem stores metadata checksums at all.
    pub fn has_metadata_csum(&self) -> bool {
        self.superblock.lock().has_metadata_csum()
    }

    /// Total blocks in the filesystem (64-bit combined).
    pub fn blocks_count(&self) -> u64 {
        self.superblock.lock().blocks_count()
    }

    /// Total inodes in the filesystem (inodes_per_group * group count).
    pub fn inodes_count(&self) -> u64 {
        self.superblock.lock().inodes_per_group as u64 * self.groups_count as u64
    }

    /// The filesystem-wide checksum seed (`ext2fs_init_csum_seed`).
    ///
    /// When INCOMPAT_CSUM_SEED is set, `mke2fs` stores a per-filesystem seed in
    /// the superblock and we must use that exact value. Otherwise the seed is
    /// derived from the filesystem UUID.
    pub fn csum_seed(&self) -> u32 {
        let sb = self.superblock.lock();
        if sb.has_metadata_csum_seed() {
            sb.checksum_seed
        } else {
            let mut seed = !0u32;
            crc32c::crc32c(&mut seed, &sb.uuid);
            seed
        }
    }

    /// Start a per-object checksum: seed the running value with the filesystem
    /// seed, then chain in the owning inode's number and generation. Used by
    /// the inode, directory-block and extent-block checksums.
    fn object_csum_seed(&self, ino: u32, generation: u32) -> u32 {
        let mut csum = self.csum_seed();
        crc32c::crc32c(&mut csum, &ino.to_le_bytes());
        crc32c::crc32c(&mut csum, &generation.to_le_bytes());
        csum
    }

    /// Recompute the checksum of an on-disk directory block and store it in the
    /// block's trailing `dir_entry_tail` (`ext2fs_dirent_csum_set`).
    ///
    /// The CRC covers everything from the start of the block up to the tail —
    /// i.e. all the dirents, but not the 12-byte tail itself.
    pub fn update_dir_block_csum(&self, dir_ino: u32, dir: &Inode, blk: &mut [u8]) {
        if !self.has_metadata_csum() || blk.len() < 12 {
            return;
        }
        let mut csum = self.object_csum_seed(dir_ino, dir.generation);
        crc32c::crc32c(&mut csum, &blk[..blk.len() - 12]);
        let off = blk.len() - 4;
        blk[off..].copy_from_slice(&csum.to_le_bytes());
    }

    /// Write a fresh `dir_entry_tail` into the last 12 bytes of a brand-new
    /// directory block. The block must already contain its entries, with the
    /// last one stopping 12 bytes short of the end.
    fn init_dir_block_tail(&self, dir_ino: u32, dir: &Inode, blk: &mut [u8]) {
        let bs = blk.len();
        if bs < 12 {
            return;
        }
        if self.has_metadata_csum() {
            let tail = bs - 12;
            blk[tail..tail + 4].copy_from_slice(&0u32.to_le_bytes()); // det_reserved_zero1
            blk[tail + 4..tail + 6].copy_from_slice(&12u16.to_le_bytes()); // det_rec_len
            blk[tail + 6] = 0; // det_reserved_name_len
            blk[tail + 7] = EXT4_FT_DIR_TAIL;
            blk[tail + 8..tail + 12].copy_from_slice(&0u32.to_le_bytes()); // det_checksum
            self.update_dir_block_csum(dir_ino, dir, blk);
        }
    }

    /// crc32c of a group block or inode bitmap (`ext2fs_block_bitmap_csum_set`):
    /// the filesystem seed followed by the bitmap bytes. The group number is
    /// deliberately *not* mixed in.
    ///
    /// `count` is the number of objects the group holds (blocks or inodes). Only
    /// the first `ceil(count / 8)` bytes are covered — the rest of the on-disk
    /// block is padding beyond the group's objects and is not part of the CRC.
    /// This matters for inode bitmaps, where a 4 KiB block holds far more bits
    /// than `inodes_per_group` needs.
    fn bitmap_csum(&self, count: u32, bitmap: &[u8]) -> u32 {
        let len = core::cmp::min(((count as usize) + 7) / 8, bitmap.len());
        let mut csum = self.csum_seed();
        crc32c::crc32c(&mut csum, &bitmap[..len]);
        csum
    }

    /// Set the trailing padding bits of a group's bitmap to 1.
    ///
    /// A bitmap's last block has bits beyond the group's block/inode count
    /// that correspond to no object. e2fsck requires them set, otherwise it
    /// reports "Padding at end of ... bitmap is not set".
    fn set_bitmap_padding(bitmap: &mut [u8], valid_bits: u32) {
        let total_bits = (bitmap.len() * 8) as u32;
        let mut bit = valid_bits;
        while bit < total_bits {
            bitmap[(bit / 8) as usize] |= 1 << (bit % 8);
            bit += 1;
        }
    }

    /// After flipping a bit in a group's bitmap, refresh everything e2fsck
    /// checks about that bitmap: the trailing padding (written into the
    /// buffer itself, since the checksum covers the on-disk bytes), the group
    /// descriptor's `*_bitmap_csum`, and — via `write_group_desc` — the
    /// descriptor's own checksum. `is_inode` selects which bitmap we touched.
    fn commit_bitmap(
        &self,
        desc: &mut BlockGroupDesc,
        bitmap: &mut [u8],
        valid_bits: u32,
        is_inode: bool,
    ) {
        Self::set_bitmap_padding(bitmap, valid_bits);
        if self.has_metadata_csum() {
            let csum = self.bitmap_csum(valid_bits, bitmap);
            if is_inode {
                desc.inode_bitmap_csum_lo = csum as u16;
                desc.inode_bitmap_csum_hi = (csum >> 16) as u16;
            } else {
                desc.block_bitmap_csum_lo = csum as u16;
                desc.block_bitmap_csum_hi = (csum >> 16) as u16;
            }
        }
    }

    // ── Group descriptor table ───────────────────────────────────────────

    /// Read the block group descriptor for group `group`.
    pub fn read_group_desc(&self, group: u32) -> Result<BlockGroupDesc, ()> {
        let sb = self.superblock.lock();
        let gdt_block = if self.block_size == 1024 { 2 } else { 1 };
        let offset_in_gdt = group as u64 * self.desc_size as u64;
        let block_num = gdt_block + offset_in_gdt / self.block_size as u64;
        let offset_in_block = (offset_in_gdt % self.block_size as u64) as usize;
        drop(sb);

        let mut buf = alloc::vec![0u8; self.block_size as usize];
        self.read_block(block_num, &mut buf)?;

        let mut desc = BlockGroupDesc::default();
        let copy_size = core::cmp::min(self.desc_size as usize, core::mem::size_of::<BlockGroupDesc>());
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf[offset_in_block..].as_ptr(),
                &mut desc as *mut BlockGroupDesc as *mut u8,
                copy_size,
            );
        }
        Ok(desc)
    }

    /// Write the block group descriptor for group `group`.
    pub fn write_group_desc(&self, group: u32, desc: &BlockGroupDesc) -> Result<(), ()> {
        let sb = self.superblock.lock();
        let gdt_block = if self.block_size == 1024 { 2 } else { 1 };
        let offset_in_gdt = group as u64 * self.desc_size as u64;
        let block_num = gdt_block + offset_in_gdt / self.block_size as u64;
        let offset_in_block = (offset_in_gdt % self.block_size as u64) as usize;
        drop(sb);

        let mut buf = alloc::vec![0u8; self.block_size as usize];
        self.read_block(block_num, &mut buf)?;

        let copy_size = core::cmp::min(self.desc_size as usize, core::mem::size_of::<BlockGroupDesc>());
        unsafe {
            core::ptr::copy_nonoverlapping(
                desc as *const BlockGroupDesc as *const u8,
                buf[offset_in_block..].as_mut_ptr(),
                copy_size,
            );
        }

        // bg_checksum (u16 at 0x1E) covers the whole descriptor with that field
        // zeroed, chained after the filesystem seed and the group number. The
        // two zero bytes must be *hashed*, not skipped — omitting them from
        // the byte stream would yield a different CRC.
        if self.has_metadata_csum() {
            let start = offset_in_block;
            let end = start + self.desc_size as usize;
            if end <= buf.len() {
                buf[start + 0x1E] = 0;
                buf[start + 0x1F] = 0;
                let mut csum = self.csum_seed();
                crc32c::crc32c(&mut csum, &group.to_le_bytes());
                crc32c::crc32c(&mut csum, &buf[start..end]);
                buf[start + 0x1E..start + 0x20].copy_from_slice(&(csum as u16).to_le_bytes());
            }
        }

        self.write_block(block_num, &buf)
    }

    /// Index of the first clear (free) bit in `bitmap`, or `None` if full.
    ///
    /// Written as a single pipeline over the bitmap's bytes rather than the
    /// nested `for byte { for bit }` scan it replaces — the same search, with
    /// the inner loop flattened into a bit scan that only visits non-`0xFF`
    /// bytes.
    fn find_free_bit(bitmap: &[u8]) -> Option<usize> {
        bitmap
            .iter()
            .position(|&byte| byte != 0xFF)
            .and_then(|byte_idx| {
                let free = !bitmap[byte_idx];
                // trailing_ones on the inverted byte: the lowest clear bit of
                // the original, i.e. the first free position in this byte.
                let bit_idx = (free & 0xFF).trailing_zeros() as usize;
                Some(byte_idx * 8 + bit_idx)
            })
    }

    /// Flip the bit at `bit` in `bitmap`, returning the previous value.
    fn test_and_set_bit(bitmap: &mut [u8], bit: usize) -> bool {
        let byte = &mut bitmap[bit / 8];
        let mask = 1u8 << (bit % 8);
        let was_set = *byte & mask != 0;
        *byte |= mask;
        was_set
    }

    /// Clear the bit at `bit` in `bitmap`, returning the previous value.
    fn test_and_clear_bit(bitmap: &mut [u8], bit: usize) -> bool {
        let byte = &mut bitmap[bit / 8];
        let mask = 1u8 << (bit % 8);
        let was_set = *byte & mask != 0;
        *byte &= !mask;
        was_set
    }

    /// Allocate a new physical block. Very basic linear scan of block bitmaps.
    pub fn alloc_block(&self) -> Result<u64, ()> {
        let sb = self.superblock.lock();
        let groups = self.groups_count;
        let blocks_per_group = sb.blocks_per_group;
        let mut first_data_block = sb.first_data_block;
        drop(sb);

        let mut buf = alloc::vec![0u8; self.block_size as usize];

        for group in 0..groups {
            let mut desc = self.read_group_desc(group)?;
            if desc.free_blocks_count() == 0 {
                continue;
            }

            let bitmap_block = desc.block_bitmap();
            self.read_block(bitmap_block, &mut buf)?;

            // How many bits of this group's block bitmap are meaningful. Only
            // the final group can be short (a partial block group).
            let valid_bits = if group + 1 == groups {
                let used = self.blocks_count().saturating_sub(group as u64 * blocks_per_group as u64) as u32;
                core::cmp::min(used, blocks_per_group)
            } else {
                blocks_per_group
            };

            if let Some(bit) = Self::find_free_bit(&buf) {
                // Found a free block! Mark it as used.
                Self::test_and_set_bit(&mut buf, bit);
                // Refresh the padding + bitmap checksum in the descriptor
                // before it is written back.
                self.commit_bitmap(&mut desc, &mut buf, valid_bits, false);
                self.write_block(bitmap_block, &buf)?;
                desc.clear_block_bitmap_uninit();
                self.write_group_desc(group, &desc)?;

                let free_bg = desc.free_blocks_count().saturating_sub(1);
                desc.free_blocks_count_lo = free_bg as u16;
                desc.free_blocks_count_hi = (free_bg >> 16) as u16;
                self.write_group_desc(group, &desc)?;

                let mut sb = self.superblock.lock();
                let free_sb = sb.free_blocks_count().saturating_sub(1);
                sb.free_blocks_count_lo = free_sb as u32;
                sb.free_blocks_count_hi = (free_sb >> 32) as u32;
                drop(sb);

                // The block number is relative to the start of the group.
                let relative_block = bit as u64;
                let absolute_block =
                    first_data_block as u64 + (group as u64 * blocks_per_group as u64) + relative_block;

                // Zero the new block
                let zero_buf = alloc::vec![0u8; self.block_size as usize];
                self.write_block(absolute_block, &zero_buf)?;

                return Ok(absolute_block);
            }
        }
        crate::println!("ext4: alloc_block failed (no space)");
        Err(())
    }

    /// Allocate a new inode.
    pub fn alloc_inode(&self, is_dir: bool) -> Result<u32, ()> {
        let sb = self.superblock.lock();
        let groups = self.groups_count;
        let inodes_per_group = sb.inodes_per_group;
        drop(sb);

        let mut buf = alloc::vec![0u8; self.block_size as usize];

        for group in 0..groups {
            let mut desc = self.read_group_desc(group)?;
            if desc.free_inodes_count() == 0 {
                continue;
            }

            let bitmap_block = desc.inode_bitmap();
            self.read_block(bitmap_block, &mut buf)?;

            // Only the last inode group can have fewer inodes than
            // inodes_per_group; the rest of its bitmap is padding.
            let valid_bits = if group + 1 == groups {
                let total = self.inodes_count();
                let used = total.saturating_sub(group as u64 * inodes_per_group as u64) as u32;
                core::cmp::min(used, inodes_per_group)
            } else {
                inodes_per_group
            };

            if let Some(bit) = Self::find_free_bit(&buf) {
                Self::test_and_set_bit(&mut buf, bit);
                self.commit_bitmap(&mut desc, &mut buf, valid_bits, true);
                self.write_block(bitmap_block, &buf)?;
                desc.clear_inode_bitmap_uninit();
                self.write_group_desc(group, &desc)?;

                let free_ino = desc.free_inodes_count().saturating_sub(1);
                desc.free_inodes_count_lo = free_ino as u16;
                desc.free_inodes_count_hi = (free_ino >> 16) as u16;
                if is_dir {
                    let used_dirs = desc.used_dirs_count().saturating_add(1);
                    desc.used_dirs_count_lo = used_dirs as u16;
                    desc.used_dirs_count_hi = (used_dirs >> 16) as u16;
                }
                self.write_group_desc(group, &desc)?;

                let mut sb = self.superblock.lock();
                sb.free_inodes_count = sb.free_inodes_count.saturating_sub(1);
                drop(sb);

                // Inodes are 1-based.
                let absolute_inode = (group * inodes_per_group) + bit as u32 + 1;
                return Ok(absolute_inode);
            }
        }
        crate::println!("ext4: alloc_inode failed (no space)");
        Err(())
    }

    /// Free a physical block by clearing its bit in the block bitmap.
    pub fn free_block(&self, absolute_block: u64) -> Result<(), ()> {
        let sb = self.superblock.lock();
        let blocks_per_group = sb.blocks_per_group;
        let first_data_block = sb.first_data_block;
        let groups = self.groups_count;
        drop(sb);

        if absolute_block < first_data_block as u64 {
            return Err(());
        }

        let relative = absolute_block - first_data_block as u64;
        let group = (relative / blocks_per_group as u64) as u32;
        let bit_idx = (relative % blocks_per_group as u64) as usize;

        let mut desc = self.read_group_desc(group)?;
        let bitmap_block = desc.block_bitmap();
        
        let mut buf = alloc::vec![0u8; self.block_size as usize];
        self.read_block(bitmap_block, &mut buf)?;
        
        let byte = bit_idx / 8;
        let bit = bit_idx % 8;
        
        if Self::test_and_clear_bit(&mut buf, byte * 8 + bit) {
            let valid_bits = if group + 1 == groups {
                let used = self.blocks_count().saturating_sub(group as u64 * blocks_per_group as u64) as u32;
                core::cmp::min(used, blocks_per_group)
            } else {
                blocks_per_group
            };
            self.commit_bitmap(&mut desc, &mut buf, valid_bits, false);
            self.write_block(bitmap_block, &buf)?;
            
            let free_bg = desc.free_blocks_count().saturating_add(1);
            desc.free_blocks_count_lo = free_bg as u16;
            desc.free_blocks_count_hi = (free_bg >> 16) as u16;
            self.write_group_desc(group, &desc)?;
            
            let mut sb = self.superblock.lock();
            let free_sb = sb.free_blocks_count().saturating_add(1);
            sb.free_blocks_count_lo = free_sb as u32;
            sb.free_blocks_count_hi = (free_sb >> 32) as u32;
            drop(sb);
        }
        Ok(())
    }

    /// Free an inode by clearing its bit in the inode bitmap.
    pub fn free_inode(&self, absolute_inode: u32, is_dir: bool) -> Result<(), ()> {
        let sb = self.superblock.lock();
        let inodes_per_group = sb.inodes_per_group;
        let groups = self.groups_count;
        drop(sb);

        if absolute_inode == 0 { return Err(()); }
        let relative = absolute_inode - 1;
        let group = relative / inodes_per_group;
        let bit_idx = (relative % inodes_per_group) as usize;

        let mut desc = self.read_group_desc(group)?;
        let bitmap_block = desc.inode_bitmap();
        
        let mut buf = alloc::vec![0u8; self.block_size as usize];
        self.read_block(bitmap_block, &mut buf)?;
        
        let byte = bit_idx / 8;
        let bit = bit_idx % 8;
        
        if Self::test_and_clear_bit(&mut buf, byte * 8 + bit) {
            let valid_bits = if group + 1 == groups {
                let used = self.inodes_count().saturating_sub(group as u64 * inodes_per_group as u64) as u32;
                core::cmp::min(used, inodes_per_group)
            } else {
                inodes_per_group
            };
            self.commit_bitmap(&mut desc, &mut buf, valid_bits, true);
            self.write_block(bitmap_block, &buf)?;
            
            let free_ino = desc.free_inodes_count().saturating_add(1);
            desc.free_inodes_count_lo = free_ino as u16;
            desc.free_inodes_count_hi = (free_ino >> 16) as u16;
            if is_dir {
                let used_dirs = desc.used_dirs_count().saturating_sub(1);
                desc.used_dirs_count_lo = used_dirs as u16;
                desc.used_dirs_count_hi = (used_dirs >> 16) as u16;
            }
            self.write_group_desc(group, &desc)?;
            
            let mut sb = self.superblock.lock();
            sb.free_inodes_count = sb.free_inodes_count.saturating_add(1);
            drop(sb);
        }
        Ok(())
    }

    // ── Inode table ──────────────────────────────────────────────────────

    /// Read an inode by number (1-based, as on disk).
    pub fn read_inode(&self, ino: u32) -> Result<Inode, ()> {
        let sb = self.superblock.lock();
        let inodes_per_group = sb.inodes_per_group;
        drop(sb);

        let group = (ino - 1) / inodes_per_group;
        let index = (ino - 1) % inodes_per_group;

        let desc = self.read_group_desc(group)?;
        let table_block = desc.inode_table();
        let byte_offset = index as u64 * self.inode_size as u64;
        let block_num = table_block + byte_offset / self.block_size as u64;
        let offset_in_block = (byte_offset % self.block_size as u64) as usize;

        let mut buf = alloc::vec![0u8; self.block_size as usize];
        self.read_block(block_num, &mut buf)?;

        let mut inode = Inode::default();
        let copy_size = core::cmp::min(self.inode_size as usize, core::mem::size_of::<Inode>());
        unsafe {
            core::ptr::copy_nonoverlapping(
                buf[offset_in_block..].as_ptr(),
                &mut inode as *mut Inode as *mut u8,
                copy_size,
            );
        }
        Ok(inode)
    }

    /// Write an inode back to the on-disk table.
    pub fn write_inode(&self, ino: u32, inode: &Inode) -> Result<(), ()> {
        let sb = self.superblock.lock();
        let inodes_per_group = sb.inodes_per_group;
        drop(sb);

        let group = (ino - 1) / inodes_per_group;
        let index = (ino - 1) % inodes_per_group;

        let desc = self.read_group_desc(group)?;
        let table_block = desc.inode_table();
        let byte_offset = index as u64 * self.inode_size as u64;
        let block_num = table_block + byte_offset / self.block_size as u64;
        let offset_in_block = (byte_offset % self.block_size as u64) as usize;

        let mut buf = alloc::vec![0u8; self.block_size as usize];
        self.read_block(block_num, &mut buf)?;

        let copy_size = core::cmp::min(self.inode_size as usize, core::mem::size_of::<Inode>());
        unsafe {
            core::ptr::copy_nonoverlapping(
                inode as *const Inode as *const u8,
                buf[offset_in_block..].as_mut_ptr(),
                copy_size,
            );
        }

        // Recompute the inode checksum over the full on-disk inode, which may
        // be larger than our `Inode` struct (bytes past the struct are
        // whatever we read back, and must still be covered).
        //
        // `ext2fs_inode_csum` zeroes i_checksum_lo (0x7C) and, when the inode
        // is long enough to have it, i_checksum_hi (0x82), then CRCs the whole
        // inode contiguously. The zeroed bytes are part of the hashed stream,
        // so they must be written as zeros rather than skipped.
        if self.has_metadata_csum() {
            let isize = self.inode_size as usize;
            if offset_in_block + isize <= buf.len() {
                let raw = &mut buf[offset_in_block..offset_in_block + isize];
                // i_extra_isize (u16 at 0x80) decides whether the high half of
                // the checksum is inside this inode at all.
                let extra_isize = u16::from_le_bytes([raw[0x80], raw[0x81]]) as usize;
                let has_hi = isize > EXT4_GOOD_OLD_INODE_SIZE && extra_isize >= 4;

                raw[0x7C] = 0;
                raw[0x7D] = 0;
                if has_hi {
                    raw[0x82] = 0;
                    raw[0x83] = 0;
                }

                let mut csum = self.object_csum_seed(ino, inode.generation);
                crc32c::crc32c(&mut csum, raw);

                raw[0x7C..0x7E].copy_from_slice(&(csum as u16).to_le_bytes());
                if has_hi {
                    raw[0x82..0x84].copy_from_slice(&((csum >> 16) as u16).to_le_bytes());
                }
            }
        }

        self.write_block(block_num, &buf)
    }

    // ── Extent-based block mapping ───────────────────────────────────────

    /// Translate a logical file block to a physical disk block for `inode`.
    ///
    /// Handles both extent-tree and classic indirect block map inodes
    /// (the latter via the ext2 fallback — not yet implemented for ext4).
    /// Maps a logical block of an inode to a physical block.
    ///
    /// `Ok(None)` is a **hole**: the block is not backed by anything and
    /// reads as zeros. `Err` is a **failed lookup**: the extent tree could
    /// not be walked (a block of the tree could not be read). The two must
    /// not be conflated: a failed lookup that is reported as a hole hands
    /// the reader zeros where the file's data should be, and a program that
    /// parses the result -- the dynamic loader, for one -- then walks off
    /// the end of a structure and dereferences a null pointer.
    pub fn logical_to_physical(&self, inode: &Inode, logical_block: u64) -> Result<Option<u64>, ()> {
        if inode.uses_extents() {
            // The inode `block` field (60 bytes) is the root of the extent tree.
            let root_data: [u8; 60] = unsafe {
                core::mem::transmute(inode.block)
            };
            let bs = self.block_size;
            let cache = self.cache.clone();
            let result = extents::lookup_extent(&root_data, logical_block, |blk| {
                let mut buf = alloc::vec![0u8; bs as usize];
                cache.read_block(blk as u32, &mut buf)?;
                Ok(buf)
            })
            .ok_or(())?;
            match result {
                extents::ExtentLookup::Mapped { phys_block, .. } => Ok(Some(phys_block)),
                extents::ExtentLookup::Hole { .. } => Ok(None),
            }
        } else {
            // Classic indirect block map (ext2/ext3 style).
            let p = self.block_size / 4; // pointers per block
            if logical_block < 12 {
                let phys = inode.block[logical_block as usize];
                if phys == 0 { Ok(None) } else { Ok(Some(phys as u64)) }
            } else {
                let mut rel_idx = (logical_block - 12) as u32;
                
                // A zero pointer is a hole; a failed block read is an error.
                let read_ptr = |block: u32, index: u32| -> Result<Option<u64>, ()> {
                    if block == 0 { return Ok(None); }
                    let mut buf = alloc::vec![0u8; self.block_size as usize];
                    self.read_block(block as u64, &mut buf)?;
                    let offset = index as usize * 4;
                    let ptr = u32::from_le_bytes(buf[offset..offset+4].try_into().unwrap());
                    if ptr == 0 { Ok(None) } else { Ok(Some(ptr as u64)) }
                };

                if rel_idx < p {
                    read_ptr(inode.block[12], rel_idx)
                } else {
                    rel_idx -= p;
                    if rel_idx < p * p {
                        let ind1 = read_ptr(inode.block[13], rel_idx / p)?.ok_or(())?;
                        read_ptr(ind1 as u32, rel_idx % p)
                    } else {
                        rel_idx -= p * p;
                        let ind1 = read_ptr(inode.block[14], rel_idx / (p * p))?.ok_or(())?;
                        let ind2 = read_ptr(ind1 as u32, (rel_idx / p) % p)?.ok_or(())?;
                        read_ptr(ind2 as u32, rel_idx % p)
                    }
                }
            }
        }
    }

    /// Allocate (if needed) the physical block for `logical_block` of a
    /// classic indirect-block-map inode (no EXTENTS flag), mirroring the
    /// ext2 driver: direct blocks 0-11 plus single indirect via block[12].
    /// Mutates `inode` in place; the caller persists it with write_inode.
    pub fn classic_get_or_alloc(&self, ino: u32, inode: &mut Inode, logical: u64) -> Result<u64, ()> {
        let bs = self.block_size as usize;
        let p = (bs / 4) as u64; // pointers per indirect block
        if logical < 12 {
            let i = logical as usize;
            if inode.block[i] == 0 {
                inode.block[i] = self.alloc_block()? as u32;
                inode.blocks_lo += (bs / 512) as u32;
                self.write_inode(ino, inode)?;
            }
            return Ok(inode.block[i] as u64);
        }
        let rel = logical - 12;
        if rel >= p {
            // Double/triple indirect allocation unsupported (same limit as ext2).
            return Err(());
        }
        if inode.block[12] == 0 {
            inode.block[12] = self.alloc_block()? as u32;
            inode.blocks_lo += (bs / 512) as u32;
            self.write_inode(ino, inode)?;
        }
        let indirect = inode.block[12];
        let mut buf = alloc::vec![0u8; bs];
        self.read_block(indirect as u64, &mut buf)?;
        let off = rel as usize * 4;
        let mut ptr = u32::from_le_bytes(buf[off..off + 4].try_into().map_err(|_| ())?);
        if ptr == 0 {
            ptr = self.alloc_block()? as u32;
            buf[off..off + 4].copy_from_slice(&ptr.to_le_bytes());
            self.write_block(indirect as u64, &buf)?;
            inode.blocks_lo += (bs / 512) as u32;
            self.write_inode(ino, inode)?;
        }
        Ok(ptr as u64)
    }

    /// Recursively free all blocks mapped in the extent tree.
    pub fn free_extent_tree(&self, inode: &Inode) -> Result<(), ()> {
        if !inode.uses_extents() { return Ok(()); }
        let root_data: [u8; 60] = unsafe { core::mem::transmute(inode.block) };
        self.free_extent_node(&root_data)
    }

    fn free_extent_node(&self, node_data: &[u8]) -> Result<(), ()> {
        let hdr = extents::read_header(node_data, 0);
        if hdr.magic != EXT4_EXT_MAGIC { return Err(()); }
        
        if hdr.depth == 0 {
            // Leaf node: free all extents
            for i in 0..hdr.entries as usize {
                let ext = extents::read_extent(node_data, extents::HEADER_SIZE + i * extents::ENTRY_SIZE);
                let start_phys = ((ext.start_hi as u64) << 32) | (ext.start_lo as u64);
                // The extent length is limited to 32768, so loop is fine
                for offset in 0..ext.len as u64 {
                    let _ = self.free_block(start_phys + offset);
                }
            }
        } else {
            // Index node: free all child nodes and then the blocks themselves
            for i in 0..hdr.entries as usize {
                let idx = extents::read_idx(node_data, extents::HEADER_SIZE + i * extents::ENTRY_SIZE);
                let child_phys = idx.leaf();
                
                let mut buf = alloc::vec![0u8; self.block_size as usize];
                if self.read_block(child_phys, &mut buf).is_ok() {
                    let _ = self.free_extent_node(&buf);
                }
                let _ = self.free_block(child_phys);
            }
        }
        Ok(())
    }

    /// Free all blocks mapped to an inode, supporting both extent trees and classic indirect mapping.
    pub fn free_all_inode_blocks(&self, inode: &mut Inode) -> Result<(), ()> {
        if inode.uses_extents() {
            self.free_extent_tree(inode)?;
            let hdr = ExtentHeader {
                magic: EXT4_EXT_MAGIC,
                entries: 0,
                max: 4,
                depth: 0,
                generation: 0,
            };
            unsafe {
                core::ptr::write_unaligned(
                    core::ptr::addr_of_mut!(inode.block) as *mut ExtentHeader,
                    hdr,
                );
            }
        } else {
            let bs = self.block_size as usize;
            for i in 0..12 {
                let blk = inode.block[i];
                if blk != 0 {
                    let _ = self.free_block(blk as u64);
                    inode.block[i] = 0;
                }
            }
            if inode.block[12] != 0 {
                let ind = inode.block[12];
                let mut ibuf = alloc::vec![0u8; bs];
                if self.read_block(ind as u64, &mut ibuf).is_ok() {
                    for chunk in ibuf.chunks_exact(4) {
                        let b = u32::from_le_bytes(chunk.try_into().unwrap_or([0; 4]));
                        if b != 0 {
                            let _ = self.free_block(b as u64);
                        }
                    }
                }
                let _ = self.free_block(ind as u64);
                inode.block[12] = 0;
            }
            if inode.block[13] != 0 {
                let dind = inode.block[13];
                let mut dbuf = alloc::vec![0u8; bs];
                if self.read_block(dind as u64, &mut dbuf).is_ok() {
                    for dchunk in dbuf.chunks_exact(4) {
                        let ind = u32::from_le_bytes(dchunk.try_into().unwrap_or([0; 4]));
                        if ind != 0 {
                            let mut ibuf = alloc::vec![0u8; bs];
                            if self.read_block(ind as u64, &mut ibuf).is_ok() {
                                for chunk in ibuf.chunks_exact(4) {
                                    let b = u32::from_le_bytes(chunk.try_into().unwrap_or([0; 4]));
                                    if b != 0 {
                                        let _ = self.free_block(b as u64);
                                    }
                                }
                            }
                            let _ = self.free_block(ind as u64);
                        }
                    }
                }
                let _ = self.free_block(dind as u64);
                inode.block[13] = 0;
            }
            if inode.block[14] != 0 {
                let tind = inode.block[14];
                let mut tbuf = alloc::vec![0u8; bs];
                if self.read_block(tind as u64, &mut tbuf).is_ok() {
                    for tchunk in tbuf.chunks_exact(4) {
                        let dind = u32::from_le_bytes(tchunk.try_into().unwrap_or([0; 4]));
                        if dind != 0 {
                            let mut dbuf = alloc::vec![0u8; bs];
                            if self.read_block(dind as u64, &mut dbuf).is_ok() {
                                for dchunk in dbuf.chunks_exact(4) {
                                    let ind = u32::from_le_bytes(dchunk.try_into().unwrap_or([0; 4]));
                                    if ind != 0 {
                                        let mut ibuf = alloc::vec![0u8; bs];
                                        if self.read_block(ind as u64, &mut ibuf).is_ok() {
                                            for chunk in ibuf.chunks_exact(4) {
                                                let b = u32::from_le_bytes(chunk.try_into().unwrap_or([0; 4]));
                                                if b != 0 {
                                                    let _ = self.free_block(b as u64);
                                                }
                                            }
                                        }
                                        let _ = self.free_block(ind as u64);
                                    }
                                }
                            }
                            let _ = self.free_block(dind as u64);
                        }
                    }
                }
                let _ = self.free_block(tind as u64);
                inode.block[14] = 0;
            }
        }
        inode.blocks_lo = 0;
        inode.size_lo = 0;
        inode.size_hi = 0;
        Ok(())
    }

    // ── Data reading ─────────────────────────────────────────────────────

    /// Read `count` bytes from `inode` starting at byte `offset`.
    ///
    /// Returns the bytes actually read (may be < count at EOF).
    pub fn read_inode_data(
        &self,
        inode: &Inode,
        offset: usize,
        buf: &mut [u8],
    ) -> Result<usize, ()> {
        let file_size = inode.size() as usize;
        if offset >= file_size {
            return Ok(0);
        }
        let count = core::cmp::min(buf.len(), file_size - offset);

        // Fast symlink: target stored directly in the inode's block array (up to 60 bytes).
        if inode.is_symlink() && file_size <= 60 {
            let root_data: [u8; 60] = unsafe { core::mem::transmute(inode.block) };
            buf[..count].copy_from_slice(&root_data[offset..offset + count]);
            return Ok(count);
        }

        // Inline data (data stored in inode body & xattrs).
        if (inode.flags & EXT4_INLINE_DATA_FL) != 0 {
            if file_size <= 60 {
                let root_data: [u8; 60] = unsafe { core::mem::transmute(inode.block) };
                buf[..count].copy_from_slice(&root_data[offset..offset + count]);
                return Ok(count);
            } else {
                crate::println!("ext4: large inline data reading not yet supported");
                return Err(());
            }
        }

        let bs = self.block_size as usize;

        let mut bytes_read = 0;
        let mut pos = offset;
        while bytes_read < count {
            let logical_block = (pos / bs) as u64;
            let offset_in_block = pos % bs;
            let chunk = core::cmp::min(bs - offset_in_block, count - bytes_read);

            if let Some(phys) = self.logical_to_physical(inode, logical_block)? {
                let mut block_buf = alloc::vec![0u8; bs];
                self.read_block(phys, &mut block_buf)?;
                buf[bytes_read..bytes_read + chunk]
                    .copy_from_slice(&block_buf[offset_in_block..offset_in_block + chunk]);
            } else {
                // Hole — fill with zeros.
                for b in &mut buf[bytes_read..bytes_read + chunk] {
                    *b = 0;
                }
            }

            bytes_read += chunk;
            pos += chunk;
        }
        Ok(bytes_read)
    }

    // ── Directory iteration ──────────────────────────────────────────────

    /// Iterate over directory entries in `inode`, calling `callback` for each.
    ///
    /// Returns `Ok(())` on success or `Err(())` on I/O error.
    pub fn iter_dir<F>(&self, inode: &Inode, mut callback: F) -> Result<(), ()>
    where
        F: FnMut(u32, &str, u8) -> bool, // (inode_num, name, file_type) -> continue?
    {
        if !inode.is_dir() {
            return Err(());
        }
        let size = inode.size() as usize;
        let bs = self.block_size as usize;
        let mut buf = alloc::vec![0u8; bs];

        let mut offset = 0;
        while offset < size {
            let chunk = core::cmp::min(bs, size - offset);
            self.read_inode_data(inode, offset, &mut buf[..chunk])?;

            let mut pos = 0;
            while pos + 8 <= chunk {
                let entry = unsafe {
                    core::ptr::read_unaligned(buf[pos..].as_ptr() as *const DirEntry)
                };
                if entry.rec_len == 0 {
                    break; // End of block or corrupt
                }
                if entry.inode != 0 && entry.name_len > 0 {
                    let name_end = pos + 8 + entry.name_len as usize;
                    if name_end <= chunk {
                        let name = core::str::from_utf8(&buf[pos + 8..name_end]).unwrap_or("");
                        if !callback(entry.inode, name, entry.file_type) {
                            return Ok(());
                        }
                    }
                }
                pos += entry.rec_len as usize;
            }
            offset += bs;
        }
        Ok(())
    }
}

// ── Per-file/directory node ──────────────────────────────────────────────────

pub struct Ext4Node {
    pub fs: Arc<Ext4Fs>,
    pub ino: u32,
    pub inode: SpinLock<Inode>,
}

impl Ext4Node {
    /// Open an inode by number, reading it from disk.
    pub fn new(fs: Arc<Ext4Fs>, ino: u32) -> Result<Self, ()> {
        let inode = fs.read_inode(ino)?;
        Ok(Self {
            fs,
            ino,
            inode: SpinLock::new(inode),
        })
    }

    pub(crate) fn add_entry(&self, name: &str, child_ino: u32, file_type: u8) -> Result<(), u64> {
        let parent = self.inode.lock();
        if !parent.is_dir() { return Err(crate::arch::x86_64::syscalls::ENOTDIR); }
        let bs = self.fs.block_size as usize;
        let parent_size = parent.size() as usize;
        let required_len = ((8 + name.len() + 3) & !3) as u16;
        let mut found_space = false;
        let mut pos = 0;
        let mut target_block_buf = alloc::vec![0u8; bs];
        while pos < parent_size {
            let logical_block = (pos / bs) as u64;
            if let Some(phys_block) = self
                .fs
                .logical_to_physical(&parent, logical_block)
                .map_err(|_| EIO)?
            {
                if self.fs.read_block(phys_block, &mut target_block_buf).is_ok() {
                    let mut block_pos = 0;
                    while block_pos < bs {
                        let mut entry = unsafe { core::ptr::read_unaligned(target_block_buf[block_pos..].as_ptr() as *const DirEntry) };
                        if entry.rec_len == 0 { break; }
                        // The trailing dir_entry_tail of a checksummed
                        // directory looks like free space (inode 0) but holds
                        // the block CRC. It is always the last 12 bytes of
                        // the block, so stop here rather than reusing it.
                        if entry.file_type == EXT4_FT_DIR_TAIL { break; }
                        let actual_len = if entry.inode == 0 { 0 } else { ((8 + entry.name_len as u16 + 3) & !3) };
                        if entry.rec_len >= actual_len + required_len {
                            if entry.inode != 0 {
                                let old_rec_len = entry.rec_len;
                                entry.rec_len = actual_len;
                                unsafe { core::ptr::write_unaligned(target_block_buf[block_pos..].as_mut_ptr() as *mut DirEntry, entry); }
                                block_pos += actual_len as usize;
                                let new_entry = DirEntry { inode: child_ino, rec_len: old_rec_len - actual_len, name_len: name.len() as u8, file_type };
                                unsafe { core::ptr::write_unaligned(target_block_buf[block_pos..].as_mut_ptr() as *mut DirEntry, new_entry); }
                                target_block_buf[block_pos + 8..block_pos + 8 + name.len()].copy_from_slice(name.as_bytes());
                            } else {
                                let old_rec_len = entry.rec_len;
                                let new_entry = DirEntry { inode: child_ino, rec_len: old_rec_len, name_len: name.len() as u8, file_type };
                                unsafe { core::ptr::write_unaligned(target_block_buf[block_pos..].as_mut_ptr() as *mut DirEntry, new_entry); }
                                target_block_buf[block_pos + 8..block_pos + 8 + name.len()].copy_from_slice(name.as_bytes());
                            }
                            self.fs.update_dir_block_csum(self.ino, &parent, &mut target_block_buf);
                            let _ = self.fs.write_block(phys_block, &target_block_buf);
                            found_space = true;
                            break;
                        }
                        block_pos += entry.rec_len as usize;
                    }
                }
            }
            if found_space { break; }
            pos += bs;
        }
        if !found_space {
            let mut new_block = alloc::vec![0u8; bs];
            // A checksummed directory reserves the last 12 bytes of every
            // block for the dir_entry_tail, so the entry stops short of it.
            let usable = if self.fs.has_metadata_csum() { bs - 12 } else { bs };
            let new_entry = DirEntry { inode: child_ino, rec_len: usable as u16, name_len: name.len() as u8, file_type };
            unsafe { core::ptr::write_unaligned(new_block.as_mut_ptr() as *mut DirEntry, new_entry); }
            new_block[8..8 + name.len()].copy_from_slice(name.as_bytes());
            self.fs.init_dir_block_tail(self.ino, &parent, &mut new_block);
            drop(parent);
            self.write_at(parent_size, &new_block)?;
        }
        Ok(())
    }

}

impl FileNode for Ext4Node {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, u64> {
        let inode = self.inode.lock();
        self.fs
            .read_inode_data(&inode, offset, buf)
            .map_err(|_| EIO)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, u64> {
        let mut inode = self.inode.lock();

        let to_write = buf.len();
        if to_write == 0 {
            return Ok(0);
        }

        let new_size = core::cmp::max(inode.size() as usize, offset + to_write);
        
        // Cannot easily support inline data expansion yet.
        if (inode.flags & EXT4_INLINE_DATA_FL) != 0 && new_size > 60 {
            return Err(EIO);
        }

        // Fast symlink overwrite
        if inode.is_symlink() && new_size <= 60 {
            let mut root_data: [u8; 60] = unsafe { core::mem::transmute(inode.block) };
            root_data[offset..offset + to_write].copy_from_slice(buf);
            inode.block = unsafe { core::mem::transmute(root_data) };
            inode.size_lo = new_size as u32;
            self.fs.write_inode(self.ino, &inode).map_err(|_| EIO)?;
            return Ok(to_write);
        }

        let mut current_offset = offset;
        let mut bytes_written = 0;
        let bs = self.fs.block_size as usize;

        let mut block_buf = alloc::vec![0u8; bs];

        while bytes_written < to_write {
            let logical_block = (current_offset / bs) as u64;
            let offset_in_block = current_offset % bs;
            let chunk = core::cmp::min(bs - offset_in_block, to_write - bytes_written);

            let phys_block = match self
                .fs
                .logical_to_physical(&inode, logical_block)
                .map_err(|_| EIO)?
            {
                Some(phys) => phys,
                None => {
                    // Need to allocate!
                    if !inode.uses_extents() {
                        // Classic indirect-block-map file (e.g. no-extents disk).
                        self.fs
                            .classic_get_or_alloc(self.ino, &mut inode, logical_block)
                            .map_err(|_| EIO)?
                    } else {
                        let new_phys = self.fs.alloc_block().map_err(|_| EIO)?;
                        let mut root_data: [u8; 60] = unsafe { core::mem::transmute(inode.block) };

                        extents::insert_extent(
                            &mut root_data,
                            logical_block,
                            new_phys,
                            bs,
                            |blk| {
                                let mut bbuf = alloc::vec![0u8; bs];
                                self.fs.read_block(blk, &mut bbuf).map(|_| bbuf)
                            },
                            |blk, data| self.fs.write_block(blk, data),
                            || self.fs.alloc_block()
                        ).map_err(|_| EIO)?;

                        inode.block = unsafe { core::mem::transmute(root_data) };
                        inode.blocks_lo += (bs / 512) as u32;
                        new_phys
                    }
                }
            };

            if chunk < bs {
                self.fs.read_block(phys_block, &mut block_buf).map_err(|_| EIO)?;
            }
            
            block_buf[offset_in_block..offset_in_block + chunk]
                .copy_from_slice(&buf[bytes_written..bytes_written + chunk]);
                
            self.fs.write_block(phys_block, &block_buf).map_err(|_| EIO)?;

            bytes_written += chunk;
            current_offset += chunk;
        }

        let now = current_time_sec();
        inode.mtime = now;
        inode.ctime = now;
        if new_size > inode.size() as usize {
            inode.size_lo = new_size as u32;
            inode.size_hi = (new_size >> 32) as u32;
        }
        
        self.fs.write_inode(self.ino, &inode).map_err(|_| EIO)?;
        Ok(bytes_written)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        let inode = self.inode.lock();
        if !inode.is_dir() {
            return Err(ENOTDIR);
        }

        let mut found_ino: Option<u32> = None;
        let _ = self.fs.iter_dir(&inode, |child_ino, child_name, _ft| {
            if child_name == name {
                found_ino = Some(child_ino);
                false // stop iterating
            } else {
                true
            }
        });

        match found_ino {
            Some(child_ino) => {
                let node = Ext4Node::new(self.fs.clone(), child_ino).map_err(|_| EIO)?;
                Ok(Arc::new(node))
            }
            None => Err(ENOENT),
        }
    }

    
    fn create(&self, name: &str, is_dir: bool, mode: u32, uid: u32, gid: u32) -> Result<Arc<dyn FileNode>, u64> {
        let mut parent = self.inode.lock();
        if !parent.is_dir() { return Err(ENOTDIR); }
        let child_ino = self.fs.alloc_inode(is_dir).map_err(|_| EIO)?;
        let mut child_inode = Inode::default();
        let now = current_time_sec();
        child_inode.atime = now;
        child_inode.mtime = now;
        child_inode.ctime = now;
        child_inode.crtime = now;
        // The caller's request, umasked by the syscall layer; the type bits
        // come from is_dir. Never a hardcoded 0755/0644 that ignores umask.
        let type_bits = if is_dir { EXT4_S_IFDIR } else { EXT4_S_IFREG };
        child_inode.mode = type_bits | mode as u16;
        child_inode.uid = uid as u16;
        child_inode.gid = gid as u16;
        child_inode.links_count = if is_dir { 2 } else { 1 };
        // ponytail: don't mark extents on a classic-mapping filesystem; fsck
        // errors otherwise ("extent format but missing EXTENTS feature").
        if self.fs.superblock.lock().has_extents() {
            child_inode.flags = EXT4_EXTENTS_FL;
            let hdr = ExtentHeader { magic: EXT4_EXT_MAGIC, entries: 0, max: 4, depth: 0, generation: 0 };
            unsafe { core::ptr::write_unaligned(core::ptr::addr_of_mut!(child_inode.block) as *mut ExtentHeader, hdr); }
        }
        self.fs.write_inode(child_ino, &child_inode).map_err(|_| EIO)?;
        if is_dir {
            // New directories start with "." and ".." in block 0 (fsck-clean,
            // and ".." lookups resolve). Written via write_at so extent and
            // classic disks share the path.
            let bs = self.fs.block_size as usize;
            let mut blk = alloc::vec![0u8; bs];
            // ".." stops short of the dir_entry_tail on a checksummed fs.
            let dotdot_len = if self.fs.has_metadata_csum() { bs - 12 } else { bs };
            write_dirent(&mut blk, 0, child_ino, 12, EXT4_FT_DIR, b".");
            write_dirent(&mut blk, 12, self.ino, dotdot_len as u16, EXT4_FT_DIR, b"..");
            self.fs.init_dir_block_tail(child_ino, &child_inode, &mut blk);
            let child_node = Ext4Node::new(self.fs.clone(), child_ino).map_err(|_| EIO)?;
            child_node.write_at(0, &blk).map_err(|_| EIO)?;
            // A subdirectory adds one link to its parent (.. entry).
            parent.links_count += 1;
            parent.mtime = now;
            parent.ctime = now;
            self.fs.write_inode(self.ino, &parent).map_err(|_| EIO)?;
        }
        drop(parent);

        self.add_entry(name, child_ino, if is_dir { EXT4_FT_DIR } else { EXT4_FT_REG_FILE })?;
        Ok(Arc::new(Ext4Node::new(self.fs.clone(), child_ino).map_err(|_| EIO)?))
    }

    

    /// Removes the child from the directory, decrements its link count, and frees it if 0.
    fn unlink(&self, name: &str) -> Result<(), u64> {
        let mut parent = self.inode.lock();
        if !parent.is_dir() { return Err(ENOTDIR); }
        
        let bs = self.fs.block_size as usize;
        let parent_size = parent.size() as usize;
        let mut pos = 0;
        let mut target_block_buf = alloc::vec![0u8; bs];
        
        while pos < parent_size {
            let logical_block = (pos / bs) as u64;
            if let Some(phys_block) = self
                .fs
                .logical_to_physical(&parent, logical_block)
                .map_err(|_| EIO)?
            {
                if self.fs.read_block(phys_block, &mut target_block_buf).is_ok() {
                    let mut block_pos = 0;
                    let mut prev_block_pos = None;
                    while block_pos < bs {
                        let mut entry = unsafe { core::ptr::read_unaligned(target_block_buf[block_pos..].as_ptr() as *const DirEntry) };
                        if entry.rec_len == 0 { break; }
                        
                        if entry.inode != 0 {
                            let entry_name = core::str::from_utf8(&target_block_buf[block_pos + 8..block_pos + 8 + entry.name_len as usize]).unwrap_or("");
                            if entry_name == name {
                                let child_ino = entry.inode;
                                let child_is_dir = entry.file_type == EXT4_FT_DIR;

                                let mut child_inode = self.fs.read_inode(child_ino).map_err(|_| EIO)?;
                                if child_is_dir {
                                    let mut has_entries = false;
                                    let _ = self.fs.iter_dir(&child_inode, |_ino, dname, _ft| {
                                        if dname != "." && dname != ".." {
                                            has_entries = true;
                                            false
                                        } else {
                                            true
                                        }
                                    });
                                    if has_entries {
                                        return Err(crate::arch::x86_64::syscalls::ENOTEMPTY);
                                    }
                                }

                                if let Some(prev_pos) = prev_block_pos {
                                    let mut prev_entry = unsafe { core::ptr::read_unaligned(target_block_buf[prev_pos..].as_ptr() as *const DirEntry) };
                                    prev_entry.rec_len += entry.rec_len;
                                    unsafe { core::ptr::write_unaligned(target_block_buf[prev_pos..].as_mut_ptr() as *mut DirEntry, prev_entry); }
                                } else {
                                    entry.inode = 0;
                                    unsafe { core::ptr::write_unaligned(target_block_buf[block_pos..].as_mut_ptr() as *mut DirEntry, entry); }
                                }
                                // Freeing an entry changes the block, so the
                                // dir_entry_tail CRC has to be redone. The tail
                                // itself is never the entry being removed (it
                                // has inode 0), so its position is unchanged.
                                self.fs.update_dir_block_csum(self.ino, &parent, &mut target_block_buf);
                                let _ = self.fs.write_block(phys_block, &target_block_buf);
                                
                                let now = current_time_sec();
                                parent.mtime = now;
                                parent.ctime = now;
                                if child_is_dir && parent.links_count > 2 {
                                    parent.links_count -= 1;
                                }
                                let _ = self.fs.write_inode(self.ino, &parent);
                                drop(parent);

                                // Update child inode
                                if child_is_dir {
                                    child_inode.links_count = 0;
                                } else if child_inode.links_count > 0 {
                                    child_inode.links_count -= 1;
                                }

                                if child_inode.links_count == 0 {
                                    child_inode.dtime = now;
                                    let _ = self.fs.free_all_inode_blocks(&mut child_inode);
                                    let _ = self.fs.write_inode(child_ino, &child_inode);
                                    let _ = self.fs.free_inode(child_ino, child_inode.is_dir());
                                } else {
                                    child_inode.ctime = now;
                                    let _ = self.fs.write_inode(child_ino, &child_inode);
                                }
                                return Ok(());
                            }
                        }
                        prev_block_pos = Some(block_pos);
                        block_pos += entry.rec_len as usize;
                    }
                }
            }
            pos += bs;
        }
        Err(ENOENT)
    }

    fn truncate(&self, size: usize) -> Result<(), u64> {
        use crate::arch::x86_64::syscalls::EISDIR;
        let mut inode = self.inode.lock();
        if inode.is_dir() {
            return Err(EISDIR);
        }
        // ponytail: inline-data/fast-symlink files only support shrink-to-fit
        // within their 60-byte bodies; larger grows go through write_at.
        let cur = inode.size() as usize;
        if size == cur {
            return Ok(());
        }
        let bs = self.fs.block_size as usize;
        if size > cur {
            // Grow: unmapped blocks read back as zeros, but the tail of the
            // last partially-used block may hold stale bytes past old EOF —
            // zero that range so the extension reads as zeros (Linux ftruncate
            // semantics). At most one block, no new allocation.
            let tail_end = core::cmp::min(size, cur.div_ceil(bs).saturating_mul(bs));
            let tail_map = if tail_end > cur {
                let lb = (cur / bs) as u64;
                self.fs
                    .logical_to_physical(&inode, lb)
                    .map_err(|_| EIO)?
                    .map(|p| (p, cur % bs, tail_end % bs))
            } else {
                None
            };
            if let Some((phys, from, to)) = tail_map {
                let to = if to == 0 { bs } else { to };
                drop(inode);
                let mut zbuf = alloc::vec![0u8; bs];
                self.fs.read_block(phys, &mut zbuf).map_err(|_| EIO)?;
                for b in &mut zbuf[from..to] {
                    *b = 0;
                }
                self.fs.write_block(phys, &zbuf).map_err(|_| EIO)?;
                inode = self.inode.lock();
            }
            let now = current_time_sec();
            inode.mtime = now;
            inode.ctime = now;
            inode.size_lo = size as u32;
            inode.size_hi = (size >> 32) as u32;
            self.fs.write_inode(self.ino, &inode).map_err(|_| EIO)?;
            return Ok(());
        }
        // Shrink: free every mapped block at logical index >= keep_blocks,
        // then drop the mappings so freed blocks can be safely reallocated.
        let keep = size.div_ceil(bs) as u64;
        let cur_blocks = cur.div_ceil(bs) as u64;
        // Snapshot survivors first (logical_to_physical borrows nothing).
        let mut survivors = alloc::vec::Vec::new();
        for lb in 0..keep {
            if let Some(phys) = self.fs.logical_to_physical(&inode, lb).map_err(|_| EIO)? {
                survivors.push((lb, phys));
            }
        }
        for lb in keep..cur_blocks {
            if let Some(phys) = self.fs.logical_to_physical(&inode, lb).map_err(|_| EIO)? {
                let _ = self.fs.free_block(phys);
                inode.blocks_lo = inode.blocks_lo.saturating_sub((bs / 512) as u32);
            }
        }
        if inode.uses_extents() {
            // Collect and free structural (index/leaf) blocks of depth > 0 tree
            let old_root_data: [u8; 60] = unsafe { core::mem::transmute(inode.block) };
            let structural_blocks = extents::collect_structural_blocks(&old_root_data, |blk| {
                let mut buf = alloc::vec![0u8; bs];
                self.fs.read_block(blk, &mut buf).map(|_| buf)
            });
            for s_blk in structural_blocks {
                let _ = self.fs.free_block(s_blk);
                inode.blocks_lo = inode.blocks_lo.saturating_sub((bs / 512) as u32);
            }

            // Rebuild a fresh leaf tree containing only survivors (reuses the
            // tested insert path; depth collapses back to 0 when it fits).
            let hdr = ExtentHeader {
                magic: EXT4_EXT_MAGIC,
                entries: 0,
                max: 4,
                depth: 0,
                generation: 0,
            };
            unsafe {
                core::ptr::write_unaligned(
                    core::ptr::addr_of_mut!(inode.block) as *mut ExtentHeader,
                    hdr,
                );
            }
            let mut root_data: [u8; 60] = unsafe { core::mem::transmute(inode.block) };
            for (lb, phys) in survivors {
                let r = extents::insert_extent(
                    &mut root_data,
                    lb,
                    phys,
                    bs,
                    |blk| {
                        let mut bbuf = alloc::vec![0u8; bs];
                        self.fs.read_block(blk, &mut bbuf).map(|_| bbuf)
                    },
                    |blk, data| self.fs.write_block(blk, data),
                    || self.fs.alloc_block(),
                );
                if r.is_err() {
                    break;
                }
            }
            inode.block = unsafe { core::mem::transmute(root_data) };
        } else {
            // Classic: clear direct pointers and indirect entries >= keep.
            for lb in keep..cur_blocks {
                if lb < 12 {
                    let blk = inode.block[lb as usize];
                    if blk != 0 {
                        let _ = self.fs.free_block(blk as u64);
                        inode.block[lb as usize] = 0;
                        inode.blocks_lo = inode.blocks_lo.saturating_sub((bs / 512) as u32);
                    }
                } else {
                    let rel = (lb - 12) as usize;
                    let p = bs / 4;
                    if rel < p && inode.block[12] != 0 {
                        let ind = inode.block[12];
                        let mut ibuf = alloc::vec![0u8; bs];
                        if self.fs.read_block(ind as u64, &mut ibuf).is_ok() {
                            let blk = u32::from_le_bytes(ibuf[rel * 4..rel * 4 + 4].try_into().unwrap_or([0; 4]));
                            if blk != 0 {
                                let _ = self.fs.free_block(blk as u64);
                                inode.blocks_lo = inode.blocks_lo.saturating_sub((bs / 512) as u32);
                            }
                            ibuf[rel * 4..rel * 4 + 4].copy_from_slice(&0u32.to_le_bytes());
                            let _ = self.fs.write_block(ind as u64, &ibuf);
                        }
                    }
                }
            }
            // Free the indirect block itself when fully emptied.
            if keep <= 12 && inode.block[12] != 0 {
                let ind = inode.block[12];
                let mut ibuf = alloc::vec![0u8; bs];
                let mut used = false;
                if self.fs.read_block(ind as u64, &mut ibuf).is_ok() {
                    for chunk in ibuf.chunks_exact(4) {
                        if u32::from_le_bytes(chunk.try_into().unwrap_or([0; 4])) != 0 {
                            used = true;
                            break;
                        }
                    }
                }
                if !used {
                    let _ = self.fs.free_block(ind as u64);
                    inode.block[12] = 0;
                    inode.blocks_lo = inode.blocks_lo.saturating_sub((bs / 512) as u32);
                }
            }
        }
        let now = current_time_sec();
        inode.mtime = now;
        inode.ctime = now;
        inode.size_lo = size as u32;
        inode.size_hi = (size >> 32) as u32;
        self.fs.write_inode(self.ino, &inode).map_err(|_| EIO)?;
        Ok(())
    }

    fn stat(&self) -> Stat {
        let inode = self.inode.lock();
        let mut st = Stat::default();
        st.st_ino = self.ino as u64;
        st.st_mode = inode.mode as u32;
        st.st_nlink = inode.links_count as u64;
        st.st_uid = inode.uid as u32;
        st.st_gid = inode.gid as u32;
        st.st_size = inode.size() as i64;
        st.st_blksize = self.fs.block_size as i64;
        st.st_blocks = inode.blocks() as i64;
        st.st_atime = inode.atime_64();
        st.st_mtime = inode.mtime_64();
        st.st_ctime = inode.ctime_64();
        st
    }

    fn is_dir(&self) -> bool {
        self.inode.lock().is_dir()
    }

    fn size(&self) -> usize {
        self.inode.lock().size() as usize
    }

    fn list_dir(&self) -> Result<Vec<(alloc::string::String, bool)>, u64> {
        let inode = self.inode.lock();
        if !inode.is_dir() {
            return Err(ENOTDIR);
        }

        let mut entries = Vec::new();
        let result = self.fs.iter_dir(&inode, |child_ino, name, file_type| {
            if name == "." || name == ".." {
                return true;
            }
            let is_dir = if file_type == EXT4_FT_UNKNOWN {
                // Fallback for filesystems without the FILETYPE feature.
                if let Ok(child_inode) = self.fs.read_inode(child_ino) {
                    child_inode.is_dir()
                } else {
                    false
                }
            } else {
                file_type == EXT4_FT_DIR
            };
            entries.push((alloc::string::String::from(name), is_dir));
            true
        });
        result.map_err(|_| EIO)?;
        Ok(entries)
    }

    


    fn rename(&self, new_name: &str) -> Result<(), u64> {
        // Renaming in the same directory (Ext4Node represents the parent dir here? No, Ext4Node represents the file!)
        // Wait, VFS rename is usually called on the parent directory `dir.rename(old_name, new_name)` or on the file itself?
        // DragOS vfs.rs doesn't specify. The prompt had `fn rename(&self, new_name: &str)`. 
        // This implies `self` is the file, and we rename it? That requires knowing the parent directory!
        // VFS `rename` is usually `parent.rename(old, new)`. Let's assume VFS does `parent.rename(old, new)`
        Err(crate::arch::x86_64::syscalls::ENOSYS)
    }

    fn link(&self, name: &str, target: alloc::sync::Arc<dyn crate::fs::vfs::FileNode>) -> Result<(), u64> {
        let target_ino = target.stat().st_ino as u32;
        if target_ino == 0 { return Err(crate::arch::x86_64::syscalls::EINVAL); }
        
        // Increment link count on target
        let mut target_inode = self.fs.read_inode(target_ino).map_err(|_| crate::arch::x86_64::syscalls::EIO)?;
        target_inode.links_count += 1;
        self.fs.write_inode(target_ino, &target_inode).map_err(|_| crate::arch::x86_64::syscalls::EIO)?;
        
        let file_type = if target.is_dir() { EXT4_FT_DIR } else { EXT4_FT_REG_FILE };
        self.add_entry(name, target_ino, file_type)?;
        Ok(())
    }

    fn symlink(&self, name: &str, target_path: &str) -> Result<alloc::sync::Arc<dyn crate::fs::vfs::FileNode>, u64> {
        let child_ino = self.fs.alloc_inode(false).map_err(|_| EIO)?;
        let mut child_inode = Inode::default();
        child_inode.mode = EXT4_S_IFLNK | 0o777;
        child_inode.links_count = 1;
        child_inode.size_lo = target_path.len() as u32;
        child_inode.flags = EXT4_EXTENTS_FL;

        if target_path.len() <= 60 {
            // Fast symlink
            unsafe {
                core::ptr::copy_nonoverlapping(target_path.as_ptr(), core::ptr::addr_of_mut!(child_inode.block) as *mut u8, target_path.len());
            }
            self.fs.write_inode(child_ino, &child_inode).map_err(|_| EIO)?;
        } else {
            // Slow symlink
            let hdr = ExtentHeader { magic: EXT4_EXT_MAGIC, entries: 0, max: 4, depth: 0, generation: 0 };
            unsafe { core::ptr::write_unaligned(core::ptr::addr_of_mut!(child_inode.block) as *mut ExtentHeader, hdr); }
            self.fs.write_inode(child_ino, &child_inode).map_err(|_| EIO)?;
            let child_node = Ext4Node::new(self.fs.clone(), child_ino).map_err(|_| EIO)?;
            child_node.write_at(0, target_path.as_bytes())?;
        }

        self.add_entry(name, child_ino, EXT4_FT_SYMLINK)?;
        Ok(alloc::sync::Arc::new(Ext4Node::new(self.fs.clone(), child_ino).map_err(|_| EIO)?))
    }

    fn chmod(&self, mode: u32) -> Result<(), u64> {
        let mut inode = self.inode.lock();
        inode.mode = (inode.mode & !0o7777) | (mode as u16 & 0o7777);
        inode.ctime = current_time_sec();
        self.fs.write_inode(self.ino, &inode).map_err(|_| crate::arch::x86_64::syscalls::EIO)
    }

    fn chown(&self, uid: u32, gid: u32) -> Result<(), u64> {
        let mut inode = self.inode.lock();
        inode.uid = uid as u16;
        inode.gid = gid as u16;
        inode.ctime = current_time_sec();
        self.fs.write_inode(self.ino, &inode).map_err(|_| crate::arch::x86_64::syscalls::EIO)
    }

    fn read_link(&self) -> Result<alloc::string::String, u64> {
        // ponytail: readlink on non-symlinks must fail EINVAL — without this
        // it returned file content, breaking fontconfig (it readlinks configs).
        if !self.inode.lock().is_symlink() {
            return Err(crate::arch::x86_64::syscalls::EINVAL);
        }
        let size = self.size();
        let mut buf = alloc::vec![0u8; size];
        let n = self.read_at(0, &mut buf).map_err(|_| crate::arch::x86_64::syscalls::EIO)?;
        buf.truncate(n);
        alloc::string::String::from_utf8(buf).map_err(|_| crate::arch::x86_64::syscalls::EINVAL)
    }

    fn cache_id(&self) -> u64 {
        // Unique across filesystems, and within one filesystem the inode
        // number is unique, so the pair identifies the file.
        (self.fs.fs_id << 32) | self.ino as u64
    }
}
