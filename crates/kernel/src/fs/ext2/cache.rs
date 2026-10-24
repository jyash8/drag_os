use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::arch::sync::spinlock::SpinLock;
use crate::fs::reader::SectorReader;

// 4096 blocks * 4KiB = 16 MiB cache. In a 2GB RAM system with a 256 MiB kernel heap,
// 16 MiB caches hot binaries, shared libs, and assets without disk thrashing.
const MAX_CACHED_BLOCKS: usize = 4096;

pub struct CachedBlock {
    pub data: Vec<u8>,
    pub dirty: bool,
}

struct CacheInner {
    blocks: BTreeMap<u32, CachedBlock>,
    order: VecDeque<u32>,
    last_block: Option<u32>,
}

pub struct BlockCache {
    inner: SpinLock<CacheInner>,
    block_size: u32,
    /// Sector reader for block I/O. Currently reads/writes go through the
    /// global block device (`crate::drivers::block`); this field is reserved
    /// for loopback (file-backed) filesystems where reads/writes target a
    /// host file rather than the block device.
    #[allow(dead_code)]
    reader: Arc<dyn SectorReader>,
}

impl BlockCache {
    pub fn new(block_size: u32) -> Self {
        Self::new_with_reader(block_size, Arc::new(crate::fs::reader::GlobalReader))
    }
    pub fn new_with_reader(block_size: u32, reader: Arc<dyn SectorReader>) -> Self {
        Self {
            inner: SpinLock::new(CacheInner {
                blocks: BTreeMap::new(),
                order: VecDeque::new(),
                last_block: None,
            }),
            block_size,
            reader,
        }
    }

    pub fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), ()> {
        let mut cache = self.inner.lock();

        if let Some(cached) = cache.blocks.get(&block) {
            buf.copy_from_slice(&cached.data);
            cache.last_block = Some(block);
            return Ok(());
        }

        let mut data = Vec::new();
        data.resize(self.block_size as usize, 0);

        // Read the requested block using a single multi-sector DMA transfer
        let sectors_per_block = self.block_size / 512;
        let start_sector = (block as u64) * (sectors_per_block as u64);

        self.reader.read_sectors(start_sector, &mut data).map_err(|_| ())?;

        buf.copy_from_slice(&data);

        // Store in cache
        cache.blocks.insert(block, CachedBlock { data, dirty: false });
        cache.order.push_back(block);

        // Sequential readahead heuristic: if accessing consecutively, prefetch next 4 blocks
        let is_sequential = cache.last_block.map_or(false, |last| block == last.wrapping_add(1));
        cache.last_block = Some(block);

        if is_sequential {
            const READAHEAD_BLOCKS: u32 = 4;
            let mut ra_count = 0;
            while ra_count < READAHEAD_BLOCKS && !cache.blocks.contains_key(&(block + 1 + ra_count)) {
                ra_count += 1;
            }
            if ra_count > 0 {
                let ra_start = block + 1;
                let mut ra_data = Vec::new();
                ra_data.resize((ra_count * self.block_size) as usize, 0);
                let ra_sector = (ra_start as u64) * (sectors_per_block as u64);
                if self.reader.read_sectors(ra_sector, &mut ra_data).is_ok() {
                    for i in 0..ra_count {
                        let offset = (i * self.block_size) as usize;
                        let bdata = ra_data[offset..offset + self.block_size as usize].to_vec();
                        let bnum = ra_start + i;
                        cache.blocks.insert(bnum, CachedBlock { data: bdata, dirty: false });
                        cache.order.push_back(bnum);
                    }
                }
            }
        }

        self.evict(&mut cache);
        Ok(())
    }

    pub fn write_block(&self, block: u32, buf: &[u8]) -> Result<(), ()> {
        let mut cache = self.inner.lock();

        let mut data = Vec::new();
        data.extend_from_slice(buf);

        let fresh = !cache.blocks.contains_key(&block);
        cache.blocks.insert(block, CachedBlock { data, dirty: true });
        if fresh {
            cache.order.push_back(block);
        }
        cache.last_block = Some(block);
        self.evict(&mut cache);
        Ok(())
    }

    fn evict(&self, cache: &mut CacheInner) {
        while cache.blocks.len() > MAX_CACHED_BLOCKS {
            let Some(oldest) = cache.order.pop_front() else { break };
            let Some(cached) = cache.blocks.remove(&oldest) else { continue };
            if cached.dirty {
                let sectors_per_block = self.block_size / 512;
                let start_sector = (oldest as u64) * (sectors_per_block as u64);
                let _ = self.reader.write_sectors(start_sector, &cached.data);
            }
        }
    }

    pub fn sync(&self) -> Result<(), ()> {
        let mut cache = self.inner.lock();
        let sectors_per_block = self.block_size / 512;

        for (block, cached) in cache.blocks.iter_mut() {
            if cached.dirty {
                let start_sector = (*block as u64) * (sectors_per_block as u64);
                self.reader.write_sectors(start_sector, &cached.data).map_err(|_| ())?;
                cached.dirty = false;
            }
        }
        Ok(())
    }
}
