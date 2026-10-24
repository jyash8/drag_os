//! linux_dirent64 packing for `getdents64` (syscall 217).
//!
//! Layout (`<linux/dirent.h>`):
//! ```c
//! struct linux_dirent64 {
//!     u64  d_ino;
//!     s64  d_off;
//!     u16  d_reclen;   // length of this whole record, padded to 8-byte align
//!     u8   d_type;     // DT_REG=8, DT_DIR=4, DT_UNKNOWN=0
//!     char d_name[];   // NUL-terminated, padding to make reclen multiple of 8
//! };
//! ```

pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;

const HEADER_LEN: usize = 19;

/// Packs one dirent64 record into `buf`.
///
/// Returns the record length written (= `d_reclen`, multiple of 8), or 0 if
/// the name (including NUL terminator) does not fit in `buf`.
pub fn pack_dirent64(
    buf: &mut [u8],
    d_ino: u64,
    d_off: i64,
    d_type: u8,
    name: &str,
) -> usize {
    let name_len = name.len();
    let needed = HEADER_LEN + name_len + 1; // header + name + NUL
    let reclen = needed.div_ceil(8) * 8;
    if reclen > buf.len() {
        return 0;
    }

    buf[0..8].copy_from_slice(&d_ino.to_ne_bytes());
    buf[8..16].copy_from_slice(&d_off.to_ne_bytes());
    buf[16..18].copy_from_slice(&(reclen as u16).to_ne_bytes());
    buf[18] = d_type;
    buf[19..19 + name_len].copy_from_slice(name.as_bytes());
    buf[19 + name_len] = 0; // NUL terminator
    // Trailing padding (reclen - needed) bytes already zero in `buf`.

    reclen
}

/// Walks a TAR archive and packs one or more `linux_dirent64` records for the
/// immediate children of `dir_name` into `out`.
///
/// - `dir_name == "/"` lists top-level entries.
/// - Any other directory lists entries whose TAR name has `dir_name` as a prefix,
///   then takes the first path component after that prefix as the child name.
///
/// `cursor` is the index of the first child to emit (callers store it on the
/// directory fd to support repeated calls). Returns `(bytes_written, new_cursor)`
/// where `new_cursor` is the index of the next entry *after* the last one
/// written. When `bytes_written == 0`, the caller has reached end-of-directory.

pub fn list_dir_into(
    data: &[u8],
    dir_name: &str,
    cursor: usize,
    out: &mut [u8],
) -> (usize, usize) {
    let prefix = if dir_name == "/" {
        alloc::string::String::new()
    } else {
        alloc::format!("{}/", dir_name)
    };

    let mut children: alloc::vec::Vec<(alloc::string::String, bool)> =
        alloc::vec::Vec::new();
    let mut off: usize = 0;
    while off + 512 <= data.len() {
        let header = &data[off..off + 512];
        if header[0] == 0 {
            break;
        }
        let name_end = header[0..100].iter().position(|&b| b == 0).unwrap_or(100);
        let raw = core::str::from_utf8(&header[0..name_end]).unwrap_or("");
        let stripped = raw.strip_prefix("./").unwrap_or(raw);

        if let Some(rest) = stripped.strip_prefix(&prefix) {
            if !rest.is_empty() {
                let child = rest.split('/').next().unwrap_or(rest);
                if !child.is_empty() {
                    let is_dir = rest.contains('/');
                    if let Some(entry) =
                        children.iter_mut().find(|(n, _)| n.as_str() == child)
                    {
                        entry.1 |= is_dir;
                    } else {
                        children.push((alloc::string::String::from(child), is_dir));
                    }
                }
            }
        }

        let size_str = core::str::from_utf8(&header[124..135]).unwrap_or("0");
        let size = usize::from_str_radix(
            size_str.trim_matches(|c: char| c == '\0' || c.is_whitespace()),
            8,
        )
        .unwrap_or(0);
        let padding = (512 - (size % 512)) % 512;
        off += 512 + size + padding;
    }

    // Emit dirent64 records from `cursor` onward.
    let mut written: usize = 0;
    let mut new_cursor = children.len();
    for (i, (name, is_dir)) in children.iter().enumerate().skip(cursor) {
        let d_type = if *is_dir { DT_DIR } else { DT_REG };
        let d_off = (i as i64) + 1;
        let rec = pack_dirent64(
            &mut out[written..],
            i as u64,
            d_off,
            d_type,
            name,
        );
        if rec == 0 {
            new_cursor = i;
            break;
        }
        written += rec;
    }

    (written, new_cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};

    #[test_case]
    fn pack_reclen_rounds_to_8() {
        let mut buf = [0u8; 128];
        // "hi" -> needed=19+2+1=22, reclen=24
        let rec = pack_dirent64(&mut buf, 42, 7, DT_REG, "hi");
        assert_eq!(rec, 24);
        assert_eq!(buf[0..8], 42u64.to_ne_bytes());
        assert_eq!(buf[8..16], 7i64.to_ne_bytes());
        assert_eq!(u16::from_ne_bytes([buf[16], buf[17]]), 24);
        assert_eq!(buf[18], DT_REG);
        assert_eq!(&buf[19..21], b"hi");
        assert_eq!(buf[21], 0);
    }

    #[test_case]
    fn pack_truncates_when_no_fit() {
        // "toolongname" -> needed=19+11+1=31, reclen=32 > 24
        let mut buf = [0u8; 24];
        assert_eq!(pack_dirent64(&mut buf, 1, 1, DT_DIR, "toolongname"), 0);
    }

    #[test_case]
    fn pack_name_exactly_fits() {
        // needed=19+4+1=24, reclen=24, exactly fits buf of 24
        let mut buf = [0u8; 24];
        assert_eq!(pack_dirent64(&mut buf, 1, 1, DT_DIR, "abcd"), 24);
    }

    #[test_case]
    fn list_dir_root_dedupes() {
        // Build a mini TAR: ./a.txt (3 bytes "abc"), ./a.txt again (dir-ish not possible here)
        // ./b/c -> child b is a dir
        let mut archive = vec![0u8; 512 * 4];
        let mk = |name: &[u8], size: usize, data: &[u8], out: &mut [u8]| {
            out[0..name.len()].copy_from_slice(name);
            out[124..135].copy_from_slice(&format!("{:011o}", size).into_bytes());
            out[135] = b'0';
            let data_off = 512;
            out[512..512 + data.len()].copy_from_slice(data);
        };
        mk(b"./a.txt", 3, b"abc", &mut archive[0..1024]);
        // ./b/c.txt length 5
        mk(b"./b/c.txt", 5, b"hello", &mut archive[1024..2048]);

        let mut out = [0u8; 256];
        let (written, cursor) = list_dir_into(&archive, "/", 0, &mut out);
        assert!(written > 0);
        // Expect children: a.txt and b (dedup, keep order). b is dir.
        assert!(cursor > 0);
        // Parse emitted records
        let mut off = 0;
        let mut names = alloc::vec::Vec::new();
        while off + 19 <= written {
            let reclen = u16::from_ne_bytes([out[off + 16], out[off + 17]]) as usize;
            if reclen < 19 { break; }
            let name_end = (off + 19..off + reclen).find(|&i| out[i] == 0);
            let name = name_end
                .map(|e| core::str::from_utf8(&out[off + 19..e]).unwrap_or(""))
                .unwrap_or("");
            names.push(alloc::string::String::from(name));
            off += reclen;
        }
        assert_eq!(names, alloc::vec!["a.txt", "b"]);
    }
}
