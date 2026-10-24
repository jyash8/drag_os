//! In-memory filesystem mounted at /tmp. Backs XDG_RUNTIME_DIR and the
//! Wayland socket for userspace compositors.

use crate::arch::sync::spinlock::SpinLock;
use crate::arch::x86_64::syscalls::Stat;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::fs::vfs::FileNode;
use crate::memory::dsocache;
use shared::boot::boot_info::HHDM_BASE;

pub struct TmpFsNode {
    inner: Arc<SpinLock<TmpInner>>,
    /// A stable identity for the DSO cache.
    ///
    /// It used to be the address of `inner`, which is a heap pointer: when a
    /// file is closed that address is freed and the *next* allocation of the
    /// same size lands there, so two different files shared one cache id. The
    /// cache is keyed by (id, page offset), so the second file was served the
    /// first file's pages -- zeros whenever the original page was a hole,
    /// which is the common case for a file that was only partly written.
    ///
    /// Anything mapped from a tmpfs (a library under /tmp, a shm object) is
    /// then parsed from another file's bytes. An identity from a counter is
    /// never reused, so the cache cannot confuse two files.
    cache_id: u64,
}

fn next_cache_id() -> u64 {
    crate::fs::vfs::standalone_dso_id()
}

/// Sparse page-granular backing: `ftruncate` only records the logical size
/// (real tmpfs semantics). Pages are allocated on first write, so a 3.75 MiB
/// wl_shm pool with a few KB touched costs a few KB of kernel heap, not the
/// full size. Previously this was a dense `Vec<u8>` and every pool truncate
/// ate ~3.75 MiB of the fixed 256 MiB heap → OOM after ~68 pools.
const TMPFS_PAGE: usize = 4096;

struct TmpInner {
    is_dir: bool,
    size: usize,
    pages: BTreeMap<u64, Box<[u8; TMPFS_PAGE]>>,
    children: BTreeMap<String, Arc<TmpFsNode>>,
    /// Permission bits, as `chmod` sets them. Needed because a program that
    /// tightens a directory to 0700 and then reads it back must see its own
    /// change -- zellij does exactly this on its temp dir
    /// (`atomic_create_dir` → `set_permissions`), and with no storage here the
    /// call fell through to `FileNode`'s `Err(ENOSYS)` default and killed the
    /// program at its first statement.
    mode: u32,
    /// Owner, as `chown` sets them. Same reason: `fs::metadata` reports them,
    /// and sshd's pty_setowner fatals if the values it just wrote do not read
    /// back.
    uid: u32,
    gid: u32,
}

impl TmpFsNode {
    pub fn new_dir(uid: u32, gid: u32, mode: u32) -> Self {
        Self {
            cache_id: next_cache_id(),
            inner: Arc::new(SpinLock::new(TmpInner {
                is_dir: true,
                size: 0,
                pages: BTreeMap::new(),
                children: BTreeMap::new(),
                mode: mode & 0o7777,
                uid,
                gid,
            })),
        }
    }

    fn new_file(uid: u32, gid: u32, mode: u32) -> Self {
        Self {
            cache_id: next_cache_id(),
            inner: Arc::new(SpinLock::new(TmpInner {
                is_dir: false,
                size: 0,
                pages: BTreeMap::new(),
                children: BTreeMap::new(),
                mode: mode & 0o7777,
                uid,
                gid,
            })),
        }
    }

    pub fn new_file_node() -> Arc<dyn FileNode> {
        Arc::new(Self::new_file(0, 0, 0o644))
    }
    pub fn new_dir_node() -> Arc<dyn FileNode> {
        Arc::new(Self::new_dir(0, 0, 0o755))
    }
}

static TMPFS_ROOT: SpinLock<Option<Arc<TmpFsNode>>> = SpinLock::new(None);

fn root() -> Arc<TmpFsNode> {
    let mut g = TMPFS_ROOT.lock();
    if let Some(r) = g.as_ref() {
        return r.clone();
    }
    let r = Arc::new(TmpFsNode::new_dir(0, 0, 0o755));
    *g = Some(r.clone());
    r
}

/// Resolve a cleaned path (no leading slash) under /tmp.
/// Returns the node; `create` controls whether missing files/dirs are made.
pub fn resolve(clean: &str, create_dirs: bool, create_file: bool) -> Option<Arc<dyn FileNode>> {
    let parts: Vec<&str> = clean.split('/').filter(|c| !c.is_empty()).collect();
    if parts.is_empty() {
        return Some(root());
    }

    let mut current = root();
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let next = current.inner.lock().children.get(*part).cloned();
        match next {
            Some(n) => current = n,
            None => {
                if !create_dirs && !create_file {
                    return None;
                }
                // Only auto-create the final component as a file when asked,
                // intermediate components always as dirs.
                if !last || create_dirs {
                    let d = Arc::new(TmpFsNode::new_dir(0, 0, 0o755));
                    current.inner.lock().children.insert(String::from(*part), d.clone());
                    current = d;
                    continue;
                } else if last && create_file {
                    let f = Arc::new(TmpFsNode::new_file(0, 0, 0o644));
                    current.inner.lock().children.insert(String::from(*part), f.clone());
                    return Some(f);
                }
                return None;
            }
        }
    }
    Some(current)
}

impl FileNode for TmpFsNode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, u64> {
        let inner = self.inner.lock();
        if inner.is_dir {
            return Err(0);
        }
        if offset >= inner.size {
            return Ok(0);
        }
        let n = (inner.size - offset).min(buf.len());
        // Holes read as zeros, unless the page was faulted in via demand
        // paging (mmap) by another process and cached in the DSO cache.  That
        // path shares physical frames between processes: a client that writes
        // to a memfd through mmap populates a DSO-cached frame, and a
        // compositor that reads the same memfd through read() must see those
        // bytes, not the tmpfs hole.
        let mut done = 0;
        while done < n {
            let pos = offset + done;
            let page_no = (pos / TMPFS_PAGE) as u64;
            let page_off = pos % TMPFS_PAGE;
            let chunk = (TMPFS_PAGE - page_off).min(n - done);
            if let Some(frame) = dsocache::lookup_and_retain(self.cache_id, page_no) {
                let frame_va = (frame.0 + HHDM_BASE.0) as *const u8;
                let page_data = unsafe { core::slice::from_raw_parts(frame_va, TMPFS_PAGE) };
                buf[done..done + chunk].copy_from_slice(&page_data[page_off..page_off + chunk]);
                dsocache::release(self.cache_id, page_no);
            } else if let Some(page) = inner.pages.get(&page_no) {
                buf[done..done + chunk].copy_from_slice(&page[page_off..page_off + chunk]);
            } else {
                buf[done..done + chunk].fill(0);
            }
            done += chunk;
        }
        Ok(n)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, u64> {
        let mut inner = self.inner.lock();
        if inner.is_dir {
            return Err(0);
        }
        let end = offset.saturating_add(buf.len());
        if end > inner.size {
            inner.size = end;
        }
        let mut done = 0;
        while done < buf.len() {
            let pos = offset + done;
            let page_no = (pos / TMPFS_PAGE) as u64;
            let page_off = pos % TMPFS_PAGE;
            let chunk = (TMPFS_PAGE - page_off).min(buf.len() - done);
            let page = inner.pages.entry(page_no).or_insert_with(|| {
                // Heap-allocated zeros (no 4 KiB stack temp).
                alloc::vec![0u8; TMPFS_PAGE]
                    .into_boxed_slice()
                    .try_into()
                    .map_err(|_| ())
                    .unwrap()
            });
            page[page_off..page_off + chunk].copy_from_slice(&buf[done..done + chunk]);
            done += chunk;
        }
        Ok(buf.len())
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        let inner = self.inner.lock();
        inner.children.get(name).map(|n| n.clone() as Arc<dyn FileNode>).ok_or(0)
    }

    fn create(&self, name: &str, is_dir: bool, mode: u32, uid: u32, gid: u32) -> Result<Arc<dyn FileNode>, u64> {
        let mut inner = self.inner.lock();
        if let Some(existing) = inner.children.get(name) {
            if existing.is_dir() == is_dir {
                return Ok(existing.clone());
            }
            return Err(0);
        }
        let node = Arc::new(if is_dir { TmpFsNode::new_dir(uid, gid, mode) } else { TmpFsNode::new_file(uid, gid, mode) });
        inner.children.insert(String::from(name), node.clone());
        Ok(node)
    }

    fn truncate(&self, size: usize) -> Result<(), u64> {
        let mut inner = self.inner.lock();
        if size < inner.size {
            // Drop fully-truncated pages; zero the tail of a straddled page
            // so bytes past EOF read back as zero after a later extend.
            let keep_pages = size.div_ceil(TMPFS_PAGE) as u64;
            inner.pages.retain(|&k, _| k < keep_pages);
            if size % TMPFS_PAGE != 0 {
                if let Some(page) = inner.pages.get_mut(&(keep_pages - 1)) {
                    page[size % TMPFS_PAGE..].fill(0);
                }
            }
        }
        inner.size = size;
        Ok(())
    }

    fn stat(&self) -> Stat {
        let inner = self.inner.lock();
        let mut st = Stat::default();
        // The file-type bits are fixed by what this node is; the permission bits
        // come from `chmod`, so a program that writes a mode and reads it back
        // sees its own value. Reporting a hardcoded 0777/0666 made every
        // `chmod` silently ineffective and, worse, meant a mode a program
        // believed it had tightened did not hold.
        st.st_mode = if inner.is_dir {
            0o40000 | (inner.mode & 0o7777)
        } else {
            0o100000 | (inner.mode & 0o7777)
        };
        st.st_uid = inner.uid;
        st.st_gid = inner.gid;
        st.st_size = inner.size as i64;
        st
    }

    fn chmod(&self, mode: u32) -> Result<(), u64> {
        let mut inner = self.inner.lock();
        // Linux keeps the file-type bits and replaces only the permission bits.
        inner.mode = mode & 0o7777;
        Ok(())
    }

    fn chown(&self, uid: u32, gid: u32) -> Result<(), u64> {
        let mut inner = self.inner.lock();
        // (u32::MAX, u32::MAX) is the "change nothing" sentinel that Linux uses
        // for each field independently.
        if uid != u32::MAX {
            inner.uid = uid;
        }
        if gid != u32::MAX {
            inner.gid = gid;
        }
        Ok(())
    }

    fn is_dir(&self) -> bool {
        self.inner.lock().is_dir
    }

    fn size(&self) -> usize {
        self.inner.lock().size
    }

    fn list_dir(&self) -> Result<Vec<(String, bool)>, u64> {
        let inner = self.inner.lock();
        Ok(inner
            .children
            .iter()
            .map(|(n, f)| (n.clone(), f.is_dir()))
            .collect())
    }

    fn unlink(&self, name: &str) -> Result<(), u64> {
        let mut inner = self.inner.lock();
        if let Some(child) = inner.children.get(name) {
            if child.is_dir() && !child.inner.lock().children.is_empty() {
                return Err(0); // ENOTEMPTY
            }
        }
        inner.children.remove(name).map(|_| ()).ok_or(0)
    }

    fn cache_id(&self) -> u64 {
        self.cache_id
    }
}
