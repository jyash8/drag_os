//! Minimal read-only sysfs exposing device metadata that userspace consumers
//! expect (`/sys/dev/char/<maj>:<min>/uevent`, class aliases). This is what
//! libudev-zero (and therefore libinput) reads instead of a real udev daemon.

use crate::arch::sync::spinlock::SpinLock;
use crate::arch::x86_64::syscalls::Stat;
use crate::fs::vfs::FileNode;
use alloc::sync::Arc;
#[allow(unused_imports)]
use alloc::vec;
use alloc::string::String;
use alloc::vec::Vec;

/// One char device known to the kernel's devtmpfs-equivalent path matcher.
#[derive(Clone, Copy)]
pub struct CharDev {
    pub major: u32,
    pub minor: u32,
    /// Path under /dev, e.g. "input/event0".
    pub devname: &'static str,
    pub subsystem: &'static str,
    /// uevent capability lines (EV=/KEY=/REL=/PROP=), pre-encoded.
    pub caps: &'static str,
}

// ponytail: runtime registry — hotplug registers nodes, boot seeds the six
// static ones via sysfs_init(). root() snapshots under one short lock hold,
// then builds the tree unlocked (SpinLock disables IRQs, so no alloc/println
// while held).
pub static SYSFS_REGISTRY: SpinLock<Vec<CharDev>> = SpinLock::new(Vec::new());

pub fn sysfs_register(dev: CharDev) {
    SYSFS_REGISTRY.lock().push(dev);
}

pub fn sysfs_unregister(major: u32, minor: u32) {
    SYSFS_REGISTRY
        .lock()
        .retain(|d| !(d.major == major && d.minor == minor));
}

// ponytail: hotplug (PS/2 init, USB attach) registers eventN≥0 nodes at
// runtime; devname is leaked once (process context only — never IRQ).
// caps is one of the EVDEV_CAPS_* literals below (must stay 'static).
pub fn sysfs_register_input(minor: usize, caps: &'static str) {
    let devname: &'static str =
        alloc::boxed::Box::leak(alloc::format!("input/event{minor}").into_boxed_str());
    sysfs_register(CharDev {
        major: 13,
        minor: 64 + minor as u32,
        devname,
        subsystem: "input",
        caps,
    });
}

// ponytail: uevent/caps literals per physical kind — the KEY/REL bitmaps are
// the proven event0/event1 strings (libinput accepts them); keyboard nodes
// just drop the REL axes and MOUSE/POINTER tags. Tablet-as-REL reuses MOUSE.
pub const EVDEV_CAPS_KBD: &str = "EV=3\nREL=0\nKEY=70000 0 0 0 ffffffff ffffffff ffffffff fffffffe\nPROP=0\n\
ID_INPUT=1\nID_INPUT_KEYBOARD=1\n";
pub const EVDEV_CAPS_MOUSE: &str = "EV=7\nREL=103\nKEY=70000 0 0 0 0 0 0 70000\nPROP=0\n\
ID_INPUT=1\nID_INPUT_MOUSE=1\nID_INPUT_POINTER=1\n";

/// Seed the four non-input boot devices. Call once from kernel boot (heap must
/// be up); idempotent so a repeat call is a no-op. event0/event1 are NOT
/// seeded here — PS/2 kbd/mouse init allocates their minors and calls
/// sysfs_register_input itself; seeding them here too would duplicate the
/// registry entries.
pub fn sysfs_init() {
    if !SYSFS_REGISTRY.lock().is_empty() {
        return;
    }
    // ALSA sound card (AC97) — major 116
    sysfs_register(CharDev {
        major: 116,
        minor: 0,
        devname: "snd/controlC0",
        subsystem: "sound",
        caps: "",
    });
    sysfs_register(CharDev {
        major: 116,
        minor: 1,
        devname: "snd/pcmC0D0p",
        subsystem: "sound",
        caps: "",
    });
    sysfs_register(CharDev {
        major: 116,
        minor: 16,
        devname: "snd/pcmC0D0c",
        subsystem: "sound",
        caps: "",
    });
    sysfs_register(CharDev {
        major: 226,
        minor: 0,
        devname: "dri/card0",
        subsystem: "drm",
        caps: "",
    });
    sysfs_register(CharDev {
        major: 226,
        minor: 128,
        devname: "dri/renderD128",
        subsystem: "drm",
        caps: "",
    });
    sysfs_register(CharDev {
        major: 29,
        minor: 0,
        devname: "fb0",
        subsystem: "graphics",
        caps: "",
    });
    sysfs_register(CharDev {
        major: 1,
        minor: 3,
        devname: "null",
        subsystem: "mem",
        caps: "",
    });
}

// ── generic node types ──────────────────────────────────────────────────────

pub struct SysFile {
    pub data: Vec<u8>,
}

pub struct SysDir {
    pub children: Vec<(alloc::string::String, Arc<dyn FileNode>)>,
}

type Name = alloc::string::String;

fn stat_node(is_dir: bool, size: usize) -> Stat {
    let mut st = Stat::default();
    st.st_mode = if is_dir { 0o40755 } else { 0o100644 };
    st.st_size = size as i64;
    st
}

impl FileNode for SysFile {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, u64> {
        if offset >= self.data.len() {
            return Ok(0);
        }
        let n = (self.data.len() - offset).min(buf.len());
        buf[..n].copy_from_slice(&self.data[offset..offset + n]);
        Ok(n)
    }
    fn write_at(&self, _off: usize, _buf: &[u8]) -> Result<usize, u64> {
        Err(0)
    }
    fn lookup(&self, _: &str) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }
    fn create(&self, _: &str, _: bool, _: u32, _: u32, _: u32) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }
    fn truncate(&self, _: usize) -> Result<(), u64> {
        Err(0)
    }
    fn stat(&self) -> Stat {
        stat_node(false, self.data.len())
    }
    fn is_dir(&self) -> bool {
        false
    }
    fn size(&self) -> usize {
        self.data.len()
    }
    fn list_dir(&self) -> Result<Vec<(Name, bool)>, u64> {
        Err(0)
    }
}

impl FileNode for SysDir {
    fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize, u64> {
        Err(0)
    }
    fn write_at(&self, _: usize, _: &[u8]) -> Result<usize, u64> {
        Err(0)
    }
    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        self.children
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, n)| n.clone())
            .ok_or(0)
    }
    fn create(&self, _: &str, _: bool, _: u32, _: u32, _: u32) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }
    fn truncate(&self, _: usize) -> Result<(), u64> {
        Err(0)
    }
    fn stat(&self) -> Stat {
        stat_node(true, 0)
    }
    fn is_dir(&self) -> bool {
        true
    }
    fn size(&self) -> usize {
        0
    }
    fn list_dir(&self) -> Result<Vec<(Name, bool)>, u64> {
        Ok(self
            .children
            .iter()
            .map(|(n, f)| (n.clone(), f.is_dir()))
            .collect())
    }
}

pub struct SysSymlink {
    pub target: alloc::string::String,
}

impl FileNode for SysSymlink {
    fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize, u64> {
        Err(0)
    }
    fn write_at(&self, _: usize, _: &[u8]) -> Result<usize, u64> {
        Err(0)
    }
    fn lookup(&self, _: &str) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }
    fn create(&self, _: &str, _: bool, _: u32, _: u32, _: u32) -> Result<Arc<dyn FileNode>, u64> {
        Err(0)
    }
    fn truncate(&self, _: usize) -> Result<(), u64> {
        Err(0)
    }
    fn stat(&self) -> Stat {
        let mut st = Stat::default();
        st.st_mode = 0o120777;
        st.st_size = self.target.len() as i64;
        st
    }
    fn is_dir(&self) -> bool {
        false
    }
    fn size(&self) -> usize {
        self.target.len()
    }
    fn list_dir(&self) -> Result<Vec<(Name, bool)>, u64> {
        Err(0)
    }
    fn read_link(&self) -> Result<alloc::string::String, u64> {
        Ok(self.target.clone())
    }
}

fn dir(children: Vec<(&str, Arc<dyn FileNode>)>) -> Arc<dyn FileNode> {
    Arc::new(SysDir {
        children: children
            .into_iter()
            .map(|(n, f)| (alloc::string::String::from(n), f))
            .collect(),
    })
}

fn file(content: &str) -> Arc<dyn FileNode> {
    Arc::new(SysFile {
        data: content.as_bytes().to_vec(),
    })
}

fn symlink(target: &str) -> Arc<dyn FileNode> {
    Arc::new(SysSymlink {
        target: alloc::string::String::from(target),
    })
}

/// Directory for one char device: uevent + dev + subsystem + device symlinks.
fn dev_dir(d: &CharDev) -> Arc<dyn FileNode> {
    let uevent = alloc::format!(
        "MAJOR={}\nMINOR={}\nDEVNAME={}\nSUBSYSTEM={}\n{}",
        d.major,
        d.minor,
        d.devname,
        d.subsystem,
        d.caps
    );
    let dev = alloc::format!("{}:{}\n", d.major, d.minor);
    let sub_target = alloc::format!("../../../class/{}", d.subsystem);
    let dev_target = match (d.subsystem, d.devname) {
        // ponytail: per-node parent — eventN (major 13, minor 64+N) links to
        // inputN; N≥2 are hotplug nodes with no special case.
        ("input", _) if d.major == 13 && d.minor >= 64 => {
            alloc::format!("../../../devices/virtual/input/input{}", d.minor - 64)
        }
        ("drm", _) | ("graphics", _) => {
            alloc::format!("../../../devices/pci0000:00/0000:00:02.0")
        }
        _ => alloc::format!("../../../devices/virtual/mem/null"),
    };
    dir(vec![
        ("uevent", file(uevent.as_str())),
        ("dev", file(dev.as_str())),
        ("subsystem", symlink(sub_target.as_str())),
        ("device", symlink(dev_target.as_str())),
    ])
}

// ── tree construction ───────────────────────────────────────────────────────

/// Build the entire /sys tree. Small enough to rebuild per mount; every node
/// is immutable.
pub fn root() -> Arc<dyn FileNode> {
    // ponytail: snapshot the registry under one short lock hold; the tree
    // build below runs unlocked. CharDev is Copy so this is a memcpy.
    let devs: Vec<CharDev> = SYSFS_REGISTRY.lock().iter().copied().collect();

    let char_owned: Vec<(String, String)> = devs
        .iter()
        .map(|d| {
            let leaf = d.devname.rsplit('/').next().unwrap_or(d.devname);
            (
                alloc::format!("{}:{}", d.major, d.minor),
                alloc::format!("../../class/{}/{}", d.subsystem, leaf),
            )
        })
        .collect();
    let char_entries: Vec<(&str, Arc<dyn FileNode>)> = char_owned
        .iter()
        .map(|(n, t)| (n.as_str(), symlink(t.as_str())))
        .collect();

    // class/<subsystem>/<leaf-name> aliases
    let mut input_kids: Vec<(&str, Arc<dyn FileNode>)> = Vec::new();
    let mut drm_kids: Vec<(&str, Arc<dyn FileNode>)> = Vec::new();
    let mut sound_kids: Vec<(&str, Arc<dyn FileNode>)> = Vec::new();
    let mut gfx_kids: Vec<(&str, Arc<dyn FileNode>)> = Vec::new();
    let mut mem_kids: Vec<(&str, Arc<dyn FileNode>)> = Vec::new();
    for d in &devs {
        let leaf = d.devname.rsplit('/').next().unwrap_or(d.devname);
        match d.subsystem {
            "input" => input_kids.push((leaf, dev_dir(d))),
            "drm" => drm_kids.push((leaf, dev_dir(d))),
            "sound" => sound_kids.push((leaf, dev_dir(d))),
            "graphics" => gfx_kids.push((leaf, dev_dir(d))),
            "mem" => mem_kids.push((leaf, dev_dir(d))),
            _ => {}
        }
    }

    // ponytail: per-node parents — every registered 13:64+N evdev gets its
    // own devices/virtual/input/inputN dir (hotplug N≥2 included, no
    // special-casing). Parent tags follow the node caps (pointer axes imply
    // MOUSE+POINTER). Looked up by (major, minor), not table index, so the
    // parent dirs survive unregister/re-register of other nodes.
    let mut input_devs: Vec<&CharDev> =
        devs.iter().filter(|d| d.major == 13 && d.minor >= 64).collect();
    input_devs.sort_by_key(|d| d.minor);
    let mut virtual_input_kids: Vec<(String, Arc<dyn FileNode>)> = Vec::new();
    for ev in input_devs {
        let n = ev.minor - 64;
        let tags = if ev.caps.contains("ID_INPUT_MOUSE") {
            "SUBSYSTEM=input\nID_INPUT=1\nID_INPUT_MOUSE=1\nID_INPUT_POINTER=1\nID_SEAT=seat0\n"
        } else {
            "SUBSYSTEM=input\nID_INPUT=1\nID_INPUT_KEYBOARD=1\nID_SEAT=seat0\n"
        };
        let ev_leaf: String = ev.devname.rsplit('/').next().unwrap_or(ev.devname).into();
        let mut kids: Vec<(&str, Arc<dyn FileNode>)> = vec![("uevent", file(tags))];
        kids.push((ev_leaf.as_str(), dev_dir(ev)));
        kids.push(("subsystem", symlink("../../../../class/input")));
        let input_dir = dir(kids);
        virtual_input_kids.push((alloc::format!("input{n}"), input_dir));
    }
    let virtual_input_children: Vec<(&str, Arc<dyn FileNode>)> = virtual_input_kids
        .iter()
        .map(|(n, d)| (n.as_str(), d.clone()))
        .collect();
    let virtual_input = dir(virtual_input_children);
    let virtual_dir = dir(vec![("input", virtual_input)]);

    let pci_gpu_uevent = "DRIVER=virtio-pci\nPCI_CLASS=30000\nPCI_ID=1AF4:1050\nPCI_SUBSYS_ID=1AF4:1100\nPCI_SLOT_NAME=0000:00:02.0\nMODALIAS=pci:v00001AF4d00001050sv00001AF4sd00001100bc03sc00i00\n";
    let pci_gpu_drm = dir(vec![
        ("card0", symlink("../../../class/drm/card0")),
        ("renderD128", symlink("../../../class/drm/renderD128")),
    ]);
    let pci_gpu_dir = dir(vec![
        ("vendor", file("0x1af4\n")),
        ("device", file("0x1050\n")),
        ("subsystem_vendor", file("0x1af4\n")),
        ("subsystem_device", file("0x1100\n")),
        ("class", file("0x030000\n")),
        ("revision", file("0x00\n")),
        ("uevent", file(pci_gpu_uevent)),
        ("subsystem", symlink("../../../bus/pci")),
        ("driver", symlink("../../../bus/pci/drivers/virtio-pci")),
        ("drm", pci_gpu_drm),
    ]);
    let pci_root_dir = dir(vec![("0000:00:02.0", pci_gpu_dir)]);

    let devices_dir = dir(vec![
        ("virtual", virtual_dir),
        ("pci0000:00", pci_root_dir),
    ]);

    dir(vec![
        (
            "dev",
            dir(vec![("char", dir(char_entries)), ("block", dir(vec![]))]),
        ),
        (
            "bus",
            dir(vec![(
                "pci",
                dir(vec![(
                    "drivers",
                    dir(vec![("virtio-pci", dir(vec![("uevent", file("DRIVER=virtio-pci\n"))]))]),
                )]),
            )]),
        ),
        ("class", dir(vec![
            ("input", dir(input_kids)),
            ("drm", dir(drm_kids)),
            ("sound", dir(sound_kids)),
            ("graphics", dir(gfx_kids)),
            ("mem", dir(mem_kids)),
        ])),
        ("devices", devices_dir),
    ])
}

/// Resolve a path relative to /sys (e.g. "dev/char/226:0/device/vendor").
/// Follows symlinks within the /sys tree with loop detection.
pub fn resolve(rel: &str) -> Option<Arc<dyn FileNode>> {
    let clean = rel.trim_matches('/');
    if clean.is_empty() {
        return Some(root());
    }

    let mut cur_dir = root();
    let mut dir_stack: Vec<Arc<dyn FileNode>> = alloc::vec![cur_dir.clone()];
    let mut parts: alloc::collections::VecDeque<alloc::string::String> = clean
        .split('/')
        .filter(|s| !s.is_empty())
        .map(alloc::string::String::from)
        .collect();

    let mut symlink_count = 0;
    while let Some(part) = parts.pop_front() {
        if part == "." {
            continue;
        }
        if part == ".." {
            if dir_stack.len() > 1 {
                dir_stack.pop();
            }
            cur_dir = dir_stack.last()?.clone();
            continue;
        }

        let next = cur_dir.lookup(&part).ok()?;
        if let Ok(target) = next.read_link() {
            if !parts.is_empty() {
                symlink_count += 1;
                if symlink_count > 32 {
                    return None;
                }
                let target_parts: Vec<&str> = target.split('/').filter(|s| !s.is_empty()).collect();
                if target.starts_with('/') {
                    cur_dir = root();
                    dir_stack = alloc::vec![cur_dir.clone()];
                }
                for tp in target_parts.into_iter().rev() {
                    parts.push_front(alloc::string::String::from(tp));
                }
                continue;
            }
            return Some(next);
        }

        if next.is_dir() {
            cur_dir = next.clone();
            dir_stack.push(cur_dir.clone());
        } else {
            if !parts.is_empty() {
                return None;
            }
            return Some(next);
        }
    }

    Some(cur_dir)
}
