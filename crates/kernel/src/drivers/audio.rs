//! Audio Drivers and Infrastructure for Drag OS (PC Speaker, AC97, Intel HDA)

pub mod pcspeaker {
    use crate::arch::x86_64::io::{inb, outb};
    use core::sync::atomic::{AtomicUsize, Ordering};

    const PIT_BASE_FREQ: u32 = 1193182;
    const PIT_COMMAND_PORT: u16 = 0x43;
    const PIT_CHANNEL2_PORT: u16 = 0x42;
    const SPEAKER_PORT: u16 = 0x61;

    static BEEP_OFF_TICK: AtomicUsize = AtomicUsize::new(0);

    /// Initialize PC speaker hardware via PIT Channel 2 and Port 0x61.
    pub fn init() {
        stop_tone();
    }

    /// Play a sound frequency (in Hz) on the PC speaker.
    pub fn play_tone(freq_hz: u32) {
        if freq_hz == 0 {
            stop_tone();
            return;
        }

        let divisor = PIT_BASE_FREQ / freq_hz;
        if divisor == 0 || divisor > 0xFFFF {
            return;
        }

        unsafe {
            // Set PIT channel 2 to mode 3 (square wave generator)
            outb(PIT_COMMAND_PORT, 0xB6);
            // Set low byte and high byte of divisor
            outb(PIT_CHANNEL2_PORT, (divisor & 0xFF) as u8);
            outb(PIT_CHANNEL2_PORT, ((divisor >> 8) & 0xFF) as u8);

            // Enable speaker output (bits 0 and 1 on port 0x61)
            let tmp = inb(SPEAKER_PORT);
            if (tmp & 0x03) != 0x03 {
                outb(SPEAKER_PORT, tmp | 0x03);
            }
        }
    }

    /// Stop playing sound on the PC speaker.
    pub fn stop_tone() {
        BEEP_OFF_TICK.store(0, Ordering::Relaxed);
        unsafe {
            let tmp = inb(SPEAKER_PORT);
            outb(SPEAKER_PORT, tmp & !0x03);
        }
    }

    /// Non-blocking async beep: plays tone and auto-stops after duration_ms.
    pub fn beep_async(freq_hz: u32, duration_ms: u32) {
        play_tone(freq_hz);
        let now = crate::time::TICKS.load(Ordering::Relaxed);
        BEEP_OFF_TICK.store(now + (duration_ms as usize), Ordering::Relaxed);
    }

    /// Synchronous blocking beep.
    pub fn beep(freq_hz: u32, duration_ms: u32) {
        play_tone(freq_hz);
        let start = crate::time::TICKS.load(Ordering::Relaxed);
        while crate::time::TICKS.load(Ordering::Relaxed) - start < (duration_ms as usize) {
            core::hint::spin_loop();
        }
        stop_tone();
    }

    /// Called on every 1ms timer tick to auto-stop expired tones.
    pub fn timer_tick() {
        let off_tick = BEEP_OFF_TICK.load(Ordering::Relaxed);
        if off_tick != 0 {
            let now = crate::time::TICKS.load(Ordering::Relaxed);
            if now >= off_tick {
                stop_tone();
            }
        }
    }
}

pub mod ac97 {
    use crate::arch::sync::spinlock::SpinLock;
    use crate::arch::x86_64::io::{inb, inw, outb, outw, outl};
    use crate::arch::x86_64::pci::PciAddress;
    use crate::memory::FRAME_ALLOCATOR;
    use core::sync::atomic::AtomicBool;
    use shared::boot::boot_info::HHDM_BASE;
    use shared::memory::address::PAGE_SIZE;
    use shared::memory::allocator::FrameAllocator;

    // NAMBAR (Mixer) Registers
    const AC97_RESET: u16 = 0x00;
    const AC97_MASTER_VOL: u16 = 0x02;
    const AC97_PCM_OUT_VOL: u16 = 0x18;
    const AC97_EXT_AUDIO_ID: u16 = 0x28;
    const AC97_EXT_AUDIO_CTRL: u16 = 0x2A;
    const AC97_PCM_FRONT_DAC_RATE: u16 = 0x2C;

    // NABMBAR (Bus Master DMA) Registers
    const AC97_PO_BDBAR: u16 = 0x10;  // PCM Out Buffer Descriptor List Base Address (32-bit)
    const AC97_PO_CIV: u16 = 0x14;    // PCM Out Current Index Value (8-bit)
    const AC97_PO_LVI: u16 = 0x15;    // PCM Out Last Valid Index (8-bit)
    const AC97_PO_CR: u16 = 0x1B;     // PCM Out Control Register (8-bit)
    const AC97_PI_BDBAR: u16 = 0x00;  // PCM In Buffer Descriptor List Base Address (32-bit)
    const AC97_PI_CIV: u16 = 0x04;    // PCM In Current Index Value (8-bit)
    const AC97_PI_LVI: u16 = 0x05;    // PCM In Last Valid Index (8-bit)
    const AC97_PI_CR: u16 = 0x0B;     // PCM In Control Register (8-bit)

    // BDL Entry (8 bytes)
    #[repr(C, packed)]
    #[derive(Clone, Copy, Default)]
    pub struct BdlEntry {
        pub addr: u32,
        pub len: u16,      // Sample count (16-bit samples)
        pub control: u16,  // Bit 15 = IOC (Interrupt On Completion)
    }

    const BDL_ENTRIES: usize = 32;
    const BUF_SIZE: usize = 2048;

    pub struct Ac97Driver {
        pub nam_bar: u16,  // Native Audio Mixer Base Port
        pub nabm_bar: u16, // Native Audio Bus Master Base Port
        bdl: usize,     // kernel-mapped BDL (HHDM_BASE + phys)
        buffers: usize, // kernel-mapped DMA buffer base (HHDM_BASE + phys)
        cur_lvi: u8,    // descriptor currently being filled
        fill: usize,    // bytes used in current descriptor
        is_playing: bool,
        pub channels: u8,  // 1 = Mono, 2 = Stereo
        pub fmt_bits: u8,  // 8 = AFMT_U8, 16 = AFMT_S16_LE
        /// Capture DMA ring (reverse of playback — hardware fills, we drain).
        capture_bdl: usize,
        capture_buffers: usize,
        capture_cur_lvi: u8,
        capture_fill: usize,
        is_capturing: bool,
    }

    pub static GLOBAL_AC97: SpinLock<Option<Ac97Driver>> = SpinLock::new(None);
    pub static AC97_PRESENT: AtomicBool = AtomicBool::new(false);

    impl Ac97Driver {
        /// Initialize AC97 PCI device and setup Bus Master DMA.
        pub fn init_pci(bus: u8, device: u8, function: u8) -> Option<Self> {
            let pci = PciAddress::new(bus, device, function);
            pci.enable_memory_and_master();

            let nam_bar = pci.read_io_bar(0)?;
            let nabm_bar = pci.read_io_bar(1)?;

            crate::println!("AC97 Audio: found at {:02x}:{:02x}.{}, NAMBAR={:#x}, NABMBAR={:#x}",
                bus, device, function, nam_bar, nabm_bar);

            unsafe {
                // Reset AC97 Bus Master Engine
                outb(nabm_bar + AC97_PO_CR, 0x02); // Reset bit
                for _ in 0..1000 { core::hint::spin_loop(); }
                outb(nabm_bar + AC97_PO_CR, 0x00); // Clear reset

                // Reset AC97 Audio Mixer
                outw(nam_bar + AC97_RESET, 0x0000);
                
                // Set Master & PCM Volume to Max (0x0000 = 0dB attenuation)
                outw(nam_bar + AC97_MASTER_VOL, 0x0000);
                outw(nam_bar + AC97_PCM_OUT_VOL, 0x0000);

                // Set Sample Rate to 44100 Hz if variable rate audio is supported
                let ext_id = inw(nam_bar + AC97_EXT_AUDIO_ID);
                if (ext_id & 1) != 0 { // Variable Rate Audio (VRA) supported
                    outw(nam_bar + AC97_EXT_AUDIO_CTRL, inw(nam_bar + AC97_EXT_AUDIO_CTRL) | 1);
                    outw(nam_bar + AC97_PCM_FRONT_DAC_RATE, 44100);
                }
            }

            let total_buf_bytes = BDL_ENTRIES * BUF_SIZE;
            let total_pages = (total_buf_bytes + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize;

            // Allocate playback BDL + buffers
            let mut alloc = FRAME_ALLOCATOR.lock();
            let bdl_phys = alloc.alloc_pages(1, PAGE_SIZE).ok()?;
            let bufs_phys = alloc.alloc_pages(total_pages, PAGE_SIZE).ok()?;
            // Allocate capture BDL + buffers
            let capture_bdl_phys = alloc.alloc_pages(1, PAGE_SIZE).ok()?;
            let capture_bufs_phys = alloc.alloc_pages(total_pages, PAGE_SIZE).ok()?;
            drop(alloc);

            let bdl = (HHDM_BASE.0 + bdl_phys.0) as usize;
            let buffers = (HHDM_BASE.0 + bufs_phys.0) as usize;
            let capture_bdl = (HHDM_BASE.0 + capture_bdl_phys.0) as usize;
            let capture_buffers = (HHDM_BASE.0 + capture_bufs_phys.0) as usize;
            let bdl_ptr = bdl as *mut BdlEntry;
            let capture_bdl_ptr = capture_bdl as *mut BdlEntry;

            unsafe {
                core::ptr::write_bytes(bdl as *mut u8, 0, PAGE_SIZE as usize);
                core::ptr::write_bytes(buffers as *mut u8, 0, total_buf_bytes);
                core::ptr::write_bytes(capture_bdl as *mut u8, 0, PAGE_SIZE as usize);
                core::ptr::write_bytes(capture_buffers as *mut u8, 0, total_buf_bytes);

                // Playback BDL
                for i in 0..BDL_ENTRIES {
                    (*bdl_ptr.add(i)).addr = (bufs_phys.0 + (i as u64 * BUF_SIZE as u64)) as u32;
                    (*bdl_ptr.add(i)).len = (BUF_SIZE / 2) as u16; // 1024 samples
                    (*bdl_ptr.add(i)).control = 0x8000; // Interrupt on completion
                }
                // Capture BDL
                for i in 0..BDL_ENTRIES {
                    (*capture_bdl_ptr.add(i)).addr = (capture_bufs_phys.0 + (i as u64 * BUF_SIZE as u64)) as u32;
                    (*capture_bdl_ptr.add(i)).len = (BUF_SIZE / 2) as u16; // 1024 samples
                    (*capture_bdl_ptr.add(i)).control = 0x8000; // Interrupt on completion
                }

                // Set playback BDBAR
                outl(nabm_bar + AC97_PO_BDBAR, bdl_phys.0 as u32);
                // Set capture BDBAR
                outl(nabm_bar + AC97_PI_BDBAR, capture_bdl_phys.0 as u32);

                // Unmute Master Volume (0x02) and PCM Out Volume (0x18) (0 = 0dB max volume)
                outw(nam_bar + 0x02, 0x0000);
                outw(nam_bar + 0x18, 0x0000);

                Some(Ac97Driver {
                    nam_bar,
                    nabm_bar,
                    bdl,
                    buffers,
                    cur_lvi: 0,
                    fill: 0,
                    is_playing: false,
                    channels: 1,
                    fmt_bits: 16,
                    capture_bdl,
                    capture_buffers,
                    capture_cur_lvi: 0,
                    capture_fill: 0,
                    is_capturing: false,
                })
            }
        }

        /// Kick DMA engine to ensure it is actively running and clear completion status.
        pub fn kick_dma(&mut self) {
            unsafe {
                outw(self.nabm_bar + 0x16, 0x1E); // Clear status bits
                let sr = inw(self.nabm_bar + 0x16);
                let cr = inb(self.nabm_bar + AC97_PO_CR);
                if (sr & 0x01) != 0 || (cr & 0x01) == 0 {
                    // QEMU and real hardware require a 0 -> 1 transition to start DMA voice
                    outb(self.nabm_bar + AC97_PO_CR, 0x00);
                    outb(self.nabm_bar + AC97_PO_CR, 0x01);
                }
                self.is_playing = true;
            }
        }

        /// Write PCM sample bytes into the AC97 DMA ring buffer.
        pub fn write_pcm_samples(&mut self, samples: &[u8]) -> Result<usize, u64> {
            if samples.is_empty() {
                return Ok(0);
            }

            if self.is_playing {
                // Check if ring buffer is full (> BDL_ENTRIES-4 descriptors queued).
                // If so, return immediately — the caller (audio write syscall) will
                // be called again on the next frame, by which time DMA has retired
                // at least one buffer.  Never busy-spin here: we hold GLOBAL_AC97
                // (a spinlock), so IF is cleared and any spin will stall the whole core.
                let sr = unsafe { inw(self.nabm_bar + 0x16) };
                if (sr & 0x01) == 0 {
                    let civ = unsafe { inb(self.nabm_bar + AC97_PO_CIV) };
                    let busy = (self.cur_lvi + (BDL_ENTRIES as u8) - civ) % (BDL_ENTRIES as u8);
                    if busy >= (BDL_ENTRIES as u8 - 4) {
                        // Ring is full; discard this chunk rather than burning CPU.
                        // The player's clock sync will pace writes naturally.
                        return Ok(samples.len());
                    }
                }
            }

            let mut src = 0;
            while src < samples.len() {
                let (v_l, v_r, adv) = if self.fmt_bits == 16 && self.channels == 1 {
                    if src + 1 >= samples.len() { break; }
                    let v = samples[src] as u16 | ((samples[src + 1] as u16) << 8);
                    (v, v, 2)
                } else if self.fmt_bits == 16 && self.channels == 2 {
                    if src + 3 >= samples.len() { break; }
                    let vl = samples[src] as u16 | ((samples[src + 1] as u16) << 8);
                    let vr = samples[src + 2] as u16 | ((samples[src + 3] as u16) << 8);
                    (vl, vr, 4)
                } else if self.fmt_bits == 8 && self.channels == 1 {
                    let u8_val = samples[src];
                    let s16_val = ((u8_val as i16) - 128) * 256;
                    let v = s16_val as u16;
                    (v, v, 1)
                } else {
                    if src + 1 >= samples.len() { break; }
                    let s_l = (((samples[src] as i16) - 128) * 256) as u16;
                    let s_r = (((samples[src + 1] as i16) - 128) * 256) as u16;
                    (s_l, s_r, 2)
                };

                let dst = (self.buffers + self.cur_lvi as usize * BUF_SIZE + self.fill) as *mut u8;
                unsafe {
                    dst.write(v_l as u8);
                    dst.add(1).write((v_l >> 8) as u8);
                    dst.add(2).write(v_r as u8);
                    dst.add(3).write((v_r >> 8) as u8);
                }
                self.fill += 4;
                src += adv;

                if self.fill >= BUF_SIZE {
                    self.commit_current();
                }
            }

            Ok(samples.len())
        }

        /// Commit the current descriptor to the ring and advance to the next one.
        fn commit_current(&mut self) {
            let idx = self.cur_lvi as usize;
            unsafe {
                let entry = (self.bdl as *mut BdlEntry).add(idx);
                (*entry).len = (BUF_SIZE / 2) as u16; // 1024 samples (16-bit words)
                (*entry).control = 0x8000;
                outb(self.nabm_bar + AC97_PO_LVI, self.cur_lvi);
            }
            self.kick_dma();
            self.cur_lvi = (self.cur_lvi + 1) % (BDL_ENTRIES as u8);
            self.fill = 0;
        }

        /// Flush any partial buffer currently in progress by padding with silence
        /// and committing to the DMA ring.
        pub fn flush(&mut self) {
            if self.fill > 0 {
                let remaining = BUF_SIZE - self.fill;
                let dst = (self.buffers + self.cur_lvi as usize * BUF_SIZE + self.fill) as *mut u8;
                unsafe {
                    core::ptr::write_bytes(dst, 0, remaining);
                }
                self.commit_current();
            }
        }

        /// Set master volume level (0..=100).
        pub fn set_volume(&mut self, volume: u8) {
            let atten = ((100 - volume.min(100)) as u16 * 31 / 100) & 0x1F;
            let val = (atten << 8) | atten;
            unsafe {
                outw(self.nam_bar + AC97_MASTER_VOL, val);
            }
        }

        /// Get master volume level (0..=100).
        pub fn get_volume(&self) -> u8 {
            let val = unsafe { inw(self.nam_bar + AC97_MASTER_VOL) };
            let atten = (val & 0x1F) as u8;
            100 - (atten * 100 / 31)
        }

        /// Reset buffer filling state.
        pub fn reset(&mut self) {
            self.fill = 0;
        }

        /// Reset capture buffer filling state.
        pub fn reset_capture(&mut self) {
            self.capture_fill = 0;
            self.is_capturing = false;
        }

        /// Kick the capture DMA engine to start capturing.
        pub fn kick_capture_dma(&mut self) {
            unsafe {
                // Clear status bits on capture control register
                let cr = inb(self.nabm_bar + AC97_PI_CR);
                if (cr & 0x01) == 0 {
                    // QEMU requires a 0->1 transition to start DMA voice on capture
                    outb(self.nabm_bar + AC97_PI_CR, 0x00);
                    outb(self.nabm_bar + AC97_PI_CR, 0x01);
                }
                self.is_capturing = true;
            }
        }

        /// Read captured PCM samples from the AC97 DMA ring buffer.
        /// Returns (bytes_read, more_available) where more_available indicates
        /// the caller should call again soon.
        pub fn read_pcm_samples(&mut self, buf: &mut [u8]) -> Result<usize, u64> {
            if buf.is_empty() {
                return Ok(0);
            }

            if !self.is_capturing {
                // Start capture if not already running
                self.kick_capture_dma();
            }

            // Read from capture ring: hardware fills descriptors, we drain them.
            // Similar to write_pcm_samples but reversed — we read from capture_buffers
            // and advance capture_cur_lvi.
            let mut src = 0;
            while src < buf.len() {
                // Check current index value (CIV) from hardware to know which descriptors are filled
                let civ = unsafe { inb(self.nabm_bar + AC97_PI_CIV) };
                let civ_u8 = civ as u8;
                // How many descriptors are ready: descriptors between capture_cur_lvi and civ
                let ready = (civ_u8 as i32 - self.capture_cur_lvi as i32 + BDL_ENTRIES as i32) % (BDL_ENTRIES as i32);
                if ready == 0 {
                    // No data available yet
                    if src > 0 {
                        break;
                    }
                    // Non-blocking: return EAGAIN-equivalent via Ok(0)
                    return Ok(0);
                }

                // Read up to one descriptor worth of data
                let idx = self.capture_cur_lvi as usize;
                let entry_addr = unsafe {
                    let entry = (self.capture_bdl as *mut BdlEntry).add(idx);
                    let len = (*entry).len as usize * 2; // len is in 16-bit samples
                    (*entry).addr as usize
                };

                // We don't need entry_addr since we know our buffer layout
                let buffer_offset = idx * BUF_SIZE;
                let available = BUF_SIZE; // full descriptor
                let to_copy = available.min(buf.len() - src);

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        (self.capture_buffers + buffer_offset) as *const u8,
                        buf.as_mut_ptr().add(src),
                        to_copy,
                    );
                }
                src += to_copy;
                self.capture_fill += to_copy;

                if self.capture_fill >= BUF_SIZE {
                    // Mark this descriptor as consumed by advancing LVI
                    unsafe {
                        outb(self.nabm_bar + AC97_PI_LVI, self.capture_cur_lvi);
                    }
                    self.capture_cur_lvi = (self.capture_cur_lvi + 1) % (BDL_ENTRIES as u8);
                    self.capture_fill = 0;
                }
            }

            Ok(src)
        }
    }

}

pub mod hda {
    use crate::arch::x86_64::pci::PciAddress;
    use crate::memory::FRAME_ALLOCATOR;
    use shared::boot::boot_info::HHDM_BASE;
    use shared::memory::address::PAGE_SIZE;
    use shared::memory::allocator::FrameAllocator;
    use core::sync::atomic::{AtomicBool, Ordering};

    const REG_GCAP: u16 = 0x00;
    const REG_VMIN: u16 = 0x02;
    const REG_VMAJ: u16 = 0x03;
    const REG_OUTPAY: u16 = 0x04;
    const REG_INPAY: u16 = 0x06;
    const REG_GCTL: u16 = 0x08;
    const REG_WAKEEN: u16 = 0x0C;
    const REG_STATESTS: u16 = 0x0E;
    const REG_GSTS: u16 = 0x0F;
    const REG_OUTSTREAMPAY: u16 = 0x10;
    const REG_INSTREAMPAY: u16 = 0x12;
    const REG_IBX: u16 = 0x14;
    const REG_CORBLBASE: u16 = 0x18;
    const REG_CORBUBASE: u16 = 0x1C;
    const REG_CORBWP: u16 = 0x20;
    const REG_CORBRWP: u16 = 0x22;
    const REG_CORBCTL: u16 = 0x24;
    const REG_CORBSIZE: u16 = 0x25;
    const REG_RIRBLBASE: u16 = 0x28;
    const REG_RIRBUBASE: u16 = 0x2C;
    const REG_RIRBWP: u16 = 0x2E;
    const REG_RIRBRWP: u16 = 0x30;
    const REG_RIRBCTL: u16 = 0x32;
    const REG_RIRBSIZE: u16 = 0x33;

    const REG_ICH: u16 = 0x34;
    const REG_IEN: u16 = 0x36;

    const REG_CORBCTL_DMAEN: u8 = 0x01;
    const REG_GCTL_CRST: u8 = 0x01;

    const REG_STREAM_BASE: u64 = 0x80;
    const REG_STREAM_STRIDE: u64 = 0x20;
    const REG_SD_CTL: u16 = 0x00;
    const REG_SD_LVI: u16 = 0x01;
    const REG_SD_FIFOS: u16 = 0x02;
    const REG_SD_FORMAT: u16 = 0x04;
    const REG_SD_BDLPL: u16 = 0x08;
    const REG_SD_BDLPU: u16 = 0x0C;
    const REG_SD_LPIB: u16 = 0x10;
    const REG_SD_CBL: u16 = 0x14;
    const REG_SD_LVI2: u16 = 0x18;

    const SD_CTL_STREAM_RESET: u8 = 0x01;
    const SD_CTL_STREAM_START: u8 = 0x02;
    const SD_CTL_STREAM_IO_ENABLE: u8 = 0x04;
    const SD_CTL_FIFOE: u8 = 0x08;

    const CORB_SIZE: usize = 256;
    const RIRB_SIZE: usize = 256;
    const BDL_ENTRIES: usize = 32;
    const BUF_SIZE: usize = 2048;

    #[repr(C, packed)]
    #[derive(Clone, Copy, Default)]
    pub struct CorbEntry {
        pub dword0: u32,
        pub dword1: u32,
    }

    #[repr(C, packed)]
    #[derive(Clone, Copy, Default)]
    pub struct RirbEntry {
        pub response: u32,
        pub resp_ex: u32,
    }

    #[repr(C, packed)]
    #[derive(Clone, Copy, Default)]
    pub struct BdlEntry {
        pub addr_lo: u32,
        pub addr_hi: u32,
        pub len: u16,
        pub ioc: u16,
    }

    pub struct Stream {
        pub active: bool,
        pub cur_idx: usize,
        pub bdl: usize,
        pub buffers: usize,
    }

    impl Stream {
        pub const fn new() -> Self {
            Self { active: false, cur_idx: 0, bdl: 0, buffers: 0 }
        }
    }

    pub struct HdaDriver {
        pub mmio_virt: u64,
        pub mmio_phys: u64,
        pub mmio_size: usize,
        pub corb: usize,
        pub corb_phys: u64,
        pub corb_wp: usize,
        pub corb_rp: usize,
        pub rirb: usize,
        pub rirb_phys: u64,
        pub rirb_wp: usize,
        pub rirb_rp: usize,
        pub playback: Stream,
        pub capture: Stream,
        pub channels: u8,
        pub fmt_bits: u8,
    }

    impl HdaDriver {
        pub const fn new() -> Self {
            Self {
                mmio_virt: 0,
                mmio_phys: 0,
                mmio_size: 0,
                corb: 0,
                corb_phys: 0,
                corb_wp: 0,
                corb_rp: 0,
                rirb: 0,
                rirb_phys: 0,
                rirb_wp: 0,
                rirb_rp: 0,
                playback: Stream::new(),
                capture: Stream::new(),
                channels: 2,
                fmt_bits: 16,
            }
        }

        pub fn init_pci(bus: u8, device: u8, function: u8) -> Option<Self> {
            let pci = PciAddress::new(bus, device, function);
            pci.enable_memory_and_master();

            let mmio_bar = pci.read_bar(0)?;
            let (mmio_phys, mmio_size) = mmio_bar;
            let mmio_virt = crate::memory::mmio::map_device_memory(mmio_phys, mmio_size)
                .ok()?
                .0;

            crate::println!(
                "HDA Audio: found at {:02x}:{:02x}.{}, MMIO={:#x} size={}",
                bus, device, function, mmio_virt, mmio_size
            );

            unsafe {
                let gctl_ptr = (mmio_virt + REG_GCTL as u64) as *mut u8;
                let gctl = gctl_ptr.read_volatile();
                gctl_ptr.write_volatile(gctl | REG_GCTL_CRST);
                for _ in 0..1000 { core::hint::spin_loop(); }
                gctl_ptr.write_volatile(gctl & !REG_GCTL_CRST);
                for _ in 0..10000 { core::hint::spin_loop(); }
            }

            let total_bytes = BDL_ENTRIES * BUF_SIZE;
            let total_pages = (total_bytes + PAGE_SIZE as usize - 1) / PAGE_SIZE as usize;

            let mut alloc = FRAME_ALLOCATOR.lock();
            let corb_phys = alloc.alloc_pages(1, PAGE_SIZE).ok()?;
            let rirb_phys = alloc.alloc_pages(1, PAGE_SIZE).ok()?;
            let pbdl_phys = alloc.alloc_pages(1, PAGE_SIZE).ok()?;
            let pbuf_phys = alloc.alloc_pages(total_pages, PAGE_SIZE).ok()?;
            let cbdl_phys = alloc.alloc_pages(1, PAGE_SIZE).ok()?;
            let cbuf_phys = alloc.alloc_pages(total_pages, PAGE_SIZE).ok()?;
            drop(alloc);

            let corb = (HHDM_BASE.0 + corb_phys.0) as usize;
            let rirb = (HHDM_BASE.0 + rirb_phys.0) as usize;
            let pbdl = (HHDM_BASE.0 + pbdl_phys.0) as usize;
            let pbuf = (HHDM_BASE.0 + pbuf_phys.0) as usize;
            let cbdl = (HHDM_BASE.0 + cbdl_phys.0) as usize;
            let cbuf = (HHDM_BASE.0 + cbuf_phys.0) as usize;

            unsafe {
                core::ptr::write_bytes(corb as *mut u8, 0, PAGE_SIZE as usize);
                core::ptr::write_bytes(rirb as *mut u8, 0, PAGE_SIZE as usize);
                core::ptr::write_bytes(pbdl as *mut u8, 0, PAGE_SIZE as usize);
                core::ptr::write_bytes(pbuf as *mut u8, 0, total_bytes);
                core::ptr::write_bytes(cbdl as *mut u8, 0, PAGE_SIZE as usize);
                core::ptr::write_bytes(cbuf as *mut u8, 0, total_bytes);

                for i in 0..BDL_ENTRIES {
                    let entry = (pbdl as *mut BdlEntry).add(i);
                    (*entry).addr_lo = (pbuf_phys.0 + (i as u64 * BUF_SIZE as u64)) as u32;
                    (*entry).addr_hi = 0;
                    (*entry).len = BUF_SIZE as u16;
                    (*entry).ioc = 0x8000;
                }
                for i in 0..BDL_ENTRIES {
                    let entry = (cbdl as *mut BdlEntry).add(i);
                    (*entry).addr_lo = (cbuf_phys.0 + (i as u64 * BUF_SIZE as u64)) as u32;
                    (*entry).addr_hi = 0;
                    (*entry).len = BUF_SIZE as u16;
                    (*entry).ioc = 0x8000;
                }

                ((mmio_virt + REG_CORBLBASE as u64) as *mut u32).write_volatile(corb_phys.0 as u32);
                ((mmio_virt + REG_CORBUBASE as u64) as *mut u32).write_volatile(0);
                ((mmio_virt + REG_CORBWP as u64) as *mut u16).write_volatile(0);
                ((mmio_virt + REG_CORBRWP as u64) as *mut u16).write_volatile(0);
                ((mmio_virt + REG_CORBCTL as u64) as *mut u8).write_volatile(REG_CORBCTL_DMAEN);
                ((mmio_virt + REG_CORBSIZE as u64) as *mut u8).write_volatile((CORB_SIZE / 2 - 1) as u8);

                ((mmio_virt + REG_RIRBLBASE as u64) as *mut u32).write_volatile(rirb_phys.0 as u32);
                ((mmio_virt + REG_RIRBUBASE as u64) as *mut u32).write_volatile(0);
                ((mmio_virt + REG_RIRBWP as u64) as *mut u16).write_volatile(0);
                ((mmio_virt + REG_RIRBRWP as u64) as *mut u16).write_volatile(0);
                ((mmio_virt + REG_RIRBCTL as u64) as *mut u8).write_volatile(REG_CORBCTL_DMAEN);
                ((mmio_virt + REG_RIRBSIZE as u64) as *mut u8).write_volatile((RIRB_SIZE / 2 - 1) as u8);

                let stream_base = mmio_virt + REG_STREAM_BASE as u64;
                ((stream_base + REG_SD_BDLPL as u64) as *mut u32).write_volatile(pbdl_phys.0 as u32);
                ((stream_base + REG_SD_BDLPU as u64) as *mut u32).write_volatile(0);

                let capture_base = mmio_virt + REG_STREAM_BASE as u64 + REG_STREAM_STRIDE as u64;
                ((capture_base + REG_SD_BDLPL as u64) as *mut u32).write_volatile(cbdl_phys.0 as u32);
                ((capture_base + REG_SD_BDLPU as u64) as *mut u32).write_volatile(0);
            }

            Some(Self {
                mmio_virt,
                mmio_phys: mmio_phys.0,
                mmio_size,
                corb,
                corb_phys: corb_phys.0,
                corb_wp: 0,
                corb_rp: 0,
                rirb,
                rirb_phys: rirb_phys.0,
                rirb_wp: 0,
                rirb_rp: 0,
                playback: Stream::new(),
                capture: Stream::new(),
                channels: 2,
                fmt_bits: 16,
            })
        }

        pub fn set_volume(&mut self, _volume: u8) {}

        fn send_command(&mut self, cad: u8, nid: u8, verb: u8, payload: u8) -> bool {
            let wp = self.corb_wp;
            let entry = unsafe { (self.corb as *mut CorbEntry).add(wp) };
            unsafe {
                let dword0 = ((cad as u32) << 28) | ((nid as u32) << 20) | ((verb as u32) << 8) | (payload as u32);
                (*entry).dword0 = dword0;
                (*entry).dword1 = 0;
            }
            self.corb_wp = (wp + 1) % CORB_SIZE;
            unsafe {
                ((self.mmio_virt + REG_CORBWP as u64) as *mut u16).write_volatile(self.corb_wp as u16);
            }
            true
        }

        pub fn poll_response(&mut self) -> Option<u32> {
            unsafe {
                let rp = ((self.mmio_virt + REG_RIRBRWP as u64) as *mut u16).read_volatile() as usize;
                if rp == self.rirb_rp {
                    return None;
                }
                let entry = (self.rirb as *mut RirbEntry).add(self.rirb_rp);
                let response = (*entry).response;
                self.rirb_rp = (self.rirb_rp + 1) % RIRB_SIZE;
                Some(response)
            }
        }

        pub fn write_stream(&mut self, stream_id: u8, data: &[u8]) -> Result<usize, u64> {
            if !self.playback.active {
                return Ok(0);
            }
            if data.is_empty() {
                return Ok(0);
            }

            let stream_base = self.mmio_virt + REG_STREAM_BASE as u64 + stream_id as u64 * REG_STREAM_STRIDE as u64;
            let mut src = 0;
            while src < data.len() {
                let cbl = unsafe { ((stream_base + REG_SD_CBL as u64) as *mut u32).read_volatile() } as usize;
                let hw_idx = cbl / BUF_SIZE;
                let free = if hw_idx >= self.playback.cur_idx {
                    BDL_ENTRIES - (hw_idx - self.playback.cur_idx) - 1
                } else {
                    self.playback.cur_idx - hw_idx - 1
                };
                if free == 0 {
                    break;
                }

                let idx = self.playback.cur_idx;
                let entry_addr = self.playback.buffers + idx * BUF_SIZE;
                let to_copy = BUF_SIZE.min(data.len() - src);

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        data.as_ptr().add(src),
                        entry_addr as *mut u8,
                        to_copy,
                    );
                }
                src += to_copy;

                if to_copy == BUF_SIZE {
                    let entry = unsafe { (self.playback.bdl as *mut BdlEntry).add(idx) };
                    unsafe {
                        (*entry).len = BUF_SIZE as u16;
                        (*entry).ioc = 0x8000;
                    }
                    self.playback.cur_idx = (idx + 1) % BDL_ENTRIES;
                    unsafe {
                        ((stream_base + REG_SD_LVI as u64) as *mut u8).write_volatile(self.playback.cur_idx as u8);
                    }
                }
            }

            unsafe {
                let ctl = ((stream_base + REG_SD_CTL as u64) as *mut u8).read_volatile();
                if (ctl & SD_CTL_STREAM_START) == 0 {
                    ((stream_base + REG_SD_CTL as u64) as *mut u8).write_volatile(ctl | SD_CTL_STREAM_START | SD_CTL_STREAM_IO_ENABLE);
                }
            }

            Ok(src)
        }

        pub fn read_stream(&mut self, stream_id: u8, buf: &mut [u8]) -> Result<usize, u64> {
            if !self.capture.active {
                return Ok(0);
            }
            if buf.is_empty() {
                return Ok(0);
            }

            let stream_base = self.mmio_virt + REG_STREAM_BASE as u64 + (stream_id as u64 + 1) * REG_STREAM_STRIDE as u64;

            unsafe {
                let ctl = ((stream_base + REG_SD_CTL as u64) as *mut u8).read_volatile();
                if (ctl & SD_CTL_STREAM_START) == 0 {
                    ((stream_base + REG_SD_CTL as u64) as *mut u8).write_volatile(ctl | SD_CTL_STREAM_START | SD_CTL_STREAM_IO_ENABLE);
                }
            }

            let mut dst = 0;
            while dst < buf.len() {
                let cbl = unsafe { ((stream_base + REG_SD_CBL as u64) as *mut u32).read_volatile() } as usize;
                let hw_idx = cbl / BUF_SIZE;
                let filled = if hw_idx >= self.capture.cur_idx {
                    hw_idx - self.capture.cur_idx
                } else {
                    BDL_ENTRIES - (self.capture.cur_idx - hw_idx)
                };
                if filled == 0 {
                    break;
                }

                let idx = self.capture.cur_idx;
                let buffer_addr = self.capture.buffers + idx * BUF_SIZE;
                let to_copy = BUF_SIZE.min(buf.len() - dst);

                unsafe {
                    core::ptr::copy_nonoverlapping(
                        buffer_addr as *const u8,
                        buf.as_mut_ptr().add(dst),
                        to_copy,
                    );
                }
                dst += to_copy;

                if to_copy == BUF_SIZE {
                    unsafe {
                        ((stream_base + REG_SD_LVI as u64) as *mut u8).write_volatile(idx as u8);
                    }
                    self.capture.cur_idx = (idx + 1) % BDL_ENTRIES;
                }
            }

            Ok(dst)
        }
    }

    pub static GLOBAL_HDA: crate::arch::sync::spinlock::SpinLock<Option<HdaDriver>> =
        crate::arch::sync::spinlock::SpinLock::new(None);
    pub static HDA_PRESENT: AtomicBool = AtomicBool::new(false);
}

pub use hda::GLOBAL_HDA;
pub use hda::HDA_PRESENT;

pub mod vfs_node {
    pub fn pcspeaker_write(buf: &[u8]) -> Result<usize, u64> {
        if buf.len() >= 4 {
            let freq = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
            super::pcspeaker::play_tone(freq);
            Ok(buf.len())
        } else if buf.len() >= 2 {
            let freq = u16::from_le_bytes([buf[0], buf[1]]) as u32;
            super::pcspeaker::play_tone(freq);
            Ok(buf.len())
        } else if !buf.is_empty() && buf[0] == 0 {
            super::pcspeaker::stop_tone();
            Ok(buf.len())
        } else {
            Ok(buf.len())
        }
    }

    pub fn dsp_write(buf: &[u8]) -> Result<usize, u64> {
        let mut guard = super::ac97::GLOBAL_AC97.lock();
        if let Some(ac97) = guard.as_mut() {
            ac97.write_pcm_samples(buf)
        } else {
            Ok(buf.len())
        }
    }

    pub fn dsp_read(buf: &mut [u8]) -> Result<usize, u64> {
        let mut guard = super::ac97::GLOBAL_AC97.lock();
        if let Some(ac97) = guard.as_mut() {
            ac97.read_pcm_samples(buf)
        } else {
            Ok(0)
        }
    }

    pub fn hda_write_stream(buf: &[u8]) -> Result<usize, u64> {
        let mut guard = super::hda::GLOBAL_HDA.lock();
        if let Some(hda) = guard.as_mut() {
            hda.write_stream(0, buf)
        } else {
            Ok(buf.len())
        }
    }

    pub fn hda_read_stream(buf: &mut [u8]) -> Result<usize, u64> {
        let mut guard = super::hda::GLOBAL_HDA.lock();
        if let Some(hda) = guard.as_mut() {
            hda.read_stream(0, buf)
        } else {
            Ok(0)
        }
    }

    pub fn hda_ioctl(cmd: u64, arg: u64) -> u64 {
        // HDA uses the same ALSA UAPI as AC97; delegate to dsp_ioctl
        super::vfs_node::dsp_ioctl(cmd, arg)
    }

    pub fn dsp_flush() -> Result<usize, u64> {
        let mut guard = super::ac97::GLOBAL_AC97.lock();
        if let Some(ac97) = guard.as_mut() {
            ac97.flush();
        }
        Ok(0)
    }

    #[repr(C)]
    struct SndXferi {
        result: i64,
        buf: u64,
        frames: u64,
    }

    pub fn dsp_ioctl(cmd: u64, arg: u64) -> u64 {
        let masked_cmd = cmd & 0xFFFF;
        let mut guard = super::ac97::GLOBAL_AC97.lock();

        if cmd == 0x40184150 || masked_cmd == 0x4150 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, core::mem::size_of::<SndXferi>()) {
                let xferi = unsafe { &mut *(arg as *mut SndXferi) };
                let channels = guard.as_ref().map(|a| a.channels as u64).unwrap_or(2);
                let bytes_per_sample = guard.as_ref().map(|a| (a.fmt_bits as u64) / 8).unwrap_or(2);
                let frame_size = (channels * bytes_per_sample).max(1);
                let total_bytes = (xferi.frames * frame_size) as usize;
                if total_bytes > 0 && crate::memory::space::validate_user_read_range(xferi.buf, total_bytes) {
                    let slice = unsafe { core::slice::from_raw_parts(xferi.buf as *const u8, total_bytes) };
                    if let Some(ac97) = guard.as_mut() {
                        let _ = ac97.write_pcm_samples(slice);
                    }
                    xferi.result = xferi.frames as i64;
                } else {
                    xferi.result = 0;
                }
            }
            return 0;
        }

        if cmd == 0x81204101 || masked_cmd == 0x4101 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 288) {
                let ptr = arg as *mut u8;
                unsafe {
                    core::ptr::write_bytes(ptr, 0, 288);
                    let id_bytes = b"AC97\0";
                    core::ptr::copy_nonoverlapping(id_bytes.as_ptr(), ptr.add(16), id_bytes.len());
                    let name_bytes = b"DragOS AC97 Audio\0";
                    core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), ptr.add(80), name_bytes.len());
                }
            }
            return 0;
        }

        if cmd == 0x81785501 || masked_cmd == 0x5501 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 376) {
                let ptr = arg as *mut u8;
                unsafe {
                    core::ptr::write_bytes(ptr, 0, 376);
                    let id_bytes = b"AC97\0";
                    core::ptr::copy_nonoverlapping(id_bytes.as_ptr(), ptr.add(4), id_bytes.len());
                    let driver_bytes = b"ac97\0";
                    core::ptr::copy_nonoverlapping(driver_bytes.as_ptr(), ptr.add(20), driver_bytes.len());
                    let name_bytes = b"DragOS AC97\0";
                    core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), ptr.add(36), name_bytes.len());
                    let longname_bytes = b"Intel 82801 AC97 Audio Controller\0";
                    core::ptr::copy_nonoverlapping(longname_bytes.as_ptr(), ptr.add(116), longname_bytes.len());
                }
            }
            return 0;
        }

        if cmd == 0x80044100 || masked_cmd == 0x4100 || cmd == 0x80045500 || masked_cmd == 0x5500 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 4) {
                unsafe {
                    *(arg as *mut u32) = 0x0002000f;
                }
            }
            return 0;
        }

        if cmd == 0xc2604110 || cmd == 0xc2604111 || cmd == 0xc0684113 || masked_cmd == 0x4110 || masked_cmd == 0x4111 || masked_cmd == 0x4113 {
            return 0;
        }

        if cmd == 0xc0505510 || masked_cmd == 0x5510 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 80) {
                unsafe {
                    let ptr = arg as *mut u8;
                    core::ptr::write_bytes(ptr, 0, 80);
                }
            }
            return 0;
        }

        if cmd == 0xc1105511 || masked_cmd == 0x5511 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 272) {
                unsafe {
                    core::ptr::write_bytes(arg as *mut u8, 0, 272);
                }
            }
            return 0;
        }

        if cmd == 0xc4c85512 || masked_cmd == 0x5512 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 1224) {
                unsafe {
                    core::ptr::write_bytes(arg as *mut u8, 0, 1224);
                }
            }
            return 0;
        }

        if cmd == 0xc4c85513 || masked_cmd == 0x5513 {
            return 0;
        }

        if cmd == 0x80984120 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 152) {
                unsafe {
                    let ptr = arg as *mut u8;
                    core::ptr::write_bytes(ptr, 0, 152);
                    *(ptr as *mut u32) = 3;
                }
            }
            return 0;
        }

        if masked_cmd == 0x4122 {
            return 0;
        }

        if cmd == 0x80184132 || masked_cmd == 0x4132 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 24) {
                unsafe {
                    let ptr = arg as *mut u8;
                    core::ptr::write_bytes(ptr, 0, 24);
                    *(ptr.add(4) as *mut u32) = 1;
                }
            }
            return 0;
        }

        if masked_cmd == 0x4140 || masked_cmd == 0x4142 {
            if let Some(ac97) = guard.as_mut() {
                ac97.kick_dma();
            }
            return 0;
        }

        if masked_cmd == 0x4143 || masked_cmd == 0x4144 || masked_cmd == 0x5001 {
            if let Some(ac97) = guard.as_mut() {
                ac97.flush();
            }
            return 0;
        }

        if masked_cmd == 0x5000 {
            if let Some(ac97) = guard.as_mut() {
                ac97.reset();
            }
            return 0;
        }

        if masked_cmd == 0x5008 {
            if let Some(ac97) = guard.as_mut() {
                ac97.kick_dma();
            }
            return 0;
        }

        if (cmd & 0xFF00) == 0x4d00 || masked_cmd == 0x4d00 || masked_cmd == 0x4d04 {
            if arg != 0 && crate::memory::space::validate_user_write_range(arg, 4) {
                unsafe {
                    let ptr = arg as *mut u32;
                    if (cmd >> 30) & 1 != 0 {
                        let val = (*ptr & 0xFF) as u8;
                        if let Some(ac97) = guard.as_mut() {
                            ac97.set_volume(val);
                        }
                    }
                    let vol = guard.as_ref().map(|a| a.get_volume()).unwrap_or(100);
                    *ptr = (vol as u32) | ((vol as u32) << 8);
                }
            }
            return 0;
        }

        if arg != 0 && crate::memory::space::validate_user_write_range(arg, 4) {
            unsafe {
                let ptr = arg as *mut u32;
                if masked_cmd == 0x5002 {
                    *ptr = 44100;
                } else if masked_cmd == 0x5003 || masked_cmd == 0x5006 {
                    let val = *ptr;
                    if let Some(ac97) = guard.as_mut() {
                        if val == 1 || val == 2 {
                            ac97.channels = val as u8;
                        }
                    }
                    *ptr = guard.as_ref().map(|a| a.channels as u32).unwrap_or(1);
                } else if masked_cmd == 0x5004 {
                    *ptr = 4096;
                } else if masked_cmd == 0x5005 {
                    let val = *ptr;
                    if let Some(ac97) = guard.as_mut() {
                        if val == 8 {
                            ac97.fmt_bits = 8;
                        } else {
                            ac97.fmt_bits = 16;
                        }
                    }
                    *ptr = guard.as_ref().map(|a| a.fmt_bits as u32).unwrap_or(16);
                }
            }
        }
        0
    }
}
