//! VFAT (minimal read-only FAT12/16/32) for DragOS.
//! Backs `mount -t vfat /dev/part /mnt`.
//! Implements BPB parsing, FAT chain walks, root dir listing, file read.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use crate::fs::vfs::FileNode;
use crate::fs::mount::{Filesystem, MountSource};
use crate::fs::reader::SectorReader;
use crate::arch::x86_64::syscalls::{Stat, ENOSYS, EINVAL, ENOENT};

#[derive(Clone)]
pub struct VfatFs {
    reader: Arc<dyn SectorReader>,
    bytes_per_sector: u32,
    sectors_per_cluster: u8,
    reserved_sectors: u32,
    num_fats: u8,
    fat_size: u32,
    data_start_sector: u64,
    root_cluster: u32,
    fat_type: u32,
    total_sectors: u64,
    root_entries: u16,
}

impl VfatFs {
    pub fn open(reader: Arc<dyn SectorReader>) -> Option<Self> {
        let mut boot = [0u8; 512];
        reader.read_sector(0, &mut boot).ok()?;
        if boot[510] != 0x55 || boot[511] != 0xaa { return None; }

        let bps = u16::from_le_bytes([boot[11], boot[12]]) as u32;
        let spc = boot[13] as u32;
        let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u32;
        let num_fats = boot[16];
        let tot16 = u16::from_le_bytes([boot[19], boot[20]]) as u32;
        let fat_sz16 = u16::from_le_bytes([boot[22], boot[23]]) as u32;
        let tot32 = u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]);
        let fat_sz32 = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]);
        if bps == 0 || spc == 0 || num_fats == 0 { return None; }

        let total_sectors = if tot16 != 0 { tot16 } else { tot32 };
        let fat_sz = if fat_sz16 != 0 { fat_sz16 } else { fat_sz32 };
        let is_fat32 = (tot16 == 0 && fat_sz16 == 0) && (fat_sz32 != 0);
        let root_cluster = if is_fat32 {
            u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]])
        } else { 0 };
        let root_entries = if !is_fat32 { u16::from_le_bytes([boot[17], boot[18]]) } else { 0 };
        let data_start = (reserved + (num_fats as u32) * fat_sz) as u64;
        let fat_type = if is_fat32 {
            32
        } else if reserved == 32 {
            16
        } else {
            12
        };
        Some(Self {
            reader,
            bytes_per_sector: bps,
            sectors_per_cluster: spc as u8,
            reserved_sectors: reserved,
            num_fats,
            fat_size: fat_sz,
            data_start_sector: data_start * (bps as u64 / 512),
            root_cluster,
            fat_type,
            total_sectors: total_sectors as u64,
            root_entries,
        })
    }

    pub fn cluster_size(&self) -> usize {
        (self.sectors_per_cluster as usize) * (self.bytes_per_sector as usize)
    }
    pub fn fat_type(&self) -> u32 { self.fat_type }
    pub fn root_cluster(&self) -> u32 { self.root_cluster }
    pub fn is_end(&self, c: u32) -> bool {
        match self.fat_type {
            12 => c >= 0x0FF8,
            16 => c >= 0xFFF8,
            32 => c >= 0x0FFFFFF8,
            _ => true,
        }
    }
    pub fn fat_entry(&self, cluster: u32) -> u32 {
        let fat_off = cluster * (self.fat_type as u32 / 8);
        let sec = self.reserved_sectors as u64 + (fat_off as u64 / (self.bytes_per_sector as u64));
        let mut sector = alloc::vec![0u8; self.bytes_per_sector as usize];
        if self.reader.read_sector(sec, &mut sector).is_err() { return 0; }
        let off = (fat_off % self.bytes_per_sector) as usize;
        match self.fat_type {
            12 => (u16::from_le_bytes([sector[off], sector[off+1]]) & 0x0FFF) as u32,
            16 => u16::from_le_bytes([sector[off], sector[off+1]]) as u32,
            32 => u32::from_le_bytes([sector[off], sector[off+1], sector[off+2], sector[off+3]]) & 0x0FFFFFFF,
            _ => 0u32,
        }
    }
    pub fn cluster_lba(&self, cluster: u32) -> u64 {
        self.data_start_sector + (cluster as u64 - 2) * (self.sectors_per_cluster as u64)
    }
    pub fn read_cluster(&self, cluster: u32, buf: &mut [u8]) -> Result<(), &'static str> {
        let bs = self.bytes_per_sector as usize;
        let secs = self.sectors_per_cluster as usize;
        self.reader.read_sectors(self.cluster_lba(cluster), buf)
    }
    pub fn parse_dir_entry(&self, e: &[u8]) -> Option<(String, bool, u32, usize)> {
        if e[0] == 0 || e[0] == 0xE5 { return None; }
        let attr = e[11];
        if attr == 0x0F { return None; }
        if attr & 0x08 != 0 { return None; }
        let raw = &e[0..8];
        let ext = &e[8..11];
        let mut name = String::new();
        for &b in raw { if b == b' ' { break; } name.push(b as char); }
        if ext.iter().any(|&x| x != b' ') {
            name.push('.');
            for &b in ext { if b == b' ' { break; } name.push(b as char); }
        }
        if name.is_empty() { return None; }
        let hi = u16::from_le_bytes([e[20], e[21]]) as u32;
        let lo = u16::from_le_bytes([e[26], e[27]]) as u32;
        let cluster = (hi << 16) | lo;
        let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]) as usize;
        let is_dir = attr & 0x10 != 0;
        Some((name, is_dir, cluster, size))
    }
    pub fn list_entries(&self, first_cluster: u32, out: &mut Vec<(String, bool, u32, usize)>) {
        if self.fat_type == 32 {
            let mut cluster = if first_cluster == 0 { self.root_cluster } else { first_cluster };
            if cluster < 2 { cluster = self.root_cluster; }
            let mut visited = 0usize;
            loop {
                if visited > 10000 { break; }
                visited += 1;
                if cluster < 2 || self.is_end(cluster) { break; }
                let cl = self.cluster_size();
                let mut b = alloc::vec![0u8; cl];
                if self.read_cluster(cluster, &mut b).is_err() { break; }
                let mut did_entry = false;
                for i in (0..cl).step_by(32) {
                    if i + 32 > b.len() { break; }
                    let e = &b[i..i+32];
                    if e[0] == 0 { break; }
                    // LFN — skip but continue walking
                    if e[11] == 0x0F { continue; }
                    if let Some(ent) = self.parse_dir_entry(e) {
                        out.push(ent);
                        did_entry = true;
                    }
                }
                // if no entries parsed in this cluster and 0x00 terminator seen, stop
                let next = self.fat_entry(cluster);
                if next == cluster || next == 0 { break; }
                cluster = next;
            }
        } else {
            // FAT12/16: root fixed, DIR is at reserved sectors location
            let root_secs = (((self.root_entries as usize) * 32) + (self.bytes_per_sector as usize - 1)) / (self.bytes_per_sector as usize);
            if root_secs == 0 { return; }
            let mut buf = alloc::vec![0u8; root_secs * self.bytes_per_sector as usize];
            let base = self.reserved_sectors as u64;
            for i in 0..root_secs {
                let off = i * self.bytes_per_sector as usize;
                let _ = self.reader.read_sector(base + (i as u64), &mut buf[off..off + self.bytes_per_sector as usize]);
            }
            parse_dir(out, &buf, self);
        }
    }
}


fn extract_lfn(_slot: &[u8], _out: &mut [char], _base: usize) {
    // LFN decoding deferred.
}

fn parse_dir(out: &mut Vec<(String, bool, u32, usize)>, data: &[u8], fs: &VfatFs) {
    let mut lfn_storage = ['\0'; 255];
    let mut lfn_count = 0;
    let mut first_cluster_active = false;
    let mut first_cluster = 0u32;
    for i in (0..data.len()).step_by(32) {
        if i + 32 > data.len() { break; }
        let e = &data[i..i+32];
        if e[0] == 0 { break; }
        if e[0] == 0xE5 {
            // reset lfn on delete marker? keep
            continue;
        }
        if e[11] == 0x0F {
            // LFN slot
            let ord = (e[0] & 0x1F) as usize;
            if ord == 0 { continue; }
            let base = (ord - 1) * 13;
            if base + 13 <= 255 {
                extract_lfn(e, &mut lfn_storage, base);
                lfn_count = lfn_count.max(ord * 13);
            }
            continue;
        }
        // regular entry
        if e[11] & 0x08 != 0 { // volume label
            // reset
            lfn_storage = ['\0'; 255];
            lfn_count = 0;
            continue;
        }
        let raw = &e[0..8];
        let ext = &e[8..11];
        let mut name = String::new();
        for &b in raw { if b == b' ' { break; } name.push(b as char); }
        // LFN override
        if ext.iter().any(|&x| x != b' ') && name.chars().last().map(|c| c != '.').unwrap_or(true) {
            // append ext if not LFN
            if !name.is_empty() && !name.contains('.') {
                name.push('.');
                for &b in ext { if b == b' ' { break; } name.push(b as char); }
            }
        }
        if name.is_empty() || name == "." || name == ".." { 
            lfn_storage = ['\0';255]; lfn_count=0; continue;
        }
        let is_dir = e[11] & 0x10 != 0;
        let hi = u16::from_le_bytes([e[20], e[21]]) as u32;
        let lo = u16::from_le_bytes([e[26], e[27]]) as u32;
        let cluster = (hi << 16) | lo;
        let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]) as usize;
        out.push((name, is_dir, cluster, size));
        let _ = fs;
    }
}


#[derive(Clone)]
pub struct VfatDir {
    pub fs: Arc<VfatFs>,
    pub first_cluster: u32,
    pub is_root: bool,
    pub cached: alloc::sync::Arc<core::sync::atomic::AtomicUsize>,
}
#[derive(Clone)]
pub struct VfatFile {
    pub fs: Arc<VfatFs>,
    pub first_cluster: u32,
    pub size: usize,
}

impl FileNode for VfatDir {
    fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize,u64> { Err(crate::arch::x86_64::syscalls::EISDIR) }
    fn write_at(&self, _: usize, _: &[u8]) -> Result<usize,u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        let mut out=Vec::new();
        self.fs.list_entries(self.first_cluster,&mut out);
        for (n,is_dir,cluster,size) in out {
            if n.to_lowercase()==name.to_lowercase() {
                if is_dir {
                    return Ok(Arc::new(VfatDir{fs:self.fs.clone(),first_cluster:cluster,is_root:false,cached:alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0))}));
                } else {
                    return Ok(Arc::new(VfatFile{fs:self.fs.clone(),first_cluster:cluster,size}));
                }
            }
        }
        Err(crate::arch::x86_64::syscalls::ENOENT)
    }
    fn create(&self,_n:&str,_i:bool,_m:u32,_u:u32,_g:u32)->Result<Arc<dyn FileNode>,u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn truncate(&self,_:usize)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn stat(&self)->crate::arch::x86_64::syscalls::Stat{crate::arch::x86_64::syscalls::Stat::default()}
    fn is_dir(&self)->bool{true}
    fn size(&self)->usize{0}
    fn list_dir(&self)->Result<Vec<(String,bool)>,u64> {
        let mut out=Vec::new();
        self.fs.list_entries(self.first_cluster,&mut out);
        Ok(out.into_iter().map(|e|(e.0,e.1)).collect())
    }
    fn unlink(&self,_:&str)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn rename(&self,_:&str)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn link(&self,_n:&str,_t:Arc<dyn FileNode>)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn symlink(&self,_n:&str,_t:&str)->Result<Arc<dyn FileNode>,u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn chmod(&self,_m:u32)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn chown(&self,_uid:u32,_gid:u32)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn read_link(&self)->Result<String,u64>{Err(crate::arch::x86_64::syscalls::EINVAL)}
    fn cache_id(&self)->u64{0}
}

impl FileNode for VfatFile {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, u64> {
        let n = (self.size - offset).min(buf.len());
        if offset >= self.size { return Ok(0); }
        // Walk cluster chain
        let cl_size = self.fs.cluster_size();
        let mut data_off = 0usize;
        let mut cluster = self.first_cluster;
        let mut buf_pos = 0usize;
        let buf_target = offset + n;
        while !self.fs.is_end(cluster) && cluster != 0 && buf_pos < buf_target {
            if data_off + cl_size <= offset {
                data_off += cl_size;
                let nxt = self.fs.fat_entry(cluster);
                if nxt == 0 || nxt == cluster { break; }
                cluster = nxt;
                continue;
            }
            let mut cluster_data = alloc::vec![0u8; cl_size];
            if self.fs.read_cluster(cluster, &mut cluster_data).is_err() { break; }
            let start_in_cluster = offset.saturating_sub(data_off).min(cl_size);
            let end_in_cluster = (buf_target - data_off).min(cl_size).max(start_in_cluster);
            let chunk = end_in_cluster - start_in_cluster;
            if chunk > 0 && buf_pos + chunk <= buf.len() && start_in_cluster + chunk <= cluster_data.len() {
                buf[buf_pos..buf_pos + chunk].copy_from_slice(&cluster_data[start_in_cluster..start_in_cluster + chunk]);
                buf_pos += chunk;
            }
            data_off += cl_size;
            let nxt = self.fs.fat_entry(cluster);
            if nxt == 0 || nxt == cluster { break; }
            cluster = nxt;
        }
        Ok(buf_pos.min(n))
    }
    fn write_at(&self, _: usize, _: &[u8]) -> Result<usize, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn lookup(&self, _: &str) -> Result<Arc<dyn FileNode>, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn create(&self, _n: &str, _i: bool, _m: u32, _u: u32, _g: u32) -> Result<Arc<dyn FileNode>, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn truncate(&self, _: usize) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn stat(&self) -> crate::arch::x86_64::syscalls::Stat { crate::arch::x86_64::syscalls::Stat::default() }
    fn is_dir(&self) -> bool { false }
    fn size(&self) -> usize { self.size }
    fn list_dir(&self) -> Result<Vec<(String, bool)>, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn unlink(&self, _: &str) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn rename(&self, _: &str) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn link(&self, _n: &str, _t: Arc<dyn FileNode>) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn symlink(&self, _n: &str, _t: &str) -> Result<Arc<dyn FileNode>, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn chmod(&self, _m: u32) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn chown(&self, _uid: u32, _gid: u32) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn read_link(&self) -> Result<String, u64> { Err(crate::arch::x86_64::syscalls::EINVAL) }
    fn cache_id(&self) -> u64 { 0 }
}

pub struct VfatFilesystem;
impl Filesystem for VfatFilesystem {
    fn name(&self) -> &'static str { "vfat" }
    fn mount(&self, source: MountSource, _flags: u64) -> Result<Arc<dyn FileNode>, u64> {
        let reader: Arc<dyn SectorReader> = match source {
            MountSource::Block => Arc::new(crate::fs::reader::GlobalReader),
            MountSource::File(node) => {
                let size = node.size() as u64;
                Arc::new(crate::fs::reader::FileReader::new(node, size))
            }
            _ => return Err(crate::arch::x86_64::syscalls::EINVAL),
        };
        let fs = VfatFs::open(reader).ok_or(crate::arch::x86_64::syscalls::EINVAL)?;
        Ok(Arc::new(VfatDir { fs: Arc::new(fs.clone()), first_cluster: fs.root_cluster(), is_root: true, cached: alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0)) }))
    }
}

pub fn register() {
    let mut fs = crate::fs::mount::FILESYSTEMS.lock();
    if !fs.iter().any(|f| f.name() == "vfat") {
        fs.push(alloc::boxed::Box::new(VfatFilesystem));
    }
}
