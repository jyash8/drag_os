//! Synthetic in-kernel dynamic `/dev/input` filesystem.
//!
//! Provides dynamic devtmpfs-like directory listing and resolution for all
//! active input devices (`event0`, `event1`, ..., `mice`, `events`, `by-path`, `by-id`)
//! directly from the kernel `INPUT_DEV` and `INPUT_CAPS` registries.

#![allow(unused)]

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::arch::x86_64::syscalls::{Stat, EINVAL, EISDIR, ENOENT, ENOTDIR};
use crate::fs::dirent::{DT_CHR, DT_DIR, DT_LNK};
use crate::fs::sysfs::SysSymlink;
use crate::fs::vfs::FileNode;

/// The `/dev/input` synthetic directory node.
pub struct DevInputDir;

impl FileNode for DevInputDir {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize, u64> {
        Err(EISDIR)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize, u64> {
        Err(EISDIR)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        if name == "." || name == ".." {
            return Ok(Arc::new(DevInputDir));
        }
        if name == "by-path" {
            return Ok(Arc::new(DevInputByPathDir));
        }
        if name == "by-id" {
            return Ok(Arc::new(DevInputByIdDir));
        }
        if name == "mice" {
            if let Some(minor) = mouse_minor() {
                return Ok(Arc::new(DevInputNode {
                    minor,
                    is_mice: true,
                }));
            }
            return Err(ENOENT);
        }
        if name == "events" {
            let kbd_minor = crate::drivers::keyboard::PS2_KBD_DEV.load(core::sync::atomic::Ordering::Relaxed);
            let minor = if kbd_minor != usize::MAX { kbd_minor } else { 0 };
            return Ok(Arc::new(DevInputNode {
                minor,
                is_mice: false,
            }));
        }
        if let Some(rest) = name.strip_prefix("event") {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(minor) = rest.parse::<usize>() {
                    if crate::drivers::keyboard::INPUT_DEV.lock().contains_key(&minor) {
                        return Ok(Arc::new(DevInputNode {
                            minor,
                            is_mice: false,
                        }));
                    }
                }
            }
        }
        if let Some(rest) = name.strip_prefix("mouse") {
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
                if let Ok(idx) = rest.parse::<usize>() {
                    if let Some(minor) = mouse_minor_at(idx) {
                        return Ok(Arc::new(DevInputNode {
                            minor,
                            is_mice: false,
                        }));
                    }
                }
            }
        }
        Err(ENOENT)
    }

    fn create(&self, _name: &str, _is_dir: bool, _mode: u32, _uid: u32, _gid: u32) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }

    fn truncate(&self, _size: usize) -> Result<(), u64> {
        Err(0)
    }

    fn stat(&self) -> Stat {
        let mut st = Stat::default();
        st.st_mode = 0o040755; // S_IFDIR | 0755
        st.st_nlink = 2;
        st.st_size = 4096;
        st.st_blksize = 4096;
        st.st_ino = 13000;
        st.st_dev = 1;
        st
    }

    fn is_dir(&self) -> bool {
        true
    }

    fn size(&self) -> usize {
        0
    }

    fn list_dir(&self) -> Result<Vec<(String, bool)>, u64> {
        let mut entries = vec![
            (String::from("."), true),
            (String::from(".."), true),
            (String::from("by-path"), true),
            (String::from("by-id"), true),
        ];

        let minors: Vec<usize> = crate::drivers::keyboard::INPUT_DEV
            .lock()
            .keys()
            .copied()
            .collect();

        for minor in minors {
            entries.push((format!("event{}", minor), false));
        }

        if mouse_minor().is_some() {
            entries.push((String::from("mice"), false));
            entries.push((String::from("mouse0"), false));
        }

        entries.push((String::from("events"), false));
        Ok(entries)
    }

    fn list_dir_types(&self) -> Result<Vec<(String, u8)>, u64> {
        let mut entries = vec![
            (String::from("."), DT_DIR),
            (String::from(".."), DT_DIR),
            (String::from("by-path"), DT_DIR),
            (String::from("by-id"), DT_DIR),
        ];

        let minors: Vec<usize> = crate::drivers::keyboard::INPUT_DEV
            .lock()
            .keys()
            .copied()
            .collect();

        for minor in minors {
            entries.push((format!("event{}", minor), DT_CHR));
        }

        if mouse_minor().is_some() {
            entries.push((String::from("mice"), DT_CHR));
            entries.push((String::from("mouse0"), DT_CHR));
        }

        entries.push((String::from("events"), DT_CHR));
        Ok(entries)
    }
}

/// A single character device node under `/dev/input` (e.g. `event0` or `mice`).
pub struct DevInputNode {
    pub minor: usize,
    pub is_mice: bool,
}

impl FileNode for DevInputNode {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize, u64> {
        Err(crate::arch::x86_64::syscalls::EAGAIN)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize, u64> {
        Err(0)
    }

    fn lookup(&self, _name: &str) -> Result<Arc<dyn FileNode>, u64> {
        Err(ENOTDIR)
    }

    fn create(&self, _name: &str, _is_dir: bool, _mode: u32, _uid: u32, _gid: u32) -> Result<Arc<dyn FileNode>, u64> {
        Err(ENOTDIR)
    }

    fn truncate(&self, _size: usize) -> Result<(), u64> {
        Err(EINVAL)
    }

    fn stat(&self) -> Stat {
        let mut st = Stat::default();
        st.st_mode = 0o020660; // S_IFCHR | 0660
        st.st_rdev = if self.is_mice {
            (13 << 8) | 63
        } else {
            (13 << 8) | (64 + self.minor as u64)
        };
        st.st_ino = 13000 + if self.is_mice { 63 } else { 64 + self.minor as u64 };
        st.st_nlink = 1;
        st.st_blksize = 512;
        st.st_dev = 1;
        st
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn size(&self) -> usize {
        0
    }

    fn list_dir(&self) -> Result<Vec<(String, bool)>, u64> {
        Err(ENOTDIR)
    }

    fn list_dir_types(&self) -> Result<Vec<(String, u8)>, u64> {
        Err(ENOTDIR)
    }
}

/// `/dev/input/by-path` directory node.
pub struct DevInputByPathDir;

impl FileNode for DevInputByPathDir {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize, u64> {
        Err(EISDIR)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize, u64> {
        Err(EISDIR)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        if name == "." || name == ".." {
            return Ok(Arc::new(DevInputByPathDir));
        }
        if let Some(minor) = resolve_by_path(name) {
            return Ok(Arc::new(SysSymlink {
                target: format!("../event{}", minor),
            }));
        }
        Err(ENOENT)
    }

    fn create(&self, _name: &str, _is_dir: bool, _mode: u32, _uid: u32, _gid: u32) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }

    fn truncate(&self, _size: usize) -> Result<(), u64> {
        Err(0)
    }

    fn stat(&self) -> Stat {
        let mut st = Stat::default();
        st.st_mode = 0o040755;
        st.st_nlink = 2;
        st.st_size = 4096;
        st.st_blksize = 4096;
        st.st_ino = 13001;
        st.st_dev = 1;
        st
    }

    fn is_dir(&self) -> bool {
        true
    }

    fn size(&self) -> usize {
        0
    }

    fn list_dir(&self) -> Result<Vec<(String, bool)>, u64> {
        let entries = get_by_path_entries();
        let mut list = vec![(String::from("."), true), (String::from(".."), true)];
        for (name, _) in entries {
            list.push((name, false));
        }
        Ok(list)
    }

    fn list_dir_types(&self) -> Result<Vec<(String, u8)>, u64> {
        let entries = get_by_path_entries();
        let mut list = vec![(String::from("."), DT_DIR), (String::from(".."), DT_DIR)];
        for (name, _) in entries {
            list.push((name, DT_LNK));
        }
        Ok(list)
    }
}

/// `/dev/input/by-id` directory node.
pub struct DevInputByIdDir;

impl FileNode for DevInputByIdDir {
    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize, u64> {
        Err(EISDIR)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize, u64> {
        Err(EISDIR)
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        if name == "." || name == ".." {
            return Ok(Arc::new(DevInputByIdDir));
        }
        if let Some(minor) = resolve_by_id(name) {
            return Ok(Arc::new(SysSymlink {
                target: format!("../event{}", minor),
            }));
        }
        Err(ENOENT)
    }

    fn create(&self, _name: &str, _is_dir: bool, _mode: u32, _uid: u32, _gid: u32) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }

    fn truncate(&self, _size: usize) -> Result<(), u64> {
        Err(0)
    }

    fn stat(&self) -> Stat {
        let mut st = Stat::default();
        st.st_mode = 0o040755;
        st.st_nlink = 2;
        st.st_size = 4096;
        st.st_blksize = 4096;
        st.st_ino = 13002;
        st.st_dev = 1;
        st
    }

    fn is_dir(&self) -> bool {
        true
    }

    fn size(&self) -> usize {
        0
    }

    fn list_dir(&self) -> Result<Vec<(String, bool)>, u64> {
        let entries = get_by_id_entries();
        let mut list = vec![(String::from("."), true), (String::from(".."), true)];
        for (name, _) in entries {
            list.push((name, false));
        }
        Ok(list)
    }

    fn list_dir_types(&self) -> Result<Vec<(String, u8)>, u64> {
        let entries = get_by_id_entries();
        let mut list = vec![(String::from("."), DT_DIR), (String::from(".."), DT_DIR)];
        for (name, _) in entries {
            list.push((name, DT_LNK));
        }
        Ok(list)
    }
}

pub fn mouse_minor() -> Option<usize> {
    let ps2_mouse = crate::drivers::keyboard::PS2_MOUSE_DEV.load(core::sync::atomic::Ordering::Relaxed);
    if ps2_mouse != usize::MAX && crate::drivers::keyboard::INPUT_DEV.lock().contains_key(&ps2_mouse) {
        return Some(ps2_mouse);
    }
    let caps_guard = crate::drivers::keyboard::INPUT_CAPS.lock();
    for (&dev_id, caps) in caps_guard.iter() {
        if caps.rel != 0 {
            return Some(dev_id);
        }
    }
    None
}

pub fn mouse_minor_at(idx: usize) -> Option<usize> {
    let caps_guard = crate::drivers::keyboard::INPUT_CAPS.lock();
    let mut count = 0;
    for (&dev_id, caps) in caps_guard.iter() {
        if caps.rel != 0 {
            if count == idx {
                return Some(dev_id);
            }
            count += 1;
        }
    }
    None
}

pub fn get_by_path_entries() -> Vec<(String, usize)> {
    let mut entries = Vec::new();
    let caps_guard = crate::drivers::keyboard::INPUT_CAPS.lock();
    for (&dev_id, caps) in caps_guard.iter() {
        let name_str = caps.name;
        if name_str.contains("ps2kbd") || (name_str.contains("keyboard") && caps.rel == 0 && caps.abs == 0) {
            entries.push((String::from("platform-i8042-serio-0-event-kbd"), dev_id));
        } else if name_str.contains("ps2mouse") {
            entries.push((String::from("platform-i8042-serio-1-event-mouse"), dev_id));
        } else if name_str.contains("usbkbd") {
            entries.push((format!("pci-0000:00:14.0-usb-0:{}:1.0-event-kbd", dev_id), dev_id));
        } else if name_str.contains("usbtablet") {
            entries.push((format!("pci-0000:00:14.0-usb-0:{}:1.0-event-mouse", dev_id), dev_id));
        } else if name_str.contains("usbmouse") {
            entries.push((format!("pci-0000:00:14.0-usb-0:{}:1.0-event-mouse", dev_id), dev_id));
        } else {
            entries.push((format!("platform-event{}", dev_id), dev_id));
        }
    }
    entries
}

pub fn resolve_by_path(name: &str) -> Option<usize> {
    for (entry_name, minor) in get_by_path_entries() {
        if entry_name == name {
            return Some(minor);
        }
    }
    None
}

pub fn get_by_id_entries() -> Vec<(String, usize)> {
    let mut entries = Vec::new();
    let caps_guard = crate::drivers::keyboard::INPUT_CAPS.lock();
    for (&dev_id, caps) in caps_guard.iter() {
        let name_str = caps.name;
        if name_str.contains("ps2kbd") {
            entries.push((String::from("ps2-keyboard-event-kbd"), dev_id));
        } else if name_str.contains("ps2mouse") {
            entries.push((String::from("ps2-mouse-event-mouse"), dev_id));
        } else if name_str.contains("usbkbd") {
            entries.push((String::from("usb-QEMU_USB_Keyboard-event-kbd"), dev_id));
        } else if name_str.contains("usbtablet") {
            entries.push((String::from("usb-QEMU_USB_Tablet-event-mouse"), dev_id));
        } else if name_str.contains("usbmouse") {
            entries.push((String::from("usb-QEMU_USB_Mouse-event-mouse"), dev_id));
        } else {
            entries.push((format!("input-{}-event", dev_id), dev_id));
        }
    }
    entries
}

pub fn resolve_by_id(name: &str) -> Option<usize> {
    for (entry_name, minor) in get_by_id_entries() {
        if entry_name == name {
            return Some(minor);
        }
    }
    None
}
