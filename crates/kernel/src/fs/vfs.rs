use alloc::{collections::VecDeque, sync::Arc};

use crate::{
    arch::{sync::spinlock::SpinLock, x86_64::syscalls},
    tasks::{PendingBlock, block_current_thread, wake_pipe_waiters},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeEnd {
    Read,
    Write,
}

pub struct PipeBuffer {
    pub buffer: VecDeque<u8>,
    pub capacity: usize,
    pub read_token: Arc<()>,  // master; each live read-end VNode holds a clon
    pub write_token: Arc<()>, // master; each live write-end VNode holds a clon
}

impl PipeBuffer {
    pub fn new() -> Self {
        Self {
            buffer: VecDeque::new(),
            capacity: 65536, // 64KiB, like Linux
            read_token: Arc::new(()),
            write_token: Arc::new(()),
        }
    }
}

pub trait FileNode: Send + Sync {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, u64>;
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, u64>;
    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64>;
    /// Create a child entry. `mode` holds the permission bits (the caller's
    /// request, already umasked by the syscall layer); `uid`/`gid` are the
    /// owner the new entry should have (`perm::new_file_owner`). The
    /// implementor sets them on the new inode at creation time — there is no
    /// window in which the entry exists with the wrong owner or mode.
    fn create(&self, name: &str, is_dir: bool, mode: u32, uid: u32, gid: u32) -> Result<Arc<dyn FileNode>, u64>;
    fn truncate(&self, size: usize) -> Result<(), u64>;
    fn stat(&self) -> crate::arch::x86_64::syscalls::Stat;
    fn is_dir(&self) -> bool;
    fn size(&self) -> usize;
    /// Returns `(name, is_dir)` for each directory entry.
    fn list_dir(&self) -> Result<alloc::vec::Vec<(alloc::string::String, bool)>, u64>;
    /// Returns `(name, d_type)` for each directory entry. Default maps `is_dir` to `DT_DIR` or `DT_REG`.
    fn list_dir_types(&self) -> Result<alloc::vec::Vec<(alloc::string::String, u8)>, u64> {
        let list = self.list_dir()?;
        Ok(list
            .into_iter()
            .map(|(n, is_dir)| {
                let dt = if is_dir {
                    crate::fs::dirent::DT_DIR
                } else {
                    crate::fs::dirent::DT_REG
                };
                (n, dt)
            })
            .collect())
    }
    /// Remove a child entry (directories must be empty). Default unsupported.
    fn unlink(&self, _name: &str) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn rename(&self, _new_name: &str) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn link(&self, _name: &str, _target: alloc::sync::Arc<dyn FileNode>) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn symlink(&self, _name: &str, _target_path: &str) -> Result<alloc::sync::Arc<dyn FileNode>, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn chmod(&self, _mode: u32) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn chown(&self, _uid: u32, _gid: u32) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn read_link(&self) -> Result<alloc::string::String, u64> { Err(crate::arch::x86_64::syscalls::EINVAL) }
    /// Stable identity for the DSO cache. Default 0 = not cacheable.
    fn cache_id(&self) -> u64 { 0 }

    /// Parent-directory handle exposed by VFAT for cross-directory moves.
    fn vfat_dir_cluster(&self) -> Option<u32> { None }
    fn vfat_fs_ptr(&self) -> u64 { 0 }
    fn rename_with_target_parent(&self, _new_name: &str, _target_parent_cluster: Option<u32>) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
}

#[repr(C, packed)]
#[derive(Copy, Clone, Default)]
pub struct EpollEvent {
    pub events: u32,
    pub data: u64,
}

pub struct EpollInstance {
    pub interest: alloc::collections::BTreeMap<i32, EpollEvent>,
    pub last_reported_events: alloc::collections::BTreeMap<i32, EpollEvent>,
}

pub struct EventFdState {
    pub counter: u64,
    pub semaphore: bool,
    pub nonblock: bool,
}

pub struct TimerFdState {
    pub deadline_ns: u64,
    pub interval_ns: u64,
    pub expirations: u64,
    pub nonblock: bool,
}

/// State for an inotify instance.
pub struct InotifyState {
    /// Watches: wd -> (path, mask)
    pub watches: alloc::collections::BTreeMap<i32, (alloc::string::String, u32)>,
    /// Next watch descriptor to assign.
    pub next_wd: i32,
    /// Pending events queue (serialized inotify_event structs).
    pub events: alloc::collections::VecDeque<u8>,
    /// Whether the fd is non-blocking.
    pub nonblock: bool,
}

impl TimerFdState {
    pub fn update_expirations(&mut self, now_ns: u64) {
        if self.deadline_ns != 0 && now_ns >= self.deadline_ns {
            if self.interval_ns == 0 {
                if self.expirations == 0 {
                    self.expirations = 1;
                }
            } else {
                let diff = now_ns - self.deadline_ns;
                let count = (diff / self.interval_ns) + 1;
                self.expirations += count;
                self.deadline_ns += count * self.interval_ns;
            }
        }
    }
}

#[derive(Clone)]
pub enum VNode {
    InitrdFile {
        data: &'static [u8],
        cursor: usize,
    },
    BlockFile {
        start_sector: u64,
        size: usize,
        cursor: usize,
    },
    FsNode {
        node: Arc<dyn FileNode>,
        cursor: usize,
    },
    Directory {
        name: alloc::string::String,
        /// Position of the next entry to return from `getdents64` (index into the
        /// TAR entries that are immediate children of this directory).
        cursor: usize,
    },
    /// An io_uring ring descriptor.
    ///
    /// `slot` indexes the kernel's ring registry (`io_uring::RING_CTX`). The
    /// ring is not a DMA-BUF: it is a shared-memory ring the user maps with
    /// `mmap(2)` at the three standard io_uring file offsets.
    IoUring { slot: u32 },
    /// A PRIME DMA-BUF file descriptor created via DRM_IOCTL_PRIME_HANDLE_TO_FD.
    ///
    /// Backed by a DmaBufFile carrying the virtio-gpu resource / GEM handle info.
    /// Used for cross-process buffer sharing (e.g. Vulkan client -> mywima
    /// compositor via Wayland zwp_linux_dmabuf_v1 + SCM_RIGHTS).
    DmaBuf(Arc<SpinLock<crate::drivers::drm::types::DmaBufFile>>),
    Pipe {
        shared: Arc<SpinLock<PipeBuffer>>,
        end: PipeEnd,
        nonblock: bool,
        token: Arc<()>, // clones the token matching `end`
    },
    PcSpeakerDevice,
    DspAudioDevice,
    /// A text console. `vt` is 0 for `/dev/tty` and `/dev/tty0` -- meaning
    /// *whichever VT is current*, resolved at each access the way Linux's
    /// `/dev/tty0` works -- and 1..=MAX_VTS for `/dev/ttyN`, naming one VT.
    TtyDevice { vt: u16 },
    // ponytail: per-physical-device evdev node — minor resolved once at
    // lookup against the live INPUT_DEV table, carried here so read/ioctl/
    // poll/stat never re-parse paths (no TOCTOU on hotplug free).
    InputEventDevice { minor: usize },
    InputMiceDevice { minor: usize },
    FbDevice { cursor: usize },
    /// `/dev/dri/card0` (minor 0), `/dev/dri/renderD128` (minor 128):
    /// DRM/KMS control node (major 226). Minor matters: libdrm matches
    /// stat rdev against sysfs, so render-vs-primary must differ.
    DrmDevice { minor: u32 },
    /// `/dev/null`: reads return EOF, writes discard everything.
    NullDevice,
    /// `/dev/ttyS0`: the COM1 serial port itself.
    ///
    /// Distinct from `TtyDevice` on purpose. A `TtyDevice` write goes to the
    /// VT cell buffer plus debugcon, and *not* to the serial port -- see the
    /// note at the `TtyDevice` write arm, where feeding every console byte to a
    /// second sink was deliberately undone. But the automated test harness
    /// drives the guest over serial (`-serial mon:stdio`) and reads QEMU's
    /// stdout, so it can only observe what reaches COM1. Without this node no
    /// userspace stdout was reachable from the harness at all: `[SHELL_READY]`
    /// and every test's result line went to `uefi_debug.log` instead, and the
    /// harness timed out at the prompt.
    ///
    /// This is the serial console in its own right, so nothing else has to
    /// change behaviour to accommodate it.
    SerialDevice,
    /// `/dev/snd/controlC0D0`: mixer controls (volume, rate, switches).
    SndControlDevice,
    /// `/dev/snd/pcmC0D0p`: playback (write → DMA → codec).
    SndPlaybackDevice,
    /// `/dev/snd/pcmC0D0c`: capture (read → DMA → memory).
    SndCaptureDevice,
    Epoll(Arc<SpinLock<EpollInstance>>),
    UnixSocket(Arc<SpinLock<crate::arch::x86_64::syscalls::net::SockInner>>),
    InetSocket(Arc<SpinLock<crate::arch::x86_64::syscalls::net::InetInner>>),
    UdpSocket(Arc<SpinLock<crate::arch::x86_64::syscalls::net::UdpInner>>),
    EventFd(Arc<SpinLock<EventFdState>>),
    TimerFd(Arc<SpinLock<TimerFdState>>),
    Inotify(Arc<SpinLock<InotifyState>>),
    /// `/dev/fuse`: the FUSE (Filesystem in Userspace) communication channel.
    /// Userspace FUSE daemons open this device, then mount and serve a
    /// filesystem by reading FUSE requests and writing FUSE responses over
    /// this fd.
    FuseDevice,
    PtyMaster(Arc<SpinLock<crate::fs::pty::PtyPair>>),
    PtySlave(Arc<SpinLock<crate::fs::pty::PtyPair>>),
}

/// Identity allocation for the DSO cache.
///
/// The cache is keyed by (identity, page offset), so the identity has to be
/// unique for the file -- across *every* mounted filesystem, not just within
/// one. An inode number is unique within a filesystem only: the guest has the
/// root image, a second ext4 image (disk2.img) and the initrd mounted at once,
/// and they all number their inodes from one. The same (inode, offset) would
/// then name several different files, and the cache would serve one of them
/// for the others -- a page of the wrong file is how the dynamic loader comes
/// to read a library's dynamic section as garbage and dereference a null
/// pointer.
///
/// Filesystems therefore combine an id from here with their own inode number.
static NEXT_DSO_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// A fresh identity, unique for the rest of the kernel's life.
pub fn next_dso_id() -> u64 {
    NEXT_DSO_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/// The DSO cache identity of a file that has no filesystem of its own
/// (a tmpfs file, for instance): a plain counter.
pub fn standalone_dso_id() -> u64 {
    // Far above the (filesystem id << 32 | inode) space, so the two can
    // never meet.
    (1u64 << 48) | next_dso_id()
}

/// Stable DSO-cache identity for a vnode, or None for non-cacheable kinds.
pub fn vnode_cache_id(v: &VNode) -> Option<u64> {
    match v {
        VNode::FsNode { node, .. } => {
            let id = node.cache_id();
            if id == 0 { None } else { Some(id) }
        }
        VNode::InitrdFile { data, .. } => Some(data.as_ptr() as u64),
        VNode::BlockFile { start_sector, .. } => Some(*start_sector),
        VNode::SndControlDevice | VNode::SndPlaybackDevice | VNode::SndCaptureDevice => None,
        _ => None,
    }
}

impl VNode {
    pub fn rename(&self, new_name: &str) -> Result<(), u64> {
        match self {
            VNode::FsNode { node, .. } => node.rename(new_name),
            _ => Err(crate::arch::x86_64::syscalls::ENOSYS),
        }
    }
    pub fn link(&self, name: &str, target: alloc::sync::Arc<dyn FileNode>) -> Result<(), u64> {
        match self {
            VNode::FsNode { node, .. } => node.link(name, target),
            _ => Err(crate::arch::x86_64::syscalls::ENOSYS),
        }
    }
    pub fn symlink(&self, name: &str, target_path: &str) -> Result<alloc::sync::Arc<dyn FileNode>, u64> {
        match self {
            VNode::FsNode { node, .. } => node.symlink(name, target_path),
            _ => Err(crate::arch::x86_64::syscalls::ENOSYS),
        }
    }
    pub fn chmod(&self, mode: u32) -> Result<(), u64> {
        match self {
            VNode::FsNode { node, .. } => node.chmod(mode),
            // ponytail: Linux applies chown/chmod to devpts nodes (sshd's
            // pty_setowner fatals otherwise); store on the pair for stat.
            VNode::PtyMaster(pty) | VNode::PtySlave(pty) => {
                pty.lock().mode = mode & 0o7777;
                Ok(())
            }
            _ => Err(crate::arch::x86_64::syscalls::ENOSYS),
        }
    }
    pub fn chown(&self, uid: u32, gid: u32) -> Result<(), u64> {
        match self {
            VNode::UnixSocket(_) => return Ok(()), // handled above
            VNode::FsNode { node, .. } => node.chown(uid, gid),
            VNode::PtyMaster(pty) | VNode::PtySlave(pty) => {
                let mut p = pty.lock();
                if uid != u32::MAX {
                    p.uid = uid;
                }
                if gid != u32::MAX {
                    p.gid = gid;
                }
                Ok(())
            }
            _ => Err(crate::arch::x86_64::syscalls::ENOSYS),
        }
    }
    /// Size of the file this node refers to, in bytes.
    pub fn size(&self) -> usize {
        match self {
            VNode::FsNode { node, .. } => node.size(),
            VNode::InitrdFile { data, .. } => data.len(),
            VNode::BlockFile { size, .. } => *size,
            VNode::DmaBuf(db) => db.lock().size as usize,
            _ => 0,
        }
    }

    pub fn truncate(&self, size: usize) -> Result<(), u64> {
        match self {
            VNode::FsNode { node, .. } => node.truncate(size),
            _ => Err(crate::arch::x86_64::syscalls::ENOSYS),
        }
    }

    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, u64> {
        if let VNode::UnixSocket(sock) = self {
            return crate::arch::x86_64::syscalls::net::sock_read(
                sock,
                buf,
                false,
            );
        }
        if let VNode::InetSocket(inet) = self {
            return crate::arch::x86_64::syscalls::net::inet_read(
                inet,
                buf,
                false,
            );
        }
        if let VNode::UdpSocket(udp) = self {
            return crate::arch::x86_64::syscalls::net::udp_read(
                udp,
                buf,
                false,
            );
        }
        match self {
            VNode::UnixSocket(_) => return Ok(0usize), // handled above
            VNode::InetSocket(_) => return Ok(0usize), // handled above
            VNode::UdpSocket(_) => return Ok(0usize), // handled above
            // Write-only by design: serial *input* arrives via the COM1 IRQ
            // into the VT line discipline, not by a read(2) on this node. So a
            // read here is EOF rather than blocking -- a program that opens it
            // for output only must not hang on a stray read.
            VNode::SerialDevice => return Ok(0),
            VNode::InitrdFile { data, cursor } => {
                if *cursor >= data.len() {
                    return Ok(0); // EOF
                }
                let remaining = data.len() - *cursor;
                let to_copy = remaining.min(buf.len());

                buf[..to_copy].copy_from_slice(&data[*cursor..*cursor + to_copy]);
                *cursor += to_copy;
                Ok(to_copy)
            }
            VNode::BlockFile {
                start_sector,
                size,
                cursor,
            } => {
                if *cursor >= *size {
                    return Ok(0);
                }
                let remaining = *size - *cursor;
                let to_read = core::cmp::min(remaining, buf.len());

                let mut blk_guard = crate::drivers::virtio::blk::BLK_DEV.lock();
                if let Some(blk) = blk_guard.as_mut() {
                    let mut bytes_read = 0;
                    let mut bounce = [0u8; 512];

                    while bytes_read < to_read {
                        let current_offset = *cursor + bytes_read;
                        let sector_offset = (current_offset / 512) as u64;
                        let offset_in_sector = current_offset % 512;
                        let chunk = core::cmp::min(512 - offset_in_sector, to_read - bytes_read);

                        if blk
                            .read_sector(*start_sector + sector_offset, &mut bounce)
                            .is_err()
                        {
                            return Err(crate::arch::x86_64::syscalls::EIO);
                        }

                        buf[bytes_read..bytes_read + chunk]
                            .copy_from_slice(&bounce[offset_in_sector..offset_in_sector + chunk]);
                        bytes_read += chunk;
                    }
                    *cursor += bytes_read;
                    Ok(bytes_read)
                } else {
                    Err(crate::arch::x86_64::syscalls::EIO)
                }
            }
            VNode::FsNode { node, cursor } => match node.read_at(*cursor, buf) {
                Ok(bytes_read) => {
                    *cursor += bytes_read;
                    Ok(bytes_read)
                }
                Err(e) => Err(e),
            },
            VNode::Directory { .. } => Err(crate::arch::x86_64::syscalls::EISDIR),
            VNode::Pipe { .. } | VNode::PcSpeakerDevice | VNode::DspAudioDevice | VNode::NullDevice | VNode::SndPlaybackDevice | VNode::SndControlDevice | VNode::Epoll(..) | VNode::EventFd(..) | VNode::TimerFd(..) | VNode::IoUring { .. } | VNode::DmaBuf(..) | VNode::FuseDevice => Ok(0),
            VNode::Inotify(..) => Err(11), // EAGAIN - no events available
            VNode::SndCaptureDevice => {
                // Try HDA capture first, then AC97
                let hda_guard = crate::drivers::audio::hda::GLOBAL_HDA.lock();
                if hda_guard.is_some() {
                    drop(hda_guard);
                    crate::drivers::audio::vfs_node::hda_read_stream(buf)
                } else {
                    drop(hda_guard);
                    crate::drivers::audio::vfs_node::dsp_read(buf)
                }
            }
            VNode::PtyMaster(pty) => crate::fs::pty::pty_master_read(pty, buf, false),
            VNode::PtySlave(pty) => crate::fs::pty::pty_slave_read(pty, buf, false),
            VNode::TtyDevice { .. } => crate::arch::x86_64::syscalls::read_stdin_internal(buf),
            VNode::InputEventDevice { .. } | VNode::InputMiceDevice { .. } => {
                // Unreachable: eventN opens are always wrapped in OpenFile and
                // read via read_file (per-open stream). EAGAIN, never block.
                return Err(crate::arch::x86_64::syscalls::EAGAIN);
            }
            VNode::SndCaptureDevice => {
                // Try HDA capture first, then AC97
                let hda_guard = crate::drivers::audio::hda::GLOBAL_HDA.lock();
                if hda_guard.is_some() {
                    drop(hda_guard);
                    crate::drivers::audio::vfs_node::hda_read_stream(buf)
                } else {
                    drop(hda_guard);
                    crate::drivers::audio::vfs_node::dsp_read(buf)
                }
            }
            VNode::FbDevice { cursor } => {
                let info = unsafe { crate::graphics::FRAMEBUFFER_INFO };
                if let Some(fb) = info {
                    let total = (fb.height * fb.stride * 4) as usize;
                    if *cursor >= total {
                        return Ok(0);
                    }
                    let to_copy = (total - *cursor).min(buf.len());
                    let fb_ptr = (shared::boot::boot_info::HHDM_BASE.0 + fb.address.0 + *cursor as u64) as *const u8;
                    unsafe {
                        core::ptr::copy_nonoverlapping(fb_ptr, buf.as_mut_ptr(), to_copy);
                    }
                    *cursor += to_copy;
                    Ok(to_copy)
                } else {
                    Err(crate::arch::x86_64::syscalls::ENOSYS)
                }
            }
            VNode::DrmDevice { .. } => {
                // Compositor event channel: page-flip/vblank completion
                // events (drm_event_vblank), drained via read().
                let n = crate::drivers::drm::drain_events(buf);
                if n == 0 {
                    // Non-blocking semantics; callers poll() first.
                    return Err(crate::arch::x86_64::syscalls::EAGAIN);
                }
                Ok(n)
            }
        }
    }

    pub fn write(&mut self, buf: &[u8]) -> Result<usize, u64> {
        if let VNode::InetSocket(inet) = self {
            return crate::arch::x86_64::syscalls::net::inet_write(inet, buf, false);
        }
        if let VNode::UdpSocket(udp) = self {
            return crate::arch::x86_64::syscalls::net::udp_write(udp, buf, false);
        }
        match self {
            VNode::UnixSocket(_) => return Ok(0usize), // handled above
            VNode::InetSocket(_) => return Ok(0usize), // handled above
            VNode::UdpSocket(_) => return Ok(0usize), // handled above
            VNode::FsNode { node, cursor } => match node.write_at(*cursor, buf) {
                Ok(bytes_written) => {
                    *cursor += bytes_written;
                    Ok(bytes_written)
                }
                Err(e) => Err(e),
            },
            VNode::PcSpeakerDevice => crate::drivers::audio::vfs_node::pcspeaker_write(buf),
            VNode::DspAudioDevice => crate::drivers::audio::vfs_node::dsp_write(buf),
            VNode::SndPlaybackDevice => {
                // Try HDA first, then AC97
                let hda_guard = crate::drivers::audio::hda::GLOBAL_HDA.lock();
                if hda_guard.is_some() {
                    drop(hda_guard);
                    crate::drivers::audio::vfs_node::hda_write_stream(buf)
                } else {
                    drop(hda_guard);
                    crate::drivers::audio::vfs_node::dsp_write(buf)
                }
            }
            VNode::SndControlDevice => {
                // HDA mixer controls
                let hda_guard = crate::drivers::audio::hda::GLOBAL_HDA.lock();
                if hda_guard.is_some() {
                    drop(hda_guard);
                    Ok(crate::drivers::audio::vfs_node::hda_ioctl(0, 0) as usize)
                } else {
                    drop(hda_guard);
                    Ok(0)
                }
            }
            VNode::TtyDevice { vt } => {
                // Job control: a background process group writing the
                // controlling terminal gets SIGTTOU and the write is dropped,
                // rather than being allowed to paint over the foreground job.
                // Checked here (not in sys_write) so it applies to every write
                // route into a VT, including pty-backed sessions.
                if !crate::arch::x86_64::syscalls::tty_write_permitted(*vt) {
                    return Ok(0);
                }
                // Output belongs to the VT this descriptor names, not to
                // whichever happens to be on screen: a process holding
                // /dev/tty3 must not paint the console the user is looking at.
                let target = if *vt == 0 { crate::graphics::vt::active_vt_number() } else { *vt };
                // ponytail: NO KD_GRAPHICS guard here, deliberately removed.
                //
                // This used to discard every write when the console was in
                // graphics mode -- on the reasoning that a compositor owns the
                // framebuffer then. That reasoning is wrong for a VT: the VT's
                // cell buffer is *not* the framebuffer. In graphics mode the VT
                // simply is not being painted to the screen, but it still exists,
                // still accumulates content, and a client writing to it still
                // expects its bytes to be retained.
                //
                // Dropping them silently was actively harmful: `write` returned
                // `buf.len()`, so the client believed it had drawn, and zellij
                // started, initialised a session, spawned tabs, and rendered
                // every frame into a buffer nobody could read. Diagnosing that
                // took a VT-dump facility, because nothing else in the system
                // could see what a full-screen program had drawn.
                //
                // The VT-to-framebuffer decision belongs to `render`, which
                // already only runs in text mode. Buffering here and deciding
                // there keeps the two concerns separate.
                let cooked = crate::tty::output_postprocess(target, buf);
                let text = core::str::from_utf8(&cooked).unwrap_or("");
                // Write to the VT's cell buffer ONLY.
                //
                // This used to call both `tprint!` (the VT) and `print!` (the
                // serial/debugcon stream) for the active VT. The duplication is
                // what made a VT dump unreadable: every byte a full-screen
                // program drew also landed in the debug log, interleaved with
                // the kernel's own `[kms]`/`[input]` lines, so a dump of VT2
                // appeared to contain kernel stats. It was not the dump that
                // was wrong — the two sinks were being fed the same stream.
                //
                // A tty write is console output, not a log line. Anything that
                // genuinely wants both should say so explicitly.
                crate::graphics::vt::with_vt(target, |v| v.write_str(text));
                crate::print!("{}", text);
                Ok(buf.len())
            }
            // ponytail: the serial port, and the only path from userspace stdout
            // to the host. `SerialPort::write_str` already expands \n to \r\n,
            // which is what a terminal expects, so the bytes go out verbatim
            // otherwise.
            VNode::SerialDevice => {
                let mut port = crate::serial_macro::COM1_PORT.lock();
                for &b in buf {
                    port.write_byte(b);
                }
                Ok(buf.len())
            }
            VNode::PtyMaster(pty) => crate::fs::pty::pty_master_write(pty, buf, false),
            VNode::PtySlave(pty) => crate::fs::pty::pty_slave_write(pty, buf, false),
            VNode::FbDevice { cursor } => {
                let info = unsafe { crate::graphics::FRAMEBUFFER_INFO };
                if let Some(fb) = info {
                    let total = (fb.height * fb.stride * 4) as usize;
                    if *cursor >= total {
                        return Ok(0);
                    }
                    let to_copy = (total - *cursor).min(buf.len());
                    let fb_ptr = (shared::boot::boot_info::HHDM_BASE.0 + fb.address.0 + *cursor as u64) as *mut u8;
                    unsafe {
                        core::ptr::copy_nonoverlapping(buf.as_ptr(), fb_ptr, to_copy);
                    }
                    *cursor += to_copy;
                    Ok(to_copy)
                } else {
                    Err(crate::arch::x86_64::syscalls::ENOSYS)
                }
            }
            VNode::NullDevice => Ok(buf.len()), // writes to /dev/null are discarded
            _ => Err(crate::arch::x86_64::syscalls::EBADF), // Read-only or unsupported
        }
    }

    pub fn lseek(&mut self, offset: isize, whence: usize) -> isize {
        if matches!(self, VNode::UnixSocket(_)) {
            return 0; // Unix sockets are not seekable but historically return 0
        }
        if matches!(self, VNode::InetSocket(_) | VNode::UdpSocket(_) | VNode::PtyMaster(_) | VNode::PtySlave(_)) {
            return -29; // ESPIPE
        }
        match self {
            VNode::UnixSocket(_) => return 0, // not seekable
            VNode::InetSocket(_) | VNode::UdpSocket(_) | VNode::PtyMaster(_) | VNode::PtySlave(_) => return -29, // ESPIPE
            VNode::SerialDevice => return -29, // ESPIPE: a character device, not a file
            VNode::IoUring { .. } => return -29, // ESPIPE: a ring is not seekable
            VNode::DmaBuf(dma_buf) => {
                let dmabuf = dma_buf.lock();
                let s = dmabuf.size as isize;
                match whence {
                    0 => { // SEEK_SET
                        if offset == 0 { 0 } else { -1 }
                    }
                    1 => { // SEEK_CUR
                        if offset == 0 { 0 } else { -1 }
                    }
                    2 => { // SEEK_END
                        s + offset
                    }
                    _ => return -1,
                }
            }
            VNode::InitrdFile { data, cursor } => {
                let new_cursor = match whence {
                    0 => offset,                       // SEEK_SET
                    1 => *cursor as isize + offset,    // SEEK_CUR
                    2 => data.len() as isize + offset, // SEEK_END
                    _ => return -1,                    // Invalid whence
                };

                if new_cursor < 0 {
                    *cursor = 0;
                    0
                } else {
                    *cursor = data.len().min(new_cursor as usize);
                    *cursor as isize
                }
            }
            VNode::BlockFile { size, cursor, .. } => {
                let s = *size as isize;
                let new_cursor = match whence {
                    0 => offset,                    // SEEK_SET
                    1 => *cursor as isize + offset, // SEEK_CUR
                    2 => s + offset,                // SEEK_END
                    _ => return -1,
                };
                if new_cursor < 0 || new_cursor > s {
                    return -1;
                }
                *cursor = new_cursor as usize;
                new_cursor
            }
            VNode::FsNode { node, cursor } => {
                let s = node.size() as isize;
                let new_cursor = match whence {
                    0 => offset,                    // SEEK_SET
                    1 => *cursor as isize + offset, // SEEK_CUR
                    2 => s + offset,                // SEEK_END
                    _ => return -1,
                };
                if new_cursor < 0 || new_cursor > s {
                    return -1;
                }
                *cursor = new_cursor as usize;
                new_cursor
            }
            VNode::FbDevice { cursor } => {
                let info = unsafe { crate::graphics::FRAMEBUFFER_INFO };
                if let Some(fb) = info {
                    let s = (fb.height * fb.stride * 4) as isize;
                    let new_pos = match whence {
                        0 => offset,
                        1 => *cursor as isize + offset,
                        2 => s + offset,
                        _ => return -1,
                    };
                    if new_pos < 0 || new_pos > s {
                        return -1;
                    }
                    *cursor = new_pos as usize;
                    *cursor as isize
                } else {
                    -1
                }
            }
            VNode::Directory { .. } | VNode::Pipe { .. } | VNode::PcSpeakerDevice | VNode::DspAudioDevice | VNode::TtyDevice { .. } | VNode::InputEventDevice { .. } | VNode::InputMiceDevice { .. } | VNode::NullDevice | VNode::DrmDevice { .. } | VNode::SndControlDevice | VNode::SndPlaybackDevice | VNode::SndCaptureDevice | VNode::Epoll(..) | VNode::EventFd(..) | VNode::TimerFd(..) | VNode::Inotify(..) | VNode::FuseDevice => 0,
        }
    }

    pub fn read_vnode(vnode: &Arc<SpinLock<VNode>>, buf: &mut [u8]) -> Result<usize, u64> {
        let pipe_info = {
            let mut v = vnode.lock();
            match &mut *v {
                VNode::Pipe {
                    shared,
                    end,
                    nonblock,
                    ..
                } => Some((shared.clone(), *end, *nonblock)),
                _ => None,
            }
        };
        match pipe_info {
            Some((shared, end, nonblock)) => Self::read_pipe_impl(shared, end, nonblock, buf),
            None => {
                let mut v = vnode.lock();
                v.read(buf)
            }
        }
    }

    pub fn write_vnode(vnode: &Arc<SpinLock<VNode>>, buf: &[u8]) -> Result<usize, u64> {
        let pipe_info = {
            let mut v = vnode.lock();
            match &mut *v {
                VNode::Pipe {
                    shared,
                    end,
                    nonblock,
                    ..
                } => Some((shared.clone(), *end, *nonblock)),
                _ => None,
            }
        };
        match pipe_info {
            Some((shared, end, nonblock)) => Self::write_pipe_impl(shared, end, nonblock, buf),
            None => {
                let mut v = vnode.lock();
                v.write(buf)
            }
        }
    }

    pub fn read_pipe_impl(
        shared: Arc<SpinLock<PipeBuffer>>,
        end: PipeEnd,
        nonblock: bool,
        buf: &mut [u8],
    ) -> Result<usize, u64> {
        if end == PipeEnd::Write {
            return Err(syscalls::EBADF);
        }
        loop {
            let mut pipe_buf = shared.lock();
            if pipe_buf.buffer.is_empty() {
                if Arc::strong_count(&pipe_buf.write_token) == 1 {
                    return Ok(0); // EOF - all write ends closed
                }
                if nonblock {
                    return Err(syscalls::EAGAIN);
                }
                drop(pipe_buf);
                block_current_thread(PendingBlock::PipeWait);
                continue;
            }
            let mut n = 0;
            while n < buf.len() {
                match pipe_buf.buffer.pop_front() {
                    Some(b) => {
                        buf[n] = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            drop(pipe_buf);
            wake_pipe_waiters();
            return Ok(n);
        }
    }

    pub fn write_pipe_impl(
        shared: Arc<SpinLock<PipeBuffer>>,
        end: PipeEnd,
        nonblock: bool,
        buf: &[u8],
    ) -> Result<usize, u64> {
        if end == PipeEnd::Read {
            return Err(syscalls::EBADF);
        }
        loop {
            let mut pipe_buf = shared.lock();
            if Arc::strong_count(&pipe_buf.read_token) == 1 {
                return Err(syscalls::EPIPE);
            }
            if pipe_buf.buffer.len() == pipe_buf.capacity {
                if nonblock {
                    return Err(syscalls::EAGAIN);
                }
                drop(pipe_buf);
                block_current_thread(PendingBlock::PipeWait);
                continue;
            }
            let mut n = 0;
            for &b in buf {
                if pipe_buf.buffer.len() == pipe_buf.capacity {
                    break;
                }
                pipe_buf.buffer.push_back(b);
                n += 1;
            }
            drop(pipe_buf);
            wake_pipe_waiters();
            return Ok(n);
        }
    }
}

pub struct OpenFile {
    pub vnode: VNode,
    pub flags: u64,
    pub path: alloc::string::String,
}

impl OpenFile {
    pub fn clone_vnode(&self) -> VNode {
        self.vnode.clone()
    }

}

impl OpenFile {
    pub fn read(&mut self, buf: &mut [u8], key: usize) -> Result<usize, u64> {
        // O_NONBLOCK must be honored from the OpenFile (the VNode carries no
        // flags); libevdev reads until EAGAIN and deadlocks otherwise.
        // key is the OpenFile Arc pointer: selects this open's private evdev
        // stream, Linux-like (dup/fork share it). Device from our own path.
        if let VNode::InputEventDevice { minor } | VNode::InputMiceDevice { minor } = self.vnode {
            return Self::drain_input_events(key, minor, buf, self.flags & 0o4000 != 0);
        }
        if let VNode::UnixSocket(sock) = &self.vnode {
            return crate::arch::x86_64::syscalls::net::sock_read(
                sock,
                buf,
                self.flags & 0o4000 != 0,
            );
        }
        if let VNode::InetSocket(inet) = &self.vnode {
            return crate::arch::x86_64::syscalls::net::inet_read(
                inet,
                buf,
                self.flags & 0o4000 != 0,
            );
        }
        if let VNode::PtyMaster(pty) = &self.vnode {
            return crate::fs::pty::pty_master_read(pty, buf, self.flags & 0o4000 != 0);
        }
        if let VNode::PtySlave(pty) = &self.vnode {
            return crate::fs::pty::pty_slave_read(pty, buf, self.flags & 0o4000 != 0);
        }
        self.vnode.read(buf)
    }

    fn drain_input_events(key: usize, dev: usize, buf: &mut [u8], nonblock: bool) -> Result<usize, u64> {
        let event_size = core::mem::size_of::<crate::drivers::keyboard::InputEvent>();
        loop {
            let mut read_bytes = 0;
            {
                while read_bytes + event_size <= buf.len() {
                    if let Some(ev) = crate::drivers::keyboard::input_pop(key, dev) {
                        unsafe {
                            let src =
                                &ev as *const crate::drivers::keyboard::InputEvent as *const u8;
                            core::ptr::copy_nonoverlapping(
                                src,
                                buf[read_bytes..].as_mut_ptr(),
                                event_size,
                            );
                        }
                        read_bytes += event_size;
                    } else {
                        break;
                    }
                }
            }
            if read_bytes > 0 {
                // ponytail: TEMP-DIAG reader identity — with per-device
                // streams this should show each pid draining its own node.
                let pid = crate::tasks::current_thread().map(|t| t.process.lock().pid).unwrap_or(0);
                crate::dprintln!("[input-drain] pid={} dev={} events={}", pid, dev, read_bytes / event_size);
                return Ok(read_bytes);
            }
            if nonblock {
                return Err(crate::arch::x86_64::syscalls::EAGAIN);
            }
            crate::tasks::block_current_thread(crate::tasks::PendingBlock::Input { reader: key, dev });
        }
    }

    pub fn write(&mut self, buf: &[u8]) -> Result<usize, u64> {
        if let VNode::UnixSocket(sock) = &self.vnode {
            return crate::arch::x86_64::syscalls::net::sock_write(
                sock,
                buf,
                self.flags & 0o4000 != 0,
            );
        }
        if let VNode::InetSocket(inet) = &self.vnode {
            return crate::arch::x86_64::syscalls::net::inet_write(
                inet,
                buf,
                self.flags & 0o4000 != 0,
            );
        }
        if let VNode::PtyMaster(pty) = &self.vnode {
            return crate::fs::pty::pty_master_write(pty, buf, self.flags & 0o4000 != 0);
        }
        if let VNode::PtySlave(pty) = &self.vnode {
            return crate::fs::pty::pty_slave_write(pty, buf, self.flags & 0o4000 != 0);
        }
        // Handle O_APPEND (0o2000 = 1024)
        const O_APPEND: u64 = 0o2000;
        if (self.flags & O_APPEND) != 0 {
            if let VNode::FsNode { node, cursor } = &mut self.vnode {
                *cursor = node.size();
            }
        }
        self.vnode.write(buf)
    }

    pub fn read_file(file: &Arc<SpinLock<OpenFile>>, buf: &mut [u8]) -> Result<usize, u64> {
        let (vnode_clone, flags) = {
            let f = file.lock();
            (f.vnode.clone(), f.flags)
        };
        let nonblock = flags & 0o4000 != 0;
        let key = Arc::as_ptr(file) as usize;

        match vnode_clone {
            VNode::Pipe { shared, end, nonblock, .. } => {
                VNode::read_pipe_impl(shared, end, nonblock, buf)
            }
            VNode::InputEventDevice { minor } | VNode::InputMiceDevice { minor } => {
                Self::drain_input_events(key, minor, buf, nonblock)
            }
            VNode::UnixSocket(sock) => {
                crate::arch::x86_64::syscalls::net::sock_read(&sock, buf, nonblock)
            }
            VNode::InetSocket(inet) => {
                crate::arch::x86_64::syscalls::net::inet_read(&inet, buf, nonblock)
            }
            VNode::UdpSocket(udp) => {
                crate::arch::x86_64::syscalls::net::udp_read(&udp, buf, nonblock)
            }
            // Reading a directory is EISDIR, not EOF. Falling through to
            // `VNode::read` already does this, but the fallback must never be
            // reached with a *fabricated* vnode: a program that probes for a
            // file and gets a directory handle back will loop forever on a
            // read that keeps returning 0.
            VNode::Directory { .. } => Err(crate::arch::x86_64::syscalls::EISDIR),
            VNode::PtyMaster(pty) => {
                crate::fs::pty::pty_master_read(&pty, buf, nonblock)
            }
            VNode::PtySlave(pty) => {
                crate::fs::pty::pty_slave_read(&pty, buf, nonblock)
            }
            VNode::TtyDevice { vt } => {
                // Resolve vt 0 (the /dev/tty, /dev/tty0, /dev/console aliases)
                // per read, so a process that opened /dev/tty0 follows the user
                // across a VT switch. Reading drains only this VT's queue, which
                // is what makes per-VT input independent.
                crate::arch::x86_64::syscalls::read_tty_vt(vt, buf)
            }
            _ => {
                let mut f = file.lock();
                f.vnode.read(buf)
            }
        }
    }

    pub fn write_file(file: &Arc<SpinLock<OpenFile>>, buf: &[u8]) -> Result<usize, u64> {
        let (vnode_clone, flags) = {
            let f = file.lock();
            (f.vnode.clone(), f.flags)
        };
        let nonblock = flags & 0o4000 != 0;

        match vnode_clone {
            VNode::Pipe { shared, end, nonblock, .. } => {
                VNode::write_pipe_impl(shared, end, nonblock, buf)
            }
            VNode::UnixSocket(sock) => {
                crate::arch::x86_64::syscalls::net::sock_write(&sock, buf, nonblock)
            }
            VNode::InetSocket(inet) => {
                crate::arch::x86_64::syscalls::net::inet_write(&inet, buf, nonblock)
            }
            VNode::UdpSocket(udp) => {
                crate::arch::x86_64::syscalls::net::udp_write(&udp, buf, nonblock)
            }
            VNode::PtyMaster(pty) => {
                crate::fs::pty::pty_master_write(&pty, buf, nonblock)
            }
            VNode::PtySlave(pty) => {
                crate::fs::pty::pty_slave_write(&pty, buf, nonblock)
            }
            _ => {
                let mut f = file.lock();
                f.write(buf)
            }
        }
    }
}


