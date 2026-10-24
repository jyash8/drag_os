use alloc::{string::String, vec::Vec};

use crate::fs::{INITRD, vfs::VNode};

#[derive(Debug)]
pub struct TarFile<'a> {
    pub name: &'a str,
    pub size: usize,
    pub data: &'a [u8],
}

/// Parses a raw TAR byte slice and returns a list of files
pub fn parse_tar(archive: &[u8]) -> Vec<TarFile<'_>> {
    let mut files = Vec::new();
    let mut offset = 0;

    while offset + 512 <= archive.len() {
        let header = &archive[offset..offset + 512];

        // A completely empty header block represents end of archive
        if header[0] == 0 {
            break;
        }

        let name_end = header[0..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = core::str::from_utf8(&header[0..name_end]).unwrap_or("unknown");
        // GNU tar prefixes entries with "./" when archiving a directory.
        let name = name.strip_prefix("./").unwrap_or(name);

        let size_str = core::str::from_utf8(&header[124..135]).unwrap_or("0");
        let size = usize::from_str_radix(size_str.trim_matches(|c: char| c == '\0' || c.is_whitespace()), 8).unwrap_or(0);

        offset += 512;
        let data = &archive[offset..offset + size];

        files.push(TarFile { name, size, data });

        let padding = (512 - (size % 512)) % 512;
        offset += size + padding;
    }

    files
}

pub fn find_initrd_file(path: &str) -> Option<&'static [u8]> {
    let initrd = *INITRD.lock();
    let data = initrd?;
    let search_path = path.strip_prefix("/").unwrap_or(path);

    let mut offset = 0;
    while offset + 512 <= data.len() {
        let header = &data[offset..offset + 512];
        if header[0] == 0 {
            break;
        }
        let name_end = header[0..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = core::str::from_utf8(&header[0..name_end]).unwrap_or("");
        let name = name.strip_prefix("./").unwrap_or(name);
        let size_str = core::str::from_utf8(&header[124..135]).unwrap_or("0");
        let size = usize::from_str_radix(size_str.trim_matches(|c: char| c == '\0' || c.is_whitespace()), 8).unwrap_or(0);
        if name == search_path {
            return Some(&data[offset + 512..offset + 512 + size]);
        }
        let padding = (512 - (size % 512)) % 512;
        offset += 512 + size + padding;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn tarify(names: &[&str]) -> Vec<u8> {
        let mut archive = Vec::new();
        for name in names {
            let mut header = [0u8; 512];
            header[0..name.len()].copy_from_slice(name.as_bytes());
            header[124..135].copy_from_slice(b"00000000000");
            header[135] = b'0'; // typeflag '0' = regular file
            archive.extend_from_slice(&header);
        }
        archive.extend_from_slice(&[0u8; 512]);
        archive
    }

    #[test_case]
    fn parse_tar_basic() {
        let archive = tarify(&["./a.txt", "./b.bin"]);
        let files = parse_tar(&archive);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].name, "a.txt");
        assert_eq!(files[1].name, "b.bin");
        assert_eq!(files[0].size, 0);
    }

    #[test_case]
    fn parse_tar_empty() {
        let archive = [0u8; 512];
        assert!(parse_tar(&archive).is_empty());
    }

    #[test_case]
    fn parse_tar_with_data() {
        let mut archive = vec![0u8; 512 * 3];
        let mut header = &mut archive[0..512];
        header[0..7].copy_from_slice(b"./x.txt");
        header[124..135].copy_from_slice(b"00000000005");
        header[135] = b'0';
        archive[512..517].copy_from_slice(b"hello");
        archive[512 + 512] = 0; // end marker at second boundary
        let files = parse_tar(&archive);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "x.txt");
        assert_eq!(files[0].size, 5);
        assert_eq!(files[0].data, b"hello");
    }

    #[test_case]
    fn parse_tar_stops_at_zero_padding() {
        // A full header block followed by a zero block (padding between entries)
        let mut archive = vec![0u8; 512];
        archive[0..7].copy_from_slice(b"./z.txt");
        let files = parse_tar(&archive);
        assert!(files.len() >= 1, "file entry followed by zero block parses");
    }
}

pub fn find_vnode(path: &str) -> Option<crate::fs::vfs::VNode> {
    if path == "/" || path == "" || path == "." {
        return Some(crate::fs::vfs::VNode::Directory {
            name: alloc::string::String::from("/"),
            cursor: 0,
        });
    }

    let search_path = path
        .strip_prefix("/")
        .or_else(|| path.strip_prefix("./"))
        .unwrap_or(path);
    if search_path == ".." {
        return Some(crate::fs::vfs::VNode::Directory {
            name: alloc::string::String::from("/"),
            cursor: 0,
        });
    }

    let canonical_path = match search_path {
        "lib/libc.so"
        | "lib/libm.so"
        | "lib/libm.so.6"
        | "lib/libpthread.so"
        | "lib/librt.so"
        | "lib/libdl.so"
        | "usr/lib/libc.so"
        | "usr/lib/libm.so"
        | "usr/lib/libpthread.so"
        | "usr/lib/librt.so"
        | "usr/lib/libdl.so" => "lib/ld-musl-x86_64.so.1",
        other => other,
    };

    let dir_prefix = alloc::format!("{}/", search_path);

    // Try BLK_DEV first
    let mut blk_guard = crate::drivers::virtio::blk::BLK_DEV.lock();
    if let Some(blk) = blk_guard.as_mut() {
        let mut sector = 0;
        let mut bounce = [0u8; 512];
        let mut dir_seen = false;
        
        while sector < blk.capacity() {
            if blk.read_sector(sector, &mut bounce).is_err() { break; }
            if bounce[0] == 0 { break; } // End of archive
            
            let name_end = bounce[0..100].iter().position(|&b| b == 0).unwrap_or(100);
            let name = core::str::from_utf8(&bounce[0..name_end]).unwrap_or("");
            let name = name.strip_prefix("./").unwrap_or(name);
            
            let size_str = core::str::from_utf8(&bounce[124..135]).unwrap_or("0");
            let size = usize::from_str_radix(size_str.trim_matches(|c: char| c == '\0' || c.is_whitespace()), 8).unwrap_or(0);
            
            if name == canonical_path {
                return Some(crate::fs::vfs::VNode::BlockFile {
                    start_sector: sector + 1,
                    size,
                    cursor: 0,
                });
            }
            if name.starts_with(&dir_prefix) || name == dir_prefix {
                dir_seen = true;
            }
            
            let sectors_for_data = (size + 511) / 512;
            sector += 1 + sectors_for_data as u64;
        }
        if dir_seen {
            return Some(crate::fs::vfs::VNode::Directory {
                name: alloc::string::String::from(search_path),
                cursor: 0,
            });
        }
    }
    drop(blk_guard);

    let initrd = *INITRD.lock();
    let data = initrd?;
    let mut dir_seen = false;
    let mut found = None;
    let mut offset = 0;
    while offset + 512 <= data.len() {
        let header = &data[offset..offset + 512];
        if header[0] == 0 {
            break;
        }
        let name_end = header[0..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = core::str::from_utf8(&header[0..name_end]).unwrap_or("");
        let name = name.strip_prefix("./").unwrap_or(name);
        let size_str = core::str::from_utf8(&header[124..135]).unwrap_or("0");
        let size = usize::from_str_radix(size_str.trim_matches(|c: char| c == '\0' || c.is_whitespace()), 8).unwrap_or(0);
        if name == canonical_path {
            found = Some(crate::fs::vfs::VNode::InitrdFile {
                data: &data[offset + 512..offset + 512 + size],
                cursor: 0,
            });
            break;
        }
        if name.starts_with(&dir_prefix) || name == dir_prefix {
            dir_seen = true;
        }
        let padding = (512 - (size % 512)) % 512;
        offset += 512 + size + padding;
    }

    if found.is_some() {
        found
    } else if dir_seen {
        Some(VNode::Directory {
            name: String::from(search_path),
            cursor: 0,
        })
    } else {
        None
    }
}
