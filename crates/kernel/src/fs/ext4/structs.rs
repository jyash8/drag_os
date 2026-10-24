// ext4 on-disk structures
//
// References:
//   https://ext4.wiki.kernel.org/index.php/Ext4_Disk_Layout
//   Linux kernel fs/ext4/ext4.h
//
// All multi-byte fields are little-endian on disk. The `#[repr(C, packed)]`
// attribute ensures the compiler does not insert padding, matching the exact
// on-disk byte layout.

#![allow(unused)]

// ── Magic & well-known inode numbers ─────────────────────────────────────────

pub const EXT4_SUPER_MAGIC: u16 = 0xEF53; // same as ext2/ext3
pub const EXT4_INODE_ROOT: u32 = 2;
pub const EXT4_INODE_JOURNAL: u32 = 8;
pub const EXT4_INODE_RESIZE: u32 = 7;

// ── Feature flags ────────────────────────────────────────────────────────────

// Compat features (backward-compatible; unknown ones are safe to mount)
pub const EXT4_FEATURE_COMPAT_DIR_PREALLOC: u32  = 0x0001;
pub const EXT4_FEATURE_COMPAT_HAS_JOURNAL: u32   = 0x0004;
pub const EXT4_FEATURE_COMPAT_EXT_ATTR: u32      = 0x0008;
pub const EXT4_FEATURE_COMPAT_RESIZE_INODE: u32  = 0x0010;
pub const EXT4_FEATURE_COMPAT_DIR_INDEX: u32     = 0x0020;
pub const EXT4_FEATURE_COMPAT_SPARSE_SUPER2: u32 = 0x0200;

// Incompat features (must understand these to mount)
pub const EXT4_FEATURE_INCOMPAT_FILETYPE: u32    = 0x0002;
pub const EXT4_FEATURE_INCOMPAT_RECOVER: u32     = 0x0004;
pub const EXT4_FEATURE_INCOMPAT_JOURNAL_DEV: u32 = 0x0008;
pub const EXT4_FEATURE_INCOMPAT_META_BG: u32     = 0x0010;
pub const EXT4_FEATURE_INCOMPAT_EXTENTS: u32     = 0x0040;
pub const EXT4_FEATURE_INCOMPAT_64BIT: u32       = 0x0080;
pub const EXT4_FEATURE_INCOMPAT_FLEX_BG: u32     = 0x0200;
pub const EXT4_FEATURE_INCOMPAT_EA_INODE: u32    = 0x0400;
pub const EXT4_FEATURE_INCOMPAT_CSUM_SEED: u32   = 0x2000;
pub const EXT4_FEATURE_INCOMPAT_LARGEDIR: u32    = 0x4000;
pub const EXT4_FEATURE_INCOMPAT_INLINE_DATA: u32 = 0x8000;

// Read-only-compat features (mount read-only if unknown)
pub const EXT4_FEATURE_RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const EXT4_FEATURE_RO_COMPAT_LARGE_FILE: u32   = 0x0002;
pub const EXT4_FEATURE_RO_COMPAT_HUGE_FILE: u32    = 0x0008;
pub const EXT4_FEATURE_RO_COMPAT_GDT_CSUM: u32     = 0x0010;
pub const EXT4_FEATURE_RO_COMPAT_DIR_NLINK: u32    = 0x0020;
pub const EXT4_FEATURE_RO_COMPAT_EXTRA_ISIZE: u32  = 0x0040;
pub const EXT4_FEATURE_RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

// Block group descriptor flag bits (bg_flags)
pub const EXT4_BG_INODE_UNINIT: u16  = 0x0001;
pub const EXT4_BG_BLOCK_UNINIT: u16  = 0x0002;
pub const EXT4_BG_INODE_ZEROED: u16  = 0x0004;

// ── Inode mode constants (identical to ext2, POSIX) ──────────────────────────

pub const EXT4_S_IFMT:  u16 = 0xF000;
pub const EXT4_S_IFSOCK: u16 = 0xC000;
pub const EXT4_S_IFLNK:  u16 = 0xA000;
pub const EXT4_S_IFREG:  u16 = 0x8000;
pub const EXT4_S_IFBLK:  u16 = 0x6000;
pub const EXT4_S_IFDIR:  u16 = 0x4000;
pub const EXT4_S_IFCHR:  u16 = 0x2000;
pub const EXT4_S_IFIFO:  u16 = 0x1000;

// ── Inode flags ──────────────────────────────────────────────────────────────

pub const EXT4_EXTENTS_FL: u32 = 0x00080000; // Inode uses extents
pub const EXT4_INLINE_DATA_FL: u32 = 0x10000000;
pub const EXT4_ENCRYPT_FL: u32 = 0x00000800;

// ── Extent tree constants ────────────────────────────────────────────────────

pub const EXT4_EXT_MAGIC: u16 = 0xF30A;
pub const EXT4_EXT_MAX_DEPTH: u16 = 5;

// ── Directory entry file types ───────────────────────────────────────────────

pub const EXT4_FT_UNKNOWN: u8  = 0;
pub const EXT4_FT_REG_FILE: u8 = 1;
pub const EXT4_FT_DIR: u8      = 2;
pub const EXT4_FT_CHRDEV: u8   = 3;
pub const EXT4_FT_BLKDEV: u8   = 4;
pub const EXT4_FT_FIFO: u8     = 5;
pub const EXT4_FT_SOCK: u8     = 6;
pub const EXT4_FT_SYMLINK: u8  = 7;
/// Pseudo file type of the 12-byte `dir_entry_tail` that terminates every
/// block of a checksummed directory. Never a real entry — it carries the
/// block's CRC and must be preserved verbatim when editing a directory.
pub const EXT4_FT_DIR_TAIL: u8 = 0xDE;

// ── Superblock (1024 bytes at offset 1024) ───────────────────────────────────
//
// The first 236 bytes are identical to ext2. Fields after byte 236 are
// ext3/ext4 extensions. We define the full 1024-byte structure.

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Superblock {
    // ── ext2 base (bytes 0–83) ───────────────────────────────────────────
    pub inodes_count: u32,          // 0x00
    pub blocks_count_lo: u32,       // 0x04
    pub r_blocks_count_lo: u32,     // 0x08
    pub free_blocks_count_lo: u32,  // 0x0C
    pub free_inodes_count: u32,     // 0x10
    pub first_data_block: u32,      // 0x14
    pub log_block_size: u32,        // 0x18  (block_size = 1024 << this)
    pub log_cluster_size: u32,      // 0x1C
    pub blocks_per_group: u32,      // 0x20
    pub clusters_per_group: u32,    // 0x24
    pub inodes_per_group: u32,      // 0x28
    pub mtime: u32,                 // 0x2C
    pub wtime: u32,                 // 0x30
    pub mnt_count: u16,             // 0x34
    pub max_mnt_count: i16,         // 0x36
    pub magic: u16,                 // 0x38  must be 0xEF53
    pub state: u16,                 // 0x3A
    pub errors: u16,                // 0x3C
    pub minor_rev_level: u16,       // 0x3E
    pub lastcheck: u32,             // 0x40
    pub checkinterval: u32,         // 0x44
    pub creator_os: u32,            // 0x48
    pub rev_level: u32,             // 0x4C
    pub def_resuid: u16,            // 0x50
    pub def_resgid: u16,            // 0x52

    // ── ext2 dynamic rev (bytes 84–235) ──────────────────────────────────
    pub first_ino: u32,             // 0x54
    pub inode_size: u16,            // 0x58
    pub block_group_nr: u16,        // 0x5A
    pub feature_compat: u32,        // 0x5C
    pub feature_incompat: u32,      // 0x60
    pub feature_ro_compat: u32,     // 0x64
    pub uuid: [u8; 16],             // 0x68
    pub volume_name: [u8; 16],      // 0x78
    pub last_mounted: [u8; 64],     // 0x88
    pub algo_bitmap: u32,           // 0xC8
    pub prealloc_blocks: u8,        // 0xCC
    pub prealloc_dir_blocks: u8,    // 0xCD
    pub reserved_gdt_blocks: u16,   // 0xCE

    // ── ext3 journal (bytes 236–263) ─────────────────────────────────────
    pub journal_uuid: [u8; 16],     // 0xD0
    pub journal_inum: u32,          // 0xE0
    pub journal_dev: u32,           // 0xE4
    pub last_orphan: u32,           // 0xE8
    pub hash_seed: [u32; 4],        // 0xEC
    pub def_hash_version: u8,       // 0xFC
    pub jnl_backup_type: u8,       // 0xFD
    pub desc_size: u16,             // 0xFE  (block group descriptor size)

    // ── ext4 extensions (bytes 264–1023) ─────────────────────────────────
    pub default_mount_opts: u32,    // 0x100
    pub first_meta_bg: u32,         // 0x104
    pub mkfs_time: u32,             // 0x108
    pub jnl_blocks: [u32; 17],      // 0x10C

    // 64-bit support
    pub blocks_count_hi: u32,       // 0x150
    pub r_blocks_count_hi: u32,     // 0x154
    pub free_blocks_count_hi: u32,  // 0x158
    pub min_extra_isize: u16,       // 0x15C
    pub want_extra_isize: u16,      // 0x15E
    pub flags: u32,                 // 0x160
    pub raid_stride: u16,           // 0x164
    pub mmp_interval: u16,          // 0x166
    pub mmp_block: u64,             // 0x168
    pub raid_stripe_width: u32,     // 0x170
    pub log_groups_per_flex: u8,    // 0x174
    pub checksum_type: u8,          // 0x175
    pub reserved_pad: u16,          // 0x176
    pub kbytes_written: u64,        // 0x178
    pub snapshot_inum: u32,         // 0x180
    pub snapshot_id: u32,           // 0x184
    pub snapshot_r_blocks_count: u64, // 0x188
    pub snapshot_list: u32,         // 0x190
    pub error_count: u32,           // 0x194
    pub first_error_time: u32,      // 0x198
    pub first_error_ino: u32,       // 0x19C
    pub first_error_block: u64,     // 0x1A0
    pub first_error_func: [u8; 32], // 0x1A8
    pub first_error_line: u32,      // 0x1C8
    pub last_error_time: u32,       // 0x1CC
    pub last_error_ino: u32,        // 0x1D0
    pub last_error_line: u32,       // 0x1D4
    pub last_error_block: u64,      // 0x1D8
    pub last_error_func: [u8; 32],  // 0x1E0
    pub mount_opts: [u8; 64],       // 0x200
    pub usr_quota_inum: u32,        // 0x240
    pub grp_quota_inum: u32,        // 0x244
    pub overhead_blocks: u32,       // 0x248
    pub backup_bgs: [u32; 2],       // 0x24C
    pub encrypt_algos: [u8; 4],     // 0x254
    pub encrypt_pw_salt: [u8; 16],  // 0x258
    pub lpf_ino: u32,               // 0x268
    pub prj_quota_inum: u32,        // 0x26C
    pub checksum_seed: u32,         // 0x270
    pub reserved: [u32; 98],        // 0x274  pad to 1024 bytes
    pub checksum: u32,              // 0x3FC  crc32c of superblock
}

impl Superblock {
    /// Block size in bytes (1024 << log_block_size).
    pub fn block_size(&self) -> u32 {
        1024 << self.log_block_size
    }

    /// Total number of blocks (64-bit combined).
    pub fn blocks_count(&self) -> u64 {
        (self.blocks_count_hi as u64) << 32 | self.blocks_count_lo as u64
    }

    /// Free block count (64-bit combined).
    pub fn free_blocks_count(&self) -> u64 {
        (self.free_blocks_count_hi as u64) << 32 | self.free_blocks_count_lo as u64
    }

    /// Whether the filesystem uses 64-bit block addressing.
    pub fn is_64bit(&self) -> bool {
        (self.feature_incompat & EXT4_FEATURE_INCOMPAT_64BIT) != 0
    }

    /// Whether the filesystem uses extents (rather than indirect block maps).
    pub fn has_extents(&self) -> bool {
        (self.feature_incompat & EXT4_FEATURE_INCOMPAT_EXTENTS) != 0
    }

    /// Whether the filesystem has a journal.
    pub fn has_journal(&self) -> bool {
        (self.feature_compat & EXT4_FEATURE_COMPAT_HAS_JOURNAL) != 0
    }

    /// Whether the filesystem has flex_bg (flexible block groups).
    pub fn has_flex_bg(&self) -> bool {
        (self.feature_incompat & EXT4_FEATURE_INCOMPAT_FLEX_BG) != 0
    }

    /// Whether the filesystem uses metadata checksums.
    pub fn has_metadata_csum(&self) -> bool {
        (self.feature_ro_compat & EXT4_FEATURE_RO_COMPAT_METADATA_CSUM) != 0
    }

    /// Whether each inode derives its own checksum seed from its inode number
    /// and generation (INCOMPAT_CSUM_SEED). When clear, the superblock's
    /// single `checksum_seed` is used for all inodes.
    pub fn has_metadata_csum_seed(&self) -> bool {
        (self.feature_incompat & EXT4_FEATURE_INCOMPAT_CSUM_SEED) != 0
    }

    /// Block group descriptor size. 32 bytes for ext2/ext3, 64 for ext4 64-bit.
    pub fn group_desc_size(&self) -> u32 {
        if self.is_64bit() && self.desc_size >= 64 {
            self.desc_size as u32
        } else {
            32
        }
    }

    /// Number of block groups.
    pub fn group_count(&self) -> u32 {
        let blocks = self.blocks_count();
        let bpg = self.blocks_per_group as u64;
        ((blocks + bpg - 1) / bpg) as u32
    }
}

// ── Block Group Descriptor ───────────────────────────────────────────────────
//
// 32 bytes in ext2/ext3, extended to 64 bytes in ext4 with 64-bit mode.

#[repr(C, packed)]
#[derive(Debug, Default, Clone, Copy)]
pub struct BlockGroupDesc {
    // ── 32-byte ext2/ext3 base ───────────────────────────────────────────
    pub block_bitmap_lo: u32,       // 0x00
    pub inode_bitmap_lo: u32,       // 0x04
    pub inode_table_lo: u32,        // 0x08
    pub free_blocks_count_lo: u16,  // 0x0C
    pub free_inodes_count_lo: u16,  // 0x0E
    pub used_dirs_count_lo: u16,    // 0x10
    pub flags: u16,                 // 0x12
    pub exclude_bitmap_lo: u32,     // 0x14
    pub block_bitmap_csum_lo: u16,  // 0x18
    pub inode_bitmap_csum_lo: u16,  // 0x1A
    pub itable_unused_lo: u16,      // 0x1C
    pub checksum: u16,              // 0x1E

    // ── 64-bit ext4 extensions (bytes 32–63) ─────────────────────────────
    pub block_bitmap_hi: u32,       // 0x20
    pub inode_bitmap_hi: u32,       // 0x24
    pub inode_table_hi: u32,        // 0x28
    pub free_blocks_count_hi: u16,  // 0x2C
    pub free_inodes_count_hi: u16,  // 0x2E
    pub used_dirs_count_hi: u16,    // 0x30
    pub itable_unused_hi: u16,      // 0x32
    pub exclude_bitmap_hi: u32,     // 0x34
    pub block_bitmap_csum_hi: u16,  // 0x38
    pub inode_bitmap_csum_hi: u16,  // 0x3A
    pub reserved: u32,              // 0x3C
}

impl BlockGroupDesc {
    /// Block bitmap physical block (64-bit combined).
    pub fn block_bitmap(&self) -> u64 {
        (self.block_bitmap_hi as u64) << 32 | self.block_bitmap_lo as u64
    }

    /// Inode bitmap physical block (64-bit combined).
    pub fn inode_bitmap(&self) -> u64 {
        (self.inode_bitmap_hi as u64) << 32 | self.inode_bitmap_lo as u64
    }

    /// Inode table starting block (64-bit combined).
    pub fn inode_table(&self) -> u64 {
        (self.inode_table_hi as u64) << 32 | self.inode_table_lo as u64
    }

    /// Free blocks in this group (32-bit combined).
    pub fn free_blocks_count(&self) -> u32 {
        (self.free_blocks_count_hi as u32) << 16 | self.free_blocks_count_lo as u32
    }

    /// Free inodes in this group (32-bit combined).
    pub fn free_inodes_count(&self) -> u32 {
        (self.free_inodes_count_hi as u32) << 16 | self.free_inodes_count_lo as u32
    }

    /// Used directories in this group (32-bit combined).
    pub fn used_dirs_count(&self) -> u32 {
        (self.used_dirs_count_hi as u32) << 16 | self.used_dirs_count_lo as u32
    }

    /// Unused inodes at the top of this group's inode table.
    pub fn itable_unused(&self) -> u32 {
        (self.itable_unused_hi as u32) << 16 | self.itable_unused_lo as u32
    }

    /// Set the low 16 bits of the inode table's unused-inode count.
    pub fn set_itable_unused_lo(&mut self, v: u16) {
        self.itable_unused_lo = v;
    }

    /// Set the high 16 bits of the inode table's unused-inode count.
    pub fn set_itable_unused_hi(&mut self, v: u16) {
        self.itable_unused_hi = v;
    }

    /// Whether the group descriptor's block bitmap is not yet initialised.
    /// With `uninit_bg` the bitmap reads as all-zero on disk and the real
    /// allocation state lives in `free_blocks_count`.
    pub fn block_bitmap_uninit(&self) -> bool {
        (self.flags & EXT4_BG_BLOCK_UNINIT) != 0
    }

    /// Whether the group descriptor's inode bitmap is not yet initialised.
    pub fn inode_bitmap_uninit(&self) -> bool {
        (self.flags & EXT4_BG_INODE_UNINIT) != 0
    }

    /// Clear EXT4_BG_BLOCK_UNINIT / EXT4_BG_INODE_UNINIT respectively. Once a
    /// bitmap is touched its real contents are on disk, so the flag must go
    /// or e2fsck will keep treating the group as empty.
    pub fn clear_block_bitmap_uninit(&mut self) {
        self.flags &= !EXT4_BG_BLOCK_UNINIT;
    }

    /// Clear EXT4_BG_INODE_UNINIT.
    pub fn clear_inode_bitmap_uninit(&mut self) {
        self.flags &= !EXT4_BG_INODE_UNINIT;
    }

    /// Whether the group descriptor itself carries a checksum
    /// (RO_COMPAT_GDT_CSUM, or any metadata_csum filesystem).
    pub fn has_checksum(&self) -> bool {
        self.checksum != 0
    }
}

// Size of the original ext2 inode. ext4 inodes are this plus `i_extra_isize`
// bytes; the checksum algorithm treats the two halves separately.
pub const EXT4_GOOD_OLD_INODE_SIZE: usize = 128;

// ── Inode (128 bytes base + extra_isize extension) ───────────────────────────
//
// The on-disk size is `sb.inode_size` (typically 256 for ext4). The first
// 128 bytes match the ext2 layout. Bytes 128..inode_size hold extended
// attributes and additional timestamps.

#[repr(C, packed)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Inode {
    pub mode: u16,                  // 0x00
    pub uid: u16,                   // 0x02
    pub size_lo: u32,               // 0x04
    pub atime: u32,                 // 0x08
    pub ctime: u32,                 // 0x0C  (inode change time)
    pub mtime: u32,                 // 0x10
    pub dtime: u32,                 // 0x14
    pub gid: u16,                   // 0x18
    pub links_count: u16,           // 0x1A
    pub blocks_lo: u32,             // 0x1C  (512-byte block units)
    pub flags: u32,                 // 0x20
    pub osd1: u32,                  // 0x24  (linux: l_i_version)
    pub block: [u32; 15],           // 0x28  (60 bytes: direct/indirect or extent tree)
    pub generation: u32,            // 0x64
    pub file_acl_lo: u32,           // 0x68
    pub size_hi: u32,               // 0x6C  (dir_acl in ext2; size_hi in ext4)
    pub obso_faddr: u32,            // 0x70
    // OS-dependent 2 (12 bytes)
    pub osd2_blocks_hi: u16,        // 0x74
    pub osd2_file_acl_hi: u16,     // 0x76
    pub osd2_uid_hi: u16,           // 0x78
    pub osd2_gid_hi: u16,           // 0x7A
    pub osd2_checksum_lo: u16,      // 0x7C
    pub osd2_reserved: u16,         // 0x7E
    // ── extra isize fields (from byte 128, if inode_size > 128) ──────────
    pub extra_isize: u16,           // 0x80
    pub checksum_hi: u16,           // 0x82
    pub ctime_extra: u32,           // 0x84  (nanosecond + epoch extension)
    pub mtime_extra: u32,           // 0x88
    pub atime_extra: u32,           // 0x8C
    pub crtime: u32,                // 0x90  (creation time)
    pub crtime_extra: u32,          // 0x94
    pub version_hi: u32,            // 0x98
    pub projid: u32,                // 0x9C
}

impl Inode {
    /// File size in bytes (64-bit combined).
    pub fn size(&self) -> u64 {
        if self.is_regular() || self.is_dir() {
            (self.size_hi as u64) << 32 | self.size_lo as u64
        } else {
            self.size_lo as u64
        }
    }

    /// Number of 512-byte blocks allocated to this file (48-bit combined).
    pub fn blocks(&self) -> u64 {
        (self.osd2_blocks_hi as u64) << 32 | self.blocks_lo as u64
    }

    fn extend_time(time_lo: u32, extra: u32) -> u64 {
        let epoch_bits = (extra & 3) as u64; // lower 2 bits are epoch extension
        (epoch_bits << 32) | (time_lo as u64)
    }

    pub fn atime_64(&self) -> u64 { Self::extend_time(self.atime, self.atime_extra) }
    pub fn ctime_64(&self) -> u64 { Self::extend_time(self.ctime, self.ctime_extra) }
    pub fn mtime_64(&self) -> u64 { Self::extend_time(self.mtime, self.mtime_extra) }

    /// Whether this inode uses extent trees (vs. classic indirect block maps).
    pub fn uses_extents(&self) -> bool {
        (self.flags & EXT4_EXTENTS_FL) != 0
    }

    pub fn is_dir(&self) -> bool {
        (self.mode & EXT4_S_IFMT) == EXT4_S_IFDIR
    }

    pub fn is_regular(&self) -> bool {
        (self.mode & EXT4_S_IFMT) == EXT4_S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        (self.mode & EXT4_S_IFMT) == EXT4_S_IFLNK
    }
}

// ── Extent tree structures ───────────────────────────────────────────────────
//
// An extent tree replaces the classic 12+1+1+1 indirect block pointers.
// The root header + entries live in the inode's 60-byte `block` field.
// Internal nodes contain `ExtentIdx`; leaf nodes contain `Extent`.

/// Extent tree header (12 bytes). Present at the start of every extent
/// tree node (root in inode, or interior/leaf in a separate block).
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct ExtentHeader {
    pub magic: u16,     // Must be EXT4_EXT_MAGIC (0xF30A)
    pub entries: u16,   // Number of valid entries following
    pub max: u16,       // Maximum entries that could follow
    pub depth: u16,     // 0 = leaf (extents); >0 = internal (indices)
    pub generation: u32,
}

/// Extent index entry (12 bytes). Points to a child node one level
/// deeper in the extent tree.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct ExtentIdx {
    pub block: u32,     // Logical block this index covers
    pub leaf_lo: u32,   // Physical block of child node (low 32 bits)
    pub leaf_hi: u16,   // Physical block of child node (high 16 bits)
    pub unused: u16,
}

impl ExtentIdx {
    /// Physical block number of the child node.
    pub fn leaf(&self) -> u64 {
        (self.leaf_hi as u64) << 32 | self.leaf_lo as u64
    }
}

/// Leaf extent entry (12 bytes). Describes a contiguous run of data blocks.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Extent {
    pub block: u32,      // First logical block number
    pub len: u16,        // Number of blocks covered (max 32768; bit 15 = uninit)
    pub start_hi: u16,   // Physical block (high 16 bits)
    pub start_lo: u32,   // Physical block (low 32 bits)
}

impl Extent {
    /// First physical block of this extent.
    pub fn start(&self) -> u64 {
        (self.start_hi as u64) << 32 | self.start_lo as u64
    }

    /// Number of blocks in this extent (masking the uninitialized bit).
    pub fn block_count(&self) -> u32 {
        (self.len & 0x7FFF) as u32
    }

    /// Whether this extent is uninitialized (preallocated but unwritten).
    pub fn is_uninit(&self) -> bool {
        (self.len & 0x8000) != 0
    }
}

// ── Directory entry (linear format) ──────────────────────────────────────────
//
// ext4 supports both linear and htree-indexed directories. The linear
// format is identical to ext2. htree is an overlay on top of these entries.

#[repr(C, packed)]
pub struct DirEntry {
    pub inode: u32,
    pub rec_len: u16,
    pub name_len: u8,
    pub file_type: u8,
    // name bytes follow inline (up to 255)
}

// ── Directory entry tail (for checksummed directories) ───────────────────────

#[repr(C, packed)]
pub struct DirEntryTail {
    pub det_reserved_zero1: u32,
    pub det_rec_len: u16,
    pub det_reserved_zero2: u8,
    pub det_reserved_ft: u8,   // 0xDE
    pub det_checksum: u32,
}

// ── Htree (hash-tree) directory structures ───────────────────────────────────

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DxRoot {
    pub dot_inode: u32,
    pub dot_rec_len: u16,
    pub dot_name_len: u8,
    pub dot_file_type: u8,
    pub dot_name: [u8; 4],
    pub dotdot_inode: u32,
    pub dotdot_rec_len: u16,
    pub dotdot_name_len: u8,
    pub dotdot_file_type: u8,
    pub dotdot_name: [u8; 4],
    pub dx_reserved: u32,
    pub dx_hash_version: u8,
    pub dx_info_len: u8,
    pub dx_indirect_levels: u8,
    pub dx_unused_flags: u8,
    pub dx_limit: u16,
    pub dx_count: u16,
    pub dx_block: u32,
    // entries follow
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct DxEntry {
    pub hash: u32,
    pub block: u32,
}

// ── Journal (JBD2) superblock ────────────────────────────────────────────────
//
// Minimal definition for the journal superblock at the start of the
// journal inode. Only needed if we ever want to replay the journal.

pub const JBD2_MAGIC: u32 = 0xC03B3998;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct JournalSuperblock {
    pub header_magic: u32,
    pub header_blocktype: u32,
    pub header_sequence: u32,
    pub blocksize: u32,
    pub maxlen: u32,
    pub first: u32,
    pub sequence: u32,
    pub start: u32,
    pub errno: u32,
    // Feature flags
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub nr_users: u32,
    pub dynsuper: u32,
    pub max_transaction: u32,
    pub max_trans_data: u32,
    pub checksum_type: u8,
    pub padding2: [u8; 3],
    pub padding: [u32; 42],
    pub checksum: u32,
    pub users: [u8; 768],       // 16 * 48 user IDs
}
