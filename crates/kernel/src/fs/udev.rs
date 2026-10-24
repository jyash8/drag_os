//! Synthetic in-kernel /run/udev dynamic device metadata filesystem.
//!
//! Dynamically generates udev database records (ID_SEAT=seat0, TAGS=:seat:seat0:uaccess:)
//! for all active devices (DRM card, PS/2 input, USB hotplug input) so userspace libraries
//! (libudev, libinput, seatd) can discover and bind hardware without a userspace udevd daemon
//! or static disk files.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::fs::sysfs::{SysDir, SysFile};
use crate::fs::vfs::FileNode;

/// Resolve a path under `/run/udev`, e.g. `data/c13:64` or `data/+subsystem:input`.
pub fn resolve(rel: &str) -> Option<Arc<dyn FileNode>> {
    let trimmed = rel.trim_matches('/');

    if trimmed.is_empty() {
        // Directory: /run/udev
        let data_dir = resolve("data")?;
        return Some(Arc::new(SysDir {
            children: vec![(String::from("data"), data_dir)],
        }));
    }

    if trimmed == "data" {
        // Directory: /run/udev/data
        let mut children: Vec<(String, Arc<dyn FileNode>)> = vec![
            (
                String::from("+subsystem:input"),
                Arc::new(SysFile { data: b"V:1\n".to_vec() }),
            ),
            (
                String::from("+subsystem:drm"),
                Arc::new(SysFile { data: b"V:1\n".to_vec() }),
            ),
            (
                String::from("+subsystem:graphics"),
                Arc::new(SysFile { data: b"V:1\n".to_vec() }),
            ),
            (
                String::from("c226:0"),
                Arc::new(SysFile {
                    data: b"I:1000\nE:ID_SEAT=seat0\nE:TAGS=:seat:seat0:master-of-seat:uaccess:\nV:1\n".to_vec(),
                }),
            ),
            (
                String::from("c226:128"),
                Arc::new(SysFile {
                    data: b"I:1001\nE:ID_SEAT=seat0\nE:TAGS=:seat:seat0:uaccess:\nV:1\n".to_vec(),
                }),
            ),
        ];

        let has_mouse = {
            let caps_guard = crate::drivers::keyboard::INPUT_CAPS.lock();
            caps_guard.values().any(|c| c.rel != 0)
        };
        if has_mouse {
            let mice_content = "I:6792955\nE:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\nE:ID_INPUT_POINTER=1\nE:ID_SEAT=seat0\nE:TAGS=:seat:seat0:uaccess:\nV:1\n";
            children.push((
                String::from("c13:63"),
                Arc::new(SysFile {
                    data: mice_content.as_bytes().to_vec(),
                }),
            ));
        }

        let caps_guard = crate::drivers::keyboard::INPUT_CAPS.lock();
        for &dev_id in caps_guard.keys() {
            let minor = 64 + dev_id;
            let filename = format!("c13:{}", minor);
            if let Some(file_node) = generate_input_udev_node(dev_id, minor, &caps_guard) {
                children.push((filename, file_node));
            }
        }
        return Some(Arc::new(SysDir { children }));
    }

    let leaf = if let Some(sub) = trimmed.strip_prefix("data/") {
        sub
    } else {
        trimmed
    };

    // Subsystem marker: +subsystem:<name>
    if leaf.starts_with("+subsystem:") {
        return Some(Arc::new(SysFile {
            data: b"V:1\n".to_vec(),
        }));
    }

    // Char device entry: c<major>:<minor>
    if let Some(dev) = leaf.strip_prefix('c') {
        let mut parts = dev.split(':');
        let major = parts.next()?.parse::<u32>().ok()?;
        let minor = parts.next()?.parse::<u32>().ok()?;

        if major == 226 {
            // DRM card
            if minor == 0 {
                let content = "I:1000\nE:ID_SEAT=seat0\nE:TAGS=:seat:seat0:master-of-seat:uaccess:\nV:1\n";
                return Some(Arc::new(SysFile {
                    data: content.as_bytes().to_vec(),
                }));
            } else if minor == 128 {
                let content = "I:1001\nE:ID_SEAT=seat0\nE:TAGS=:seat:seat0:uaccess:\nV:1\n";
                return Some(Arc::new(SysFile {
                    data: content.as_bytes().to_vec(),
                }));
            }
        } else if major == 13 {
            // Input device: minor = 64 + dev_id, or minor = 63 for mice
            if minor == 63 {
                let mice_content = "I:6792955\nE:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\nE:ID_INPUT_POINTER=1\nE:ID_SEAT=seat0\nE:TAGS=:seat:seat0:uaccess:\nV:1\n";
                return Some(Arc::new(SysFile {
                    data: mice_content.as_bytes().to_vec(),
                }));
            } else if minor >= 64 {
                let dev_id = (minor - 64) as usize;
                let caps_guard = crate::drivers::keyboard::INPUT_CAPS.lock();
                return generate_input_udev_node(dev_id, minor as usize, &caps_guard);
            }
        }
    }

    None
}

fn generate_input_udev_node(
    dev_id: usize,
    minor: usize,
    caps_guard: &alloc::collections::BTreeMap<usize, crate::drivers::keyboard::InputCaps>,
) -> Option<Arc<dyn FileNode>> {
    let caps = caps_guard.get(&dev_id)?;

    let mut s = format!("I:{}\nE:ID_INPUT=1\n", 6792950 + minor);
    let has_key = caps.key.iter().any(|&b| b != 0);
    let has_rel = caps.rel != 0;
    let has_abs = caps.abs != 0;

    if has_key {
        s.push_str("E:ID_INPUT_KEYBOARD=1\n");
    }
    if has_rel {
        s.push_str("E:ID_INPUT_MOUSE=1\nE:ID_INPUT_POINTER=1\n");
    }
    if has_abs {
        let has_pen = caps.key.get(320 / 8).map(|b| b & (1 << (320 % 8)) != 0).unwrap_or(false);
        if has_pen {
            s.push_str("E:ID_INPUT_TABLET=1\nE:ID_INPUT_POINTER=1\n");
        } else {
            s.push_str("E:ID_INPUT_MOUSE=1\nE:ID_INPUT_POINTER=1\n");
        }
    }
    s.push_str("E:ID_SEAT=seat0\nE:TAGS=:seat:seat0:uaccess:\nV:1\n");

    Some(Arc::new(SysFile {
        data: s.into_bytes(),
    }))
}
