//! CRC-32C (Castagnoli) — the checksum ext4 uses for every metadata object
//! when `metadata_csum` (RO_COMPAT 0x400) is enabled.
//!
//! Reflected algorithm, polynomial 0x1EDC6F41 reversed to 0x82F63B78.
//!
//! # Seed convention
//!
//! This mirrors `ext2fs_crc32c_le()` exactly, which is the reference every
//! on-disk ext4 checksum is defined against. That function performs **no**
//! pre- or post-inversion: the caller's seed is used directly as the initial
//! shift register, and the result is the final register. So
//!
//! ```text
//! crc32c_le(~0, data, len)  ==  raw(~0, data)
//! ```
//!
//! and chaining is literal: the value returned by one call is a valid seed for
//! the next. Note this differs from the "standard" CRC-32C check value
//! convention (`crc32c_le` of `"123456789"` over a register seeded at
//! `0xFFFFFFFF`, then inverted), which is *not* what ext4 stores.
//!
//! Chaining matters here because ext4's checksums deliberately skip a field in
//! the middle of the object they cover (an inode's `i_checksum_lo`/`hi`, a
//! directory block's `det_checksum`, a descriptor's `bg_checksum`). Splitting
//! the object into the ranges around the skipped field and chaining over them
//! hashes exactly the same byte sequence as zeroing the field first.


/// Reflected CRC-32C polynomial.
const POLY: u32 = 0x82F6_3B78;

/// Build the 256-entry lookup table at compile time.
const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            // Conditionally XOR the polynomial, then shift right.
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

/// Continue a CRC-32C over `data`, updating the running value in place.
///
/// `bitbuf` is the raw shift register: pass `!0u32` to start a fresh checksum,
/// then feed the result straight back in to continue over another buffer.
pub fn crc32c(bitbuf: &mut u32, data: &[u8]) {
    let mut crc = *bitbuf;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = TABLE[idx] ^ (crc >> 8);
    }
    *bitbuf = crc;
}

/// One-shot CRC-32C over `data` seeded with `seed`.
pub fn checksum(seed: u32, data: &[u8]) -> u32 {
    let mut c = seed;
    crc32c(&mut c, data);
    c
}
