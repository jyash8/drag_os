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
    pub fn sync_all(&self) -> Result<(),()> { Ok(()) }


    pub fn move_vfat_entry(&self, file: &VfatFile, new_name: &str, target_parent_cluster: Option<u32>) -> Result<(), u64> {
        let leaf = new_name.trim_end_matches('/').rsplit('/').next().unwrap_or(new_name);
        if leaf.is_empty() || leaf == "." || leaf == ".." { return Err(crate::arch::x86_64::syscalls::EINVAL); }
        let src_parent = file.parent_cluster;
        let dst_parent = target_parent_cluster.unwrap_or(src_parent);
        let mut old_data = match self.read_dir_bytes(src_parent) {
            Ok(d) => d, Err(e) => return Err(e),
        };
        // Locate the source short entry by its first cluster.
        let mut old_run: Option<usize> = None;
        let mut old_lfn: usize = 0;
        let mut created_cluster = 0u32;
        let mut file_size_usize = file.size.load(core::sync::atomic::Ordering::SeqCst);
        let mut is_dir = false;
        let mut attr = 0x20u8;
        let clu = file.first_cluster.load(core::sync::atomic::Ordering::SeqCst);
        let nent = old_data.len() / 32;
        for i in 0..nent {
            if i + 1 > nent { break; }
            let p = i * 32;
            let e = &old_data[p..p+32];
            if e[0] == 0 || e[0] == 0xE5 { continue; }
            if e[11] == 0x0F { continue; }
            let hi = u16::from_le_bytes([e[20], e[21]]) as u32;
            let lo = u16::from_le_bytes([e[26], e[27]]) as u32;
            if (hi << 16) | lo == clu {
                created_cluster = clu;
                attr = e[11];
                is_dir = (attr & 0x10) != 0;
                file_size_usize = u32::from_le_bytes([e[28],e[29],e[30],e[31]]) as usize;
                old_run = Some(i);
                // count leading LFN
                let mut j = i;
                while j > 0 {
                    let prev = &old_data[(j-1)*32..j*32];
                    if prev[11] == 0x0F && prev[0] != 0 && prev[0] != 0xE5 { j -= 1; } else { break; }
                }
                old_lfn = i - j;
                // store actual slot start in a second run variable below by rewriting
                break;
            }
        }
        let run_start0 = match old_run {
            Some(i) => i.saturating_sub(old_lfn),
            None => return Err(crate::arch::x86_64::syscalls::ENOENT),
        };
        let run_len = old_lfn + 1;

        // Build a new alias for new_name.
        let (stem, ext) = if let Some(pos) = leaf.rfind('.') { (&leaf[..pos], &leaf[pos+1..]) } else { (leaf, "") };
        let mut short_name = [b' '; 8];
        for (i,b) in stem.bytes().take(8).enumerate() { short_name[i] = if b.is_ascii_lowercase() { b-32 } else { b }; }
        let mut short_ext = [b' '; 3];
        for (i,b) in ext.bytes().take(3).enumerate() { short_ext[i] = if b.is_ascii_lowercase() { b-32 } else { b }; }
        // Collision: try bare, then ~1..~9.
        loop {
            let mut cand_buf = alloc::string::String::new();
            for &b in &short_name { if b != b' ' { cand_buf.push(b as char); } }
            if !short_ext.iter().all(|&x|x==b' ') { cand_buf.push('.'); for &b in &short_ext { if b!=b' ' { cand_buf.push(b as char);} } }
            let mut existing = alloc::vec::Vec::new();
            self.list_entries(dst_parent, &mut existing);
            if !existing.iter().any(|(n,_,_,_)| n.to_uppercase()==cand_buf.to_uppercase()) { break; }
            short_name[6]=b'~';
            let digit = b'1'; short_name[7]=digit;
            short_name[5]=b' ';
        }
        // Build LFN entries if needed.
        let mut utf16: alloc::vec::Vec<u16> = alloc::vec::Vec::new();
        for ch in new_name.chars() { let mut buf=[0u16;2]; utf16.extend_from_slice(ch.encode_utf16(&mut buf)); }
        let need_lfn = !((leaf.chars().count() <= 8 && ext.bytes().count() <= 3));
        let lfn_count = if need_lfn { ((utf16.len() + 12) / 13).max(1) } else { 0 };
        let mut entries: alloc::vec::Vec<[u8;32]> = alloc::vec::Vec::new();
        // checksum
        let mut sum: u8 = 0;
        for byte in short_name.iter().chain(short_ext.iter()) {
            if sum & 0x01 != 0 { sum = (sum >> 1) + 0x80; } else { sum >>= 1; }
            sum = sum.wrapping_add(*byte);
        }
        let checksum = sum;
        for i in 0..lfn_count {
            let mut e=[0u8;32];
            let base=i*13;
            let mut put = |pos: usize,u:u16| { e[pos]=(u&0xff) as u8; e[pos+1]=(u>>8) as u8; };
            for k in 0..13 {
                let u=if base+k < utf16.len() { utf16[base+k] } else if base+k==utf16.len() { 0u16 } else { 0xffffu16 };
                if k<5 { put(1+k*2,u); }
                else if k<11 { put(14+(k-5)*2,u); }
                else { put(28+(k-11)*2,u); }
            }
            e[0] = if i + 1 == lfn_count { (lfn_count as u8) | 0x40 } else { (lfn_count - i) as u8 };
            e[11]=0x0f; e[12]=0; e[13]=checksum; e[26]=0; e[27]=0;
            entries.push(e);
        }
        let mut short=[0u8;32];
        short[..8].copy_from_slice(&short_name);
        short[8..11].copy_from_slice(&short_ext);
        short[11]=attr;
        let hi=(created_cluster >>16) as u16; let lo=created_cluster as u16;
        short[20]=(hi&0xff) as u8; short[21]=(hi>>8) as u8;
        short[26]=(lo&0xff) as u8; short[27]=(lo>>8) as u8;
        let sz=file_size_usize as u32;
        let b=sz.to_le_bytes();
        short[28]=b[0];short[29]=b[1];short[30]=b[2];short[31]=b[3];
        entries.push(short);
        let need = entries.len();

        // Read target directory.
        let mut target_data = match self.read_dir_bytes(dst_parent) {
            Ok(d)=>d, Err(e)=>return Err(e),
        };
        // Find contiguous free run.
        let slots_per = target_data.len()/32;
        let mut run_start = None;
        'outer: for i in 0..slots_per.saturating_sub(need) {
            let mut ok=true;
            for k in 0..need {
                let idx=(i+k)*32;
                if target_data[idx]!=0 && target_data[idx]!=0xE5 { ok=false; break; }
            }
            if ok { run_start=Some(i); break 'outer; }
        }
        let (chosen_cluster, target_start, append_bytes) = if dst_parent != 0 || self.fat_type==32 {
            if let Some(r)=run_start {
                (None,r,false)
            } else {
                // append
                let last = match self.last_dir_cluster(dst_parent) { Some(c)=>c, None=>return Err(crate::arch::x86_64::syscalls::ENOMEM) };
                let newc = self.allocate_cluster().ok_or(crate::arch::x86_64::syscalls::ENOMEM)?;
                self.set_fat_entry(last,newc);
                // zero cluster
                let cs=self.cluster_size();
                let mut z=alloc::vec![0u8;cs];
                let _=self.write_cluster(newc,&z);
                (Some(newc),0,true)
            }
        } else {
            // fixed root
            if run_start.is_none() { return Err(crate::arch::x86_64::syscalls::ENOMEM); }
            (None, run_start.unwrap(), false)
        };
        // If appended, write one cluster buffer, otherwise write full target directory bytes.
        if let Some(newc)=chosen_cluster {
            let mut cluster_buf=alloc::vec![0u8; self.cluster_size()];
            for (i,e) in entries.iter().enumerate() { for k in 0..32 { cluster_buf[i*32+k]=e[k]; } }
            let _=self.write_cluster(newc,&cluster_buf);
        } else if append_bytes == false {
            let start = (target_start * 32) as usize;
            for (i,e) in entries.iter().enumerate() { for k in 0..32 { target_data[start+i*32+k]=e[k]; } }
            // flush target directory (same parent)
            self.write_dir_bytes(dst_parent,&target_data)?;
        }
        // mark old entries deleted
        for k in 0..(old_lfn+1) {
            let idx=(run_start0+k)*32;
            if idx+32 <= old_data.len() { old_data[idx] = 0xE5; }
        }
        self.write_dir_bytes(src_parent,&old_data)?;
        // update file metadata
        // update entry_sector for non-append? irrelevant but try; leave.
        Ok(())
    }

    fn read_dir_bytes(&self, parent_cluster: u32) -> Result<alloc::vec::Vec<u8>,u64> {
        if self.fat_type != 32 && parent_cluster == 0 {
            let root_secs = (((self.root_entries as usize) * 32) + (self.bytes_per_sector as usize - 1)) / (self.bytes_per_sector as usize);
            let base = self.reserved_sectors as u64;
            let mut buf = alloc::vec![0u8; root_secs * self.bytes_per_sector as usize];
            for i in 0..root_secs {
                let off = i * self.bytes_per_sector as usize;
                let _ = self.reader.read_sector(base + i as u64, &mut buf[off..off + self.bytes_per_sector as usize]);
            }
            Ok(buf)
        } else {
            let first = if parent_cluster == 0 { self.root_cluster } else { parent_cluster };
            if first < 2 { return Err(crate::arch::x86_64::syscalls::ENOENT); }
            let mut out = alloc::vec::Vec::new();
            let mut cur = first;
            let mut visited=0usize;
            while visited < 10000 {
                visited +=1;
                if cur <2 || self.is_end(cur) { break; }
                let cl = self.cluster_size();
                let mut b=alloc::vec![0u8;cl];
                if self.read_cluster(cur,&mut b).is_err() { return Err(crate::arch::x86_64::syscalls::EIO); }
                out.extend_from_slice(&b);
                let nxt = self.fat_entry(cur);
                if nxt==0 || nxt==cur || self.is_end(nxt) { break; }
                cur=nxt;
            }
            Ok(out)
        }
    }

    fn write_dir_bytes(&self, parent_cluster: u32, data: &[u8]) -> Result<(),u64> {
        if self.fat_type != 32 && parent_cluster == 0 {
            let root_secs = (((self.root_entries as usize) * 32) + (self.bytes_per_sector as usize - 1)) / (self.bytes_per_sector as usize);
            let base = self.reserved_sectors as u64;
            for i in 0..root_secs {
                let off=i*self.bytes_per_sector as usize;
                let _ = self.reader.write_sector(base+i as u64,&data[off..off+self.bytes_per_sector as usize]);
            }
            Ok(())
        } else {
            let first = if parent_cluster == 0 { self.root_cluster } else { parent_cluster };
            let cl = self.cluster_size();
            if data.len()!=cl { return Err(crate::arch::x86_64::syscalls::EINVAL); }
            self.write_cluster(first,&data);
            Ok(())
        }
    }

    fn last_dir_cluster(&self, parent_cluster: u32) -> Option<u32> {
        let first = if parent_cluster==0 { if self.fat_type==32 { self.root_cluster } else { return None; } } else { parent_cluster };
        let mut cur=first; let mut visited=0; let mut last=Option::None;
        while cur >= 2 && cur < 0x0FFFFFFF && visited <10000 {
            visited+=1;
            last=Some(cur);
            if self.is_end(self.fat_entry(cur)) { break; }
            let n=self.fat_entry(cur);
            if n==0||n==cur { break; }
            cur=n;
        }
        last
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
            }
            parse_dir(out, &buf, self);
        }
    }
}

fn decode_lfn_slot(e: &[u8]) -> [u16;13] {
    let mut chars = [0u16;13];
    // layout: 1-10, 14-25, 28-31
    for i in 0..5 { chars[i] = u16::from_le_bytes([e[1 + i*2], e[2 + i*2]]); }
    for i in 0..6 { chars[5+i] = u16::from_le_bytes([e[14 + i*2], e[15 + i*2]]); }
    for i in 0..2 { chars[11+i] = u16::from_le_bytes([e[28 + i*2], e[29 + i*2]]); }
    chars
}

fn parse_dir(out: &mut Vec<(String, bool, u32, usize)>, data: &[u8], fs: &VfatFs) {
    let mut lfn = [0u16; 255];
    let mut have_lfn = false;
    for i in (0..data.len()).step_by(32) {
        if i + 32 > data.len() { break; }
        let e = &data[i..i+32];
        if e[0] == 0 { break; }
        if e[0] == 0xE5 {
            have_lfn = false;
            continue;
        }
        if e[11] == 0x0F {
            // LFN slot
            let ord = (e[0] & 0x1F) as usize;
            if ord == 0 { continue; }
            let base = (ord - 1) * 13;
            if base + 13 <= 255 {
                let chars = decode_lfn_slot(e);
                for j in 0..13 {
                    lfn[base + j] = chars[j];
                }
                have_lfn = true;
            }
            continue;
        }
        if e[11] & 0x08 != 0 {
            have_lfn = false;
            continue;
        }
        let mut name = String::new();
        if have_lfn {
            for &c in &lfn {
                if c == 0 || c == 0xFFFF { break; }
                if let Some(ch) = char::from_u32(c as u32) {
                    name.push(ch);
                }
            }
        }
        if name.is_empty() {
            let raw = &e[0..8];
            let ext = &e[8..11];
            for &b in raw { if b == b' ' { break; } name.push(b as char); }
            if ext.iter().any(|&x| x != b' ') {
                name.push('.');
                for &b in ext { if b == b' ' { break; } name.push(b as char); }
            }
        }
        if name.is_empty() || name == "." || name == ".." {
            have_lfn = false;
            continue;
        }
        let is_dir = e[11] & 0x10 != 0;
        let hi = u16::from_le_bytes([e[20], e[21]]) as u32;
        let lo = u16::from_le_bytes([e[26], e[27]]) as u32;
        let cluster = (hi << 16) | lo;
        let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]) as usize;
        out.push((name, is_dir, cluster, size));
        have_lfn = false;
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
    pub first_cluster: Arc<core::sync::atomic::AtomicU32>,
    pub parent_cluster: u32,
    pub size: Arc<core::sync::atomic::AtomicUsize>,
    pub entry_sector: u64,
    pub entry_offset: usize,
}

impl FileNode for VfatDir {
    fn read_at(&self, _: usize, _: &mut [u8]) -> Result<usize,u64> { Err(crate::arch::x86_64::syscalls::EISDIR) }
    fn write_at(&self, _: usize, _: &[u8]) -> Result<usize,u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn lookup(&self, name: &str) -> Result<Arc<dyn FileNode>, u64> {
        let mut out=Vec::new();
        self.fs.list_entries(self.first_cluster, &mut out);
        for (n,is_dir,cluster,size) in out {
            if n.to_lowercase()==name.to_lowercase() {
                if is_dir {
                    return Ok(Arc::new(VfatDir{fs:self.fs.clone(),first_cluster:cluster,is_root:false,cached:alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0))}));
                } else {
                    return Ok(Arc::new(VfatFile{fs:self.fs.clone(), first_cluster:alloc::sync::Arc::new(core::sync::atomic::AtomicU32::new(cluster)), parent_cluster:if self.is_root { 0 } else { self.first_cluster }, size:Arc::new(core::sync::atomic::AtomicUsize::new(size)), entry_sector:0, entry_offset:0}));
                }
            }
        }
        Err(crate::arch::x86_64::syscalls::ENOENT)
    }
    fn create(&self, name: &str, is_dir: bool, _mode: u32, _uid: u32, _gid: u32) -> Result<Arc<dyn FileNode>,u64> {
        use crate::arch::x86_64::syscalls::{EEXIST, EINVAL, ENOENT, ENOMEM, ENOSYS};
        if name.is_empty() || name.contains('/') || name == "." || name == ".." { return Err(EINVAL); }
        if self.lookup(name).is_ok() { return Err(EEXIST); }
        // 8.3 short-name conversion (basic, no VFAT LFN emulation on create).
        let (stem, ext) = if let Some(pos) = name.rfind('.') { (&name[..pos], &name[pos+1..]) } else { (name, "") };
        let mut short_name = [b' '; 8];
        for (i,b) in stem.bytes().take(8).enumerate() { short_name[i] = if b.is_ascii_lowercase() { b - 32 } else { b }; }
        let mut short_ext = [b' '; 3];
        for (i,b) in ext.bytes().take(3).enumerate() { short_ext[i] = if b.is_ascii_lowercase() { b - 32 } else { b }; }

        // LFN/alias collision guard: if the bare short name exists, append a
        // two-character "~N" suffix to the stem.
        let mut collisions_seen = 0u8;
        loop {
            let mut cand_buf = alloc::string::String::new();
            for &b in &short_name { if b != b' ' { cand_buf.push(b as char); } }
            if !short_ext.iter().all(|&x| x==b' ') {
                cand_buf.push('.');
                for &b in &short_ext { if b != b' ' { cand_buf.push(b as char); } }
            }
            let mut out = alloc::vec::Vec::new();
            self.fs.list_entries(self.first_cluster, &mut out);
            let exists = out.iter().any(|(n,_,_,_)| n.to_uppercase()==cand_buf.to_uppercase());
            if !exists { break; }
            collisions_seen += 1;
            if collisions_seen > 8 { break; }
            let digit = b'0' + collisions_seen;
            short_name[6] = b'~';
            short_name[7] = digit;
        }


        let mut created_cluster: u32 = 0;
        let mut sec_lba: u64 = 0;
        let mut slot_off: usize = 0;
        // Encode name as UTF-16 for long-file-name (LFN) entries.
        // Only emit LFN slots when the name does not fit the legacy 8.3 form.
        let mut utf16_units: alloc::vec::Vec<u16> = alloc::vec::Vec::new();
        for ch in name.chars() {
            let mut tmp = [0u16; 2];
            utf16_units.extend_from_slice(ch.encode_utf16(&mut tmp));
        }
        let name_chars = name.chars().count();
        let ext_len = ext.bytes().count();
        let need_lfn = !((name_chars <= 8 && ext_len <= 3) && true) ;
        // Checksum of the 8.3 short alias used by LFN slots.
        let lfn_checksum = {
            let mut sum: u8 = 0;
            for byte in short_name.iter().chain(short_ext.iter()) {
                if sum & 0x01 != 0 { sum = (sum >> 1) + 0x80; } else { sum >>= 1; }
                sum = sum.wrapping_add(*byte);
            }
            sum
        };
        let lfn_count = if need_lfn { ((utf16_units.len() + 12) / 13).max(1) } else { 0 };
        let slot_count = lfn_count + 1usize;

        // Build LFN slots (physical ordering; sequence 1..lfn_count).
        let mut lfn_entries: alloc::vec::Vec<[u8; 32]> = alloc::vec::Vec::with_capacity(lfn_count);
        for i in 0..lfn_count {
            let mut e = [0u8; 32];
            let seq = (i + 1) as u8;
            e[0] = if i + 1 == lfn_count { seq | 0x40 } else { seq };
            e[11] = 0x0f;
            e[13] = lfn_checksum;
            let base = i * 13usize;
            let mut put_char = |slot_off: usize, u: u16| {
                e[slot_off] = (u & 0xff) as u8;
                e[slot_off + 1] = (u >> 8) as u8;
            };
            for k in 0usize..13 {
                let unit = if base + k < utf16_units.len() {
                    utf16_units[base + k]
                } else if base + k == utf16_units.len() {
                    0u16
                } else {
                    0xffffu16
                };
                if k < 5 { put_char(1 + k * 2, unit); }
                else if k < 11 { put_char(14 + (k - 5) * 2, unit); }
                else { put_char(28 + (k - 11) * 2, unit); }
            }
            lfn_entries.push(e);
        }

        // Build short 8.3 entry template.
        let attr = if is_dir { 0x10u8 } else { 0x20u8 };
        let mut short_entry = [0u8; 32];
        short_entry[..8].copy_from_slice(&short_name);
        short_entry[8..11].copy_from_slice(&short_ext);
        short_entry[11] = attr;

        // Directory sector buffers and where the first appended slot begins.
        let bps = self.fs.bytes_per_sector as usize;
        let mut first_cluster: Option<u32> = None;
        let mut start_entry_idx: usize = 0;
        let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let mut base_lba: u64 = 0;
        let mut fixed_root = self.fs.fat_type != 32 && self.is_root;

        if fixed_root {
            let root_entries = self.fs.root_entries as usize;
            let root_secs = (root_entries * 32 + bps - 1) / bps;
            buf = alloc::vec![0u8; root_secs * bps];
            base_lba = self.fs.reserved_sectors as u64;
            for si in 0..root_secs {
                let off = si * bps;
                let _ = self.fs.reader.read_sector(base_lba + si as u64, &mut buf[off..off + bps]);
            }
            // Find a contiguous free run of slot_count entries in the fixed root.
            let mut run_start: Option<usize> = None;
            'findrun: for i in 0..(root_entries.saturating_sub(slot_count)) {
                let mut ok = true;
                for k in 0..slot_count {
                    if buf[(i + k) * 32] != 0x00 && buf[(i + k) * 32] != 0xE5 {
                        ok = false; break;
                    }
                }
                if ok { run_start = Some(i); break 'findrun; }
            }
            match run_start {
                Some(idx) => start_entry_idx = idx,
                None => {
                    return Err(crate::arch::x86_64::syscalls::ENOMEM);
                }
            }
        } else {
            // Walk the cluster chain searching for a contiguous free run.
            let mut first = if self.first_cluster == 0 { self.fs.root_cluster } else { self.first_cluster };
            if first < 2 { first = self.fs.root_cluster; }
            let mut chain: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
            let mut cur = first;
            let mut visited=0usize;
            while visited < 10000 {
                visited += 1;
                if cur < 2 || self.fs.is_end(cur) { break; }
                chain.push(cur);
                let n = self.fs.fat_entry(cur);
                if n==0 || n==cur || self.fs.is_end(n) { break; }
                cur=n;
            }
            if chain.is_empty() {
                return Err(crate::arch::x86_64::syscalls::ENOMEM);
            }
            let cl = self.fs.cluster_size();
            let entries_per_cluster = cl / 32;
            let mut chosen: bool = false;
            for cluster in &chain {
                let mut b = alloc::vec![0u8; cl];
                if self.fs.read_cluster(*cluster, &mut b).is_err() { continue; }
                for i in 0..entries_per_cluster.saturating_sub(slot_count) {
                    let mut ok = true;
                    for k in 0..slot_count {
                        if b[(i+k)* 32] != 0 && b[(i+k)*32] != 0xE5 { ok=false; break; }
                    }
                    if ok {
                        first_cluster = Some(*cluster);
                        start_entry_idx = i;
                        buf = b;
                        chosen = true; break;
                    }
                }
                if chosen { break; }
            }
            if !chosen {
                // Append a new cluster (enough room for all appended entries).
                let last = *chain.last().unwrap();
                let newc = self.fs.allocate_cluster().ok_or(crate::arch::x86_64::syscalls::ENOMEM)?;
                self.fs.set_fat_entry(last, newc);
                let n2 = newc;
                first_cluster = Some(n2);
                start_entry_idx = 0;
                buf = alloc::vec![0u8; self.fs.cluster_size()];
                chosen = true;
            }
            let cl = self.fs.cluster_size();
            base_lba = self.fs.cluster_lba(first_cluster.unwrap());
            // target cluster global mapping
            let _ = cl;
        }

        // Allocate short-entry first-cluster for directories.
        created_cluster = 0;
        if is_dir {
            let c = self.fs.allocate_cluster().ok_or(crate::arch::x86_64::syscalls::ENOMEM)?;
            created_cluster = c;
            let hi = (c >> 16) as u16;
            let lo = c as u16;
            short_entry[20] = (hi & 0xff) as u8; short_entry[21] = (hi >> 8) as u8;
            short_entry[26] = (lo & 0xff) as u8; short_entry[27] = (lo >> 8) as u8;
            let cs = self.fs.cluster_size();
            let z = alloc::vec![0u8; cs];
            let _ = self.fs.write_cluster(c, &z);
        }

        // Append LFN + short entry bytes.
        let mut short_entry_erased = short_entry;
        for k in 0..32 {
            let slot = start_entry_idx * 32 + k;
            buf[slot] = 0;
        }
        for (i, e) in lfn_entries.iter().enumerate() {
            let off = (start_entry_idx + i) * 32;
            for k in 0..32 { buf[off + k] = e[k]; }
        }
        let off_short = (start_entry_idx + lfn_count) * 32;
        for k in 0..32 { buf[off_short + k] = short_entry_erased[k]; }

        if fixed_root {
            // Write all root sectors back.
            for si in 0..(buf.len() / bps) {
                let _ = self.fs.reader.write_sector(base_lba + si as u64, &buf[si * bps..(si + 1) * bps]);
            }
            let final_byte_off = off_short;
            sec_lba = base_lba + (final_byte_off / bps) as u64;
            slot_off = final_byte_off % bps;
        } else {
            let cl = self.fs.cluster_size();
            // write cluster
            let _ = self.fs.write_cluster(first_cluster.unwrap(), &buf);
            let final_byte_off = off_short;
            let sec_in_cluster = final_byte_off / bps;
            sec_lba = self.fs.cluster_lba(first_cluster.unwrap()) + sec_in_cluster as u64;
            slot_off = final_byte_off % bps;
        }
        let entry_cluster = created_cluster;
        // Preserve old final short writes
        let _ = (sec_lba, slot_off);
        let entry_cluster = created_cluster;
        let ent_size = 0usize;
        let parent = if self.is_root { 0 } else { self.first_cluster };
        Ok(Arc::new(VfatFile{fs:self.fs.clone(), first_cluster: alloc::sync::Arc::new(core::sync::atomic::AtomicU32::new(entry_cluster)), parent_cluster:parent, size:alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(ent_size)), entry_sector:sec_lba, entry_offset:slot_off}))
    }
    fn truncate(&self,_:usize)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn stat(&self)->crate::arch::x86_64::syscalls::Stat{crate::arch::x86_64::syscalls::Stat::default()}
    fn is_dir(&self)->bool{true}
    fn size(&self)->usize{0}
    fn list_dir(&self)->Result<Vec<(String,bool)>,u64> {
        let mut out=Vec::new();
        self.fs.list_entries(self.first_cluster, &mut out);
        Ok(out.into_iter().map(|e|(e.0,e.1)).collect())
    }
    fn unlink(&self, name: &str) -> Result<(),u64> {
        use crate::arch::x86_64::syscalls::{ENOENT, ENOSYS};
        if name.is_empty() || name=="." || name==".." { return Err(ENOENT); }
        // FAT32 root/non-root case in one walk plus FAT12/16.
        let mut target_sector: Option<u64> = None;
        let mut target_off: usize = 0;
        if self.fs.fat_type != 32 && self.is_root {
            let root_secs = (((self.fs.root_entries as usize) * 32) + (self.fs.bytes_per_sector as usize - 1)) / (self.fs.bytes_per_sector as usize);
            let base = self.fs.reserved_sectors as u64;
            let target= name.to_uppercase();
            for sidx in 0..root_secs {
                let mut sec=[0u8;512];
                if self.fs.reader.read_sector(base+sidx as u64,&mut sec).is_err() { continue; }
                for i in (0..512).step_by(32) {
                    if sec[i]==0 || sec[i]==0xE5 { if sec[i]==0 { break; } continue; }
                    if sec[i+11]==0x0F { continue; }
                    if let Some((n,_,first,_)) = self.fs.parse_dir_entry(&sec[i..i+32]) {
                        if n.to_uppercase()==target {
                            target_sector=Some(base+sidx as u64); target_off=i; // need bind first
                            let _=first;
                        }
                    }
                }
                if target_sector.is_some() { break; }
            }
        } else {
            let mut first = if self.first_cluster==0 { self.fs.root_cluster } else { self.first_cluster };
            if first<2 { first=self.fs.root_cluster; }
            let mut cur=first;
            let mut visited=0usize;
            'outer: while cur>=2 && visited<10000 {
                visited+=1;
                if self.fs.is_end(cur) { break; }
                let cl=self.fs.cluster_size();
                let mut b=alloc::vec![0u8;cl];
                if self.fs.read_cluster(cur,&mut b).is_err() { break; }
                let target=name.to_uppercase();
                for i in (0..cl).step_by(32) {
                    if i+32>b.len() { break; }
                    let e=&b[i..i+32];
                    if e[0]==0 { break; }
                    if e[0]==0xE5 { continue; }
                    if e[11]==0x0F { continue; }
                    if let Some((n,_,_fc,_))=self.fs.parse_dir_entry(e) {
                        if n.to_uppercase()==target {
                            let sec=self.fs.cluster_lba(cur)+ (i as u64 / self.fs.bytes_per_sector as u64);
                            target_sector=Some(sec); target_off=i % self.fs.bytes_per_sector as usize;
                            break 'outer;
                        }
                    }
                }
                let nxt=self.fs.fat_entry(cur);
                if nxt==0 || nxt==cur || self.fs.is_end(nxt) { break; }
                cur=nxt;
            }
        }
        let (sec, off) = match target_sector { Some(s)=>(s,target_off), None=>return Err(ENOENT) };
        // mark deleted
        let mut sector=[0u8;512];
        if self.fs.reader.read_sector(sec,&mut sector).is_err() { return Err(ENOSYS); }
        sector[off]=0xE5;
        self.fs.reader.write_sector(sec,&sector).map_err(|_|ENOSYS)?;
        Ok(())
    }
    fn rename(&self, _new_name: &str) -> Result<(),u64> {
        // Best-effort rename restricted to overwrite short name parts.
        // A full implementation with collision detection and LFN adjustment
        // is deferred.
        Err(crate::arch::x86_64::syscalls::ENOSYS)
    }
    fn link(&self,_n:&str,_t:Arc<dyn FileNode>)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn symlink(&self,_n:&str,_t:&str)->Result<Arc<dyn FileNode>,u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn chmod(&self,_m:u32)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn chown(&self,_uid:u32,_gid:u32)->Result<(),u64>{Err(crate::arch::x86_64::syscalls::ENOSYS)}
    fn read_link(&self)->Result<String,u64>{Err(crate::arch::x86_64::syscalls::EINVAL)}
    fn cache_id(&self)->u64{0}
    fn vfat_dir_cluster(&self)->Option<u32>{Some(self.first_cluster)}
    fn vfat_fs_ptr(&self)->u64{Arc::as_ptr(&self.fs) as u64}
}

impl FileNode for VfatFile {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, u64> {
        let size = self.size.load(core::sync::atomic::Ordering::SeqCst);
        let n = (size - offset).min(buf.len());
        if offset >= size { return Ok(0); }
        // Walk cluster chain
        let cl_size = self.fs.cluster_size();
        let mut data_off = 0usize;
        let mut cluster = self.first_cluster.load(core::sync::atomic::Ordering::SeqCst);
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
    fn write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, u64> { self.fs.write_wrap(self, offset, buf) }
    fn lookup(&self, _: &str) -> Result<Arc<dyn FileNode>, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn create(&self, _n: &str, _i: bool, _m: u32, _u: u32, _g: u32) -> Result<Arc<dyn FileNode>, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn truncate(&self, size: usize) -> Result<(), u64> { self.fs.truncate_wrap(self, size) }
    fn stat(&self) -> crate::arch::x86_64::syscalls::Stat { let mut st=crate::arch::x86_64::syscalls::Stat::default(); st.st_size=self.size.load(core::sync::atomic::Ordering::SeqCst) as i64; st }
    fn is_dir(&self) -> bool { false }
    fn size(&self) -> usize { self.size.load(core::sync::atomic::Ordering::SeqCst) }
    fn list_dir(&self) -> Result<Vec<(String, bool)>, u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn unlink(&self, _: &str) -> Result<(), u64> { Err(crate::arch::x86_64::syscalls::ENOSYS) }
    fn rename(&self, new_name: &str) -> Result<(), u64> { self.fs.rename_wrap(self, new_name) }
    fn vfat_fs_ptr(&self) -> u64 { Arc::as_ptr(&self.fs) as u64 }
    fn vfat_dir_cluster(&self) -> Option<u32> { Some(self.parent_cluster) }
    fn rename_with_target_parent(&self, new_name: &str, target_parent_cluster: Option<u32>) -> Result<(), u64> {
        if target_parent_cluster.is_none() || target_parent_cluster == Some(self.parent_cluster) {
            self.fs.rename_wrap(self,new_name)
        } else {
            self.fs.move_vfat_entry(self,new_name,target_parent_cluster)
        }
    }
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
        *crate::fs::MOUNTED_VFAT.lock() = Some(Arc::new(fs.clone()));
        Ok(Arc::new(VfatDir { fs: Arc::new(fs.clone()), first_cluster: fs.root_cluster(), is_root: true, cached: alloc::sync::Arc::new(core::sync::atomic::AtomicUsize::new(0)) }))
    }
}

pub fn register() {
    let mut fs = crate::fs::mount::FILESYSTEMS.lock();
    if !fs.iter().any(|f| f.name() == "vfat") {
        fs.push(alloc::boxed::Box::new(VfatFilesystem));
    }
}

impl VfatFs {
    pub fn eoc_value(&self) -> u32 {
        match self.fat_type {
            12 => 0x0FFF,
            16 => 0xFFFF,
            32 => 0x0FFFFFFF,
            _ => 0xFFFFFFFF,
        }
    }
    pub fn set_fat_entry(&self, cluster: u32, value: u32) -> bool {
        let fat_off = cluster * (self.fat_type / 8);
        let sec = self.reserved_sectors as u64 + ((fat_off as u64) / (self.bytes_per_sector as u64));
        let off = (fat_off % self.bytes_per_sector) as usize;
        let mut sector = [0u8; 512];
        if self.reader.read_sector(sec, &mut sector).is_err() { return false; }
        match self.fat_type {
            12 => {
                let word_off = off & !1;
                let mut word = u16::from_le_bytes([sector[word_off], sector[word_off + 1]]);
                if cluster % 2 == 0 {
                    word = (word & 0xF000) | (value as u16 & 0x0FFF);
                } else {
                    word = (word & 0x000F) | (((value & 0x0FFF) as u16) << 4);
                }
                let le = word.to_le_bytes();
                sector[word_off] = le[0];
                sector[word_off + 1] = le[1];
            }
            16 => {
                let le = (value as u16).to_le_bytes();
                sector[off] = le[0];
                sector[off + 1] = le[1];
            }
            32 => {
                let le = value.to_le_bytes();
                sector[off] = le[0];
                sector[off + 1] = le[1];
                sector[off + 2] = le[2];
                sector[off + 3] = le[3];
            }
            _ => return false,
        }
        self.reader.write_sector(sec, &sector).is_ok()
    }
    pub fn locate_entry(&self, parent_cluster: u32, target_first: u32) -> Option<(u64, usize)> {
        if self.fat_type == 32 {
            let mut cluster = if parent_cluster == 0 { self.root_cluster } else { parent_cluster };
            if cluster < 2 { cluster = self.root_cluster; }
            let mut visited = 0usize;
            loop {
                if visited > 10000 { break; }
                visited += 1;
                if cluster < 2 || self.is_end(cluster) { break; }
                let cl = self.cluster_size();
                let mut b = alloc::vec![0u8; cl];
                if self.read_cluster(cluster, &mut b).is_err() { break; }
                for i in (0..cl).step_by(32) {
                    if i + 32 > b.len() { break; }
                    let e = &b[i..i + 32];
                    if e[0] == 0 || e[0] == 0xE5 { continue; }
                    if e[11] == 0x0F { continue; }
                    let hi = u16::from_le_bytes([e[20], e[21]]) as u32;
                    let lo = u16::from_le_bytes([e[26], e[27]]) as u32;
                    if (hi << 16) | lo == target_first {
                        let sec = self.cluster_lba(cluster) + (i as u64 / self.bytes_per_sector as u64);
                        let off = i % self.bytes_per_sector as usize;
                        return Some((sec, off));
                    }
                }
                let next = self.fat_entry(cluster);
                if next == 0 || next == cluster || self.is_end(next) { break; }
                cluster = next;
            }
            None
        } else {
            let root_secs = (((self.root_entries as usize) * 32) + (self.bytes_per_sector as usize - 1)) / (self.bytes_per_sector as usize);
            let base = self.reserved_sectors as u64;
            for sidx in 0..root_secs {
                let mut sec = [0u8; 512];
                if self.reader.read_sector(base + sidx as u64, &mut sec).is_err() { continue; }
                for i in (0..512).step_by(32) {
                    let e = &sec[i..i + 32];
                    if e[0] == 0 || e[0] == 0xE5 { continue; }
                    if e[11] == 0x0F { continue; }
                    let hi = u16::from_le_bytes([e[20], e[21]]) as u32;
                    let lo = u16::from_le_bytes([e[26], e[27]]) as u32;
                    if (hi << 16) | lo == target_first {
                        return Some((base + sidx as u64, i));
                    }
                }
            }
            None
        }
    }
    pub fn write_cluster(&self, cluster: u32, data: &[u8]) -> Result<(), &'static str> {
        self.reader.write_sectors(self.cluster_lba(cluster), data)
    }
    pub fn write_wrap(&self, file: &VfatFile, offset: usize, buf: &[u8]) -> Result<usize, u64> {
        use crate::arch::x86_64::syscalls::ENOMEM;
        if buf.is_empty() { return Ok(0); }
        let cl_size = self.cluster_size();
        let old_size = file.size.load(core::sync::atomic::Ordering::SeqCst);
        let target_end = offset.saturating_add(buf.len());
        if target_end < offset { return Ok(0); }
        let needed = (target_end.max(old_size) + cl_size - 1) / cl_size;
        // Gather existing chain
        let mut chain: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
        let mut cur = file.first_cluster.load(core::sync::atomic::Ordering::SeqCst);
        let mut visited = 0usize;
        while chain.len() < needed && cur >= 2 && visited < 100000 {
            visited += 1;
            chain.push(cur);
            if self.is_end(self.fat_entry(cur)) { break; }
            let next = self.fat_entry(cur);
            if next == 0 || next == cur { break; }
            cur = next;
        }
        let first0 = if chain.is_empty() {
            file.first_cluster.load(core::sync::atomic::Ordering::SeqCst)
        } else { 0 };
        // Allocate as needed
        while chain.len() < needed {
            let c = self.allocate_cluster().ok_or(ENOMEM)?;
            if chain.is_empty() {
                // first cluster newly allocated
            } else {
                let prev = *chain.last().unwrap();
                self.set_fat_entry(prev, c);
            }
            chain.push(c);
        }
        if file.first_cluster.load(core::sync::atomic::Ordering::SeqCst) == 0 && !chain.is_empty() {
            file.first_cluster.store(chain[0], core::sync::atomic::Ordering::SeqCst);
        }
        let first_cluster = if file.first_cluster.load(core::sync::atomic::Ordering::SeqCst) != 0 {
            file.first_cluster.load(core::sync::atomic::Ordering::SeqCst)
        } else if !chain.is_empty() { chain[0] } else { 0 };
        if first_cluster == 0 { return Err(ENOMEM); }
        // if initial had no chain and file.first_cluster update above, chain[0] remains new cluster
        // read-modify-write each overlapping cluster
        for idx in 0..chain.len() {
            let cluster = chain[idx];
            let start = idx * cl_size;
            let end = start + cl_size;
            let overlap_lo = offset.max(start);
            let overlap_hi = target_end.min(end);
            if overlap_hi <= overlap_lo { continue; }
            let mut data = alloc::vec![0u8; cl_size];
            let _ = self.read_cluster(cluster, &mut data);
            let gap = overlap_lo - start;
            let cnt = overlap_hi - overlap_lo;
            let src_start = overlap_lo - offset;
            data[gap..gap + cnt].copy_from_slice(&buf[src_start..src_start + cnt]);
            let _ = self.write_cluster(cluster, &data);
        }
        let new_size = target_end.max(old_size);
        file.size.store(new_size, core::sync::atomic::Ordering::SeqCst);
        // best-effort parent entry update
        let target_first = file.first_cluster.load(core::sync::atomic::Ordering::SeqCst);
        if let Some((sec, off)) = self.locate_entry(file.parent_cluster, target_first) {
            let mut sector = [0u8; 512];
            if self.reader.read_sector(sec, &mut sector).is_ok() {
                let hi = (target_first >> 16) & 0xFFFF;
                let lo = target_first & 0xFFFF;
                sector[off + 20] = (hi & 0xFF) as u8;
                sector[off + 21] = ((hi >> 8) & 0xFF) as u8;
                sector[off + 26] = (lo & 0xFF) as u8;
                sector[off + 27] = ((lo >> 8) & 0xFF) as u8;
                let sz = new_size as u32;
                let b = sz.to_le_bytes();
                sector[off + 28] = b[0];
                sector[off + 29] = b[1];
                sector[off + 30] = b[2];
                sector[off + 31] = b[3];
                let _ = self.reader.write_sector(sec, &sector);
            }
        }
        Ok(buf.len())
    }
    pub fn allocate_cluster(&self) -> Option<u32> {
        let max = (self.total_sectors.saturating_sub(self.data_start_sector) / self.sectors_per_cluster as u64).saturating_add(2) as u32;
        for c in 2u32..=max {
            if self.fat_entry(c) == 0 {
                if self.set_fat_entry(c, self.eoc_value()) { return Some(c); }
            }
        }
        None
    }
    pub fn truncate_wrap(&self, file: &VfatFile, new_size: usize) -> Result<(), u64> {
        let size = file.size.load(core::sync::atomic::Ordering::SeqCst);
        if new_size >= size {
            file.size.store(new_size, core::sync::atomic::Ordering::SeqCst);
            return Ok(());
        }
        let cl_size = self.cluster_size();
        let needed = (new_size + cl_size - 1) / cl_size;
        // gather chain
        let mut chain: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
        let mut cur = file.first_cluster.load(core::sync::atomic::Ordering::SeqCst);
        let mut visited = 0usize;
        while cur >= 2 && visited < 100000 {
            visited += 1;
            chain.push(cur);
            if self.is_end(self.fat_entry(cur)) { break; }
            let n = self.fat_entry(cur);
            if n == 0 || n == cur { break; }
            cur = n;
        }
        // free tail
        if needed < chain.len() {
            for c in &chain[needed..] {
                self.set_fat_entry(*c, 0);
            }
            if needed == 0 {
                file.first_cluster.store(0, core::sync::atomic::Ordering::SeqCst);
            } else {
                self.set_fat_entry(chain[needed - 1], self.eoc_value());
            }
        }
        file.size.store(new_size, core::sync::atomic::Ordering::SeqCst);
        if new_size % cl_size != 0 && needed > 0 {
            // truncate logical tail; physical sector leaves zeros (already allocated page)
        }
        Ok(())
    }
}

impl VfatFs {
    pub fn rename_wrap(&self, file: &VfatFile, new_name: &str) -> Result<(), u64> {
        use crate::arch::x86_64::syscalls::{EEXIST, ENOENT, ENOSYS, EINVAL};
        // sys_rename passes the full destination path; accept the leaf component.
        let leaf = new_name.trim_end_matches('/').rsplit('/').next().unwrap_or(new_name);
        if leaf.is_empty() || leaf=="." || leaf==".." { return Err(EINVAL); }
        // 8.3 target
        let (stem, ext) = if let Some(pos)=leaf.rfind('.') { (&leaf[..pos], &leaf[pos+1..]) } else { (leaf, "") };
        let mut short_name=[b' ';8];
        for (i,b) in stem.bytes().take(8).enumerate() { short_name[i]=if b.is_ascii_lowercase(){b-32}else{b}; }
        let mut short_ext=[b' ';3];
        for (i,b) in ext.bytes().take(3).enumerate() { short_ext[i]=if b.is_ascii_lowercase(){b-32}else{b}; }

        // check duplicate
        let mut chain=alloc::vec::Vec::new();
        let parent = file.parent_cluster;
        let mut cur = if parent==0 { self.root_cluster } else { parent };
        if cur < 2 { cur = self.root_cluster; }
        let mut visited=0usize;
        while cur>=2 && visited<10000 {
            visited+=1;
            if self.is_end(cur) { break; }
            chain.push(cur);
            let n=self.fat_entry(cur);
            if n==0 || n==cur || self.is_end(n) { break; }
            cur=n;
        }
        // duplicate scan
        for cluster in &chain {
            let cl=self.cluster_size();
            let mut b=alloc::vec![0u8;cl];
            if self.read_cluster(*cluster,&mut b).is_err() { continue; }
            for i in (0..cl).step_by(32) {
                if i+32>b.len(){break;}
                let e=&b[i..i+32];
                if e[0]==0||e[0]==0xE5 { if e[0]==0 { break; } continue; }
                if e[11]==0x0F { continue; }
                if let Some((n,_,_,_))=self.parse_dir_entry(e) {
                    if n.to_uppercase()==new_name.to_uppercase() { return Err(EEXIST); }
                }
            }
        }
        // locate old entry
        let old = self.locate_entry(parent, file.first_cluster.load(core::sync::atomic::Ordering::SeqCst));
        let old=(old.ok_or(ENOENT)?);
        // find free slot (possibly append)
        let mut sec_lba=0u64;
        let mut slot_off=0usize;
        let mut new_sector: Option<[u8;512]> = None;
        for cluster in &chain {
            let cl=self.cluster_size();
            let mut b=alloc::vec![0u8;cl];
            if self.read_cluster(*cluster,&mut b).is_err() { continue; }
            for i in (0..cl).step_by(32) {
                if i+32>b.len(){break;}
                if b[i]==0x00 || b[i]==0xE5 {
                    let sid=i/(self.bytes_per_sector as usize);
                    let off=i%(self.bytes_per_sector as usize);
                    let mut sec=[0u8;512];
                    let _=self.reader.read_sector(self.cluster_lba(*cluster)+sid as u64,&mut sec);
                    sec_lba=self.cluster_lba(*cluster)+sid as u64;
                    slot_off=off;
                    new_sector=Some(sec);
                    break;
                }
            }
            if new_sector.is_some() { break; }
        }
        if new_sector.is_none() {
            // append new cluster
            match chain.last() {
                Some(last) => {
                    if let Some(c)= self.allocate_cluster() {
                        self.set_fat_entry(*last,c);
                        let mut sec=[0u8;512];
                        new_sector=Some(sec);
                        sec_lba=self.cluster_lba(c);
                        slot_off=0;
                    }
                }
                None => {}
            }
        }
        let mut sector = new_sector.ok_or(crate::arch::x86_64::syscalls::ENOMEM)?;
        // copy old entry into new slot, rewrite name
        let mut oldsec=[0u8;512];
        self.reader.read_sector(old.0,&mut oldsec).ok();
        for k in 0..32 { sector[slot_off+k]=oldsec[old.1+k]; }
        sector[slot_off..slot_off+8].copy_from_slice(&short_name);
        sector[slot_off+8..slot_off+11].copy_from_slice(&short_ext);
        self.reader.write_sector(sec_lba,&sector).map_err(|_|ENOSYS)?;
        // delete old slot
        let mut oldsec2=[0u8;512];
        if self.reader.read_sector(old.0,&mut oldsec2).is_ok() {
            oldsec2[old.1]=0xE5;
            let _=self.reader.write_sector(old.0,&oldsec2);
        }
        Ok(())
    }
}

