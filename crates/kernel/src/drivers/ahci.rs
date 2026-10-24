//! Serial ATA (SATA) Advanced Host Controller Interface (AHCI 1.3.1) Driver.
//! Supports AHCI HBA enumeration, port initialization, ATA IDENTIFY, and polled DMA reads/writes.

#![allow(dead_code)]

use core::ptr::{copy_nonoverlapping, read_volatile, write_bytes, write_volatile};
use alloc::vec::Vec;
use shared::memory::address::VirtAddr;

use crate::{
    arch::x86_64::pci::PciAddress,
    drivers::block::BlockDevice,
    drivers::virtio::DmaBuf,
    println,
};

// -----------------------------------------------------------------------------
// Generic Host Control (GHC) Memory Registers (ABAR + Offset)
// -----------------------------------------------------------------------------
const HBA_CAP: usize = 0x00;     // Host Capabilities (32-bit)
const HBA_GHC: usize = 0x04;     // Global Host Control (32-bit)
const HBA_IS: usize = 0x08;      // Interrupt Status (32-bit)
const HBA_PI: usize = 0x0C;      // Ports Implemented (32-bit)
const HBA_VS: usize = 0x10;      // Version (32-bit)
const HBA_CAP2: usize = 0x24;    // Extended Capabilities (32-bit)
const HBA_BOHC: usize = 0x28;    // BIOS/OS Handoff Control and Status (32-bit)

const GHC_HR: u32 = 1 << 0;      // HBA Reset
const GHC_IE: u32 = 1 << 1;      // Interrupt Enable
const GHC_AE: u32 = 1 << 31;     // AHCI Enable

const BOHC_BOS: u32 = 1 << 0;    // BIOS Owned Semaphore
const BOHC_OOS: u32 = 1 << 1;    // OS Owned Semaphore

// -----------------------------------------------------------------------------
// Port Registers (ABAR + 0x100 + Port * 0x80)
// -----------------------------------------------------------------------------
const PORT_CLB: usize = 0x00;    // Command List Base Address (Low 32-bit)
const PORT_CLBU: usize = 0x04;   // Command List Base Address (High 32-bit)
const PORT_FB: usize = 0x08;     // FIS Base Address (Low 32-bit)
const PORT_FBU: usize = 0x0C;    // FIS Base Address (High 32-bit)
const PORT_IS: usize = 0x10;     // Interrupt Status
const PORT_IE: usize = 0x14;     // Interrupt Enable
const PORT_CMD: usize = 0x18;    // Command and Status
const PORT_TFD: usize = 0x20;    // Task File Data
const PORT_SIG: usize = 0x24;    // Signature
const PORT_SSTS: usize = 0x28;   // Serial ATA Status (SCR0: SStatus)
const PORT_SCTL: usize = 0x2C;   // Serial ATA Control (SCR2: SControl)
const PORT_SERR: usize = 0x30;   // Serial ATA Error (SCR1: SError)
const PORT_SACT: usize = 0x34;   // Serial ATA Active (SCR3: SActive)
const PORT_CI: usize = 0x38;     // Command Issue

const PORT_CMD_ST: u32 = 1 << 0;   // Start
const PORT_CMD_SUD: u32 = 1 << 1;  // Spin-Up Device
const PORT_CMD_POD: u32 = 1 << 2;  // Power On Device
const PORT_CMD_FRE: u32 = 1 << 4;  // FIS Receive Enable
const PORT_CMD_FR: u32 = 1 << 14;  // FIS Receive Running
const PORT_CMD_CR: u32 = 1 << 15;  // Command List Running

const SATA_SIG_ATA: u32 = 0x00000101;   // SATA drive
const SATA_SIG_ATAPI: u32 = 0xEB140101; // SATAPI optical drive

const HBA_PORT_DET_PRESENT: u32 = 3;
const HBA_PORT_IPM_ACTIVE: u32 = 1;

const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_IDENTIFY: u8 = 0xEC;

const ATA_DEV_BUSY: u32 = 0x80;
const ATA_DEV_DRQ: u32 = 0x08;

// -----------------------------------------------------------------------------
// In-Memory AHCI Data Structures
// -----------------------------------------------------------------------------

/// AHCI Command Header (32 bytes). 32 entries per Command List (1024 bytes total).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct HbaCmdHeader {
    pub flags: u16,        // CFL: 5 bits, A: 1, W: 1, P: 1, R: 1, B: 1, C: 1, rsv: 1, PMP: 4
    pub prdtl: u16,        // PRDT length in entries
    pub prdbc: u32,        // PRD byte count transferred
    pub ctba: u32,         // Command Table Base Address Low
    pub ctbau: u32,        // Command Table Base Address High
    pub rsv: [u32; 4],
}

/// Physical Region Descriptor Table Entry (16 bytes).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct HbaPrdtEntry {
    pub dba: u32,          // Data Base Address Low
    pub dbau: u32,         // Data Base Address High
    pub rsv0: u32,
    pub dbc: u32,          // Byte count (bit 0..21 = length - 1, bit 31 = Interrupt on Completion)
}

/// Register Host-to-Device (H2D) FIS (20 bytes).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct FisRegH2D {
    pub fis_type: u8,      // 0x27 (FIS_TYPE_REG_H2D)
    pub pmport_c: u8,      // bit 7 = command (1), bits 0..3 = Port Multiplier
    pub command: u8,       // ATA Command
    pub featurel: u8,
    pub lba0: u8,
    pub lba1: u8,
    pub lba2: u8,
    pub device: u8,        // 1 << 6 (LBA mode)
    pub lba3: u8,
    pub lba4: u8,
    pub lba5: u8,
    pub featureh: u8,
    pub countl: u8,        // Sector Count Low
    pub counth: u8,        // Sector Count High
    pub icc: u8,
    pub control: u8,
    pub rsv: [u8; 4],
}

// -----------------------------------------------------------------------------
// AHCI Port Device Implementation
// -----------------------------------------------------------------------------

pub struct AhciPort {
    port_base: VirtAddr,
    cmd_list_dma: DmaBuf,   // 1024 bytes (32 Command Headers)
    recv_fis_dma: DmaBuf,   // 256 bytes
    cmd_table_dma: DmaBuf,  // 4096 bytes (Command Table with FIS and PRDTs)
    data_dma: DmaBuf,       // 4096 bytes bounce buffer
    capacity: u64,          // Capacity in 512-byte sectors
    model: [u8; 40],        // ASCII Model Name
    port_idx: u8,
}

impl AhciPort {
    unsafe fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.port_base.0 + offset as u64) as *const u32) }
    }

    unsafe fn write_reg(&self, offset: usize, val: u32) {
        unsafe { write_volatile((self.port_base.0 + offset as u64) as *mut u32, val) }
    }

    /// Stop the port command and FIS receive engines.
    fn stop_cmd(&self) {
        unsafe {
            let mut cmd = self.read_reg(PORT_CMD);
            cmd &= !(PORT_CMD_ST | PORT_CMD_FRE);
            self.write_reg(PORT_CMD, cmd);

            // Wait until CR (bit 15) and FR (bit 14) are clear
            for _ in 0..100_000 {
                let status = self.read_reg(PORT_CMD);
                if (status & (PORT_CMD_CR | PORT_CMD_FR)) == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }
    }

    /// Start the port command and FIS receive engines.
    fn start_cmd(&self) {
        unsafe {
            // Wait until CR is clear before setting ST
            for _ in 0..100_000 {
                if (self.read_reg(PORT_CMD) & PORT_CMD_CR) == 0 {
                    break;
                }
                core::hint::spin_loop();
            }

            let mut cmd = self.read_reg(PORT_CMD);
            cmd |= PORT_CMD_FRE;
            self.write_reg(PORT_CMD, cmd);
            cmd |= PORT_CMD_ST;
            self.write_reg(PORT_CMD, cmd);
        }
    }

    /// Initialize a single AHCI SATA port and probe connected drive.
    pub fn init(port_base: VirtAddr, port_idx: u8) -> Option<Self> {
        let cmd_list_dma = DmaBuf::new(1024)?;
        let recv_fis_dma = DmaBuf::new(256)?;
        let cmd_table_dma = DmaBuf::new(4096)?;
        let data_dma = DmaBuf::new(65536)?;

        let mut port = Self {
            port_base,
            cmd_list_dma,
            recv_fis_dma,
            cmd_table_dma,
            data_dma,
            capacity: 0,
            model: [0u8; 40],
            port_idx,
        };

        // Zero out allocated DMA memory buffers
        unsafe {
            write_bytes(port.cmd_list_dma.as_mut_ptr(), 0, port.cmd_list_dma.len());
            write_bytes(port.recv_fis_dma.as_mut_ptr(), 0, port.recv_fis_dma.len());
            write_bytes(port.cmd_table_dma.as_mut_ptr(), 0, port.cmd_table_dma.len());
            write_bytes(port.data_dma.as_mut_ptr(), 0, port.data_dma.len());
        }

        // 1. Stop command execution
        port.stop_cmd();

        // 2. Set Command List & Received FIS Base Addresses
        unsafe {
            port.write_reg(PORT_CLB, port.cmd_list_dma.phys().0 as u32);
            port.write_reg(PORT_CLBU, (port.cmd_list_dma.phys().0 >> 32) as u32);

            port.write_reg(PORT_FB, port.recv_fis_dma.phys().0 as u32);
            port.write_reg(PORT_FBU, (port.recv_fis_dma.phys().0 >> 32) as u32);

            // Clear interrupt status and error registers
            port.write_reg(PORT_IS, 0xFFFF_FFFF);
            port.write_reg(PORT_SERR, 0xFFFF_FFFF);
        }

        // 3. Start command execution
        port.start_cmd();

        // 4. Send ATA IDENTIFY command (0xEC)
        if port.identify().is_err() {
            return None;
        }

        Some(port)
    }

    /// Issue an ATA command using Command Slot 0 and poll for completion.
    fn issue_command(
        &mut self,
        cmd: u8,
        lba: u64,
        count: u16,
        is_write: bool,
        buf_phys: u64,
        byte_count: u32,
    ) -> Result<(), &'static str> {
        // 1. Prepare Command Header 0
        let cmd_header = unsafe { &mut *(self.cmd_list_dma.as_mut_ptr() as *mut HbaCmdHeader) };
        let fis_dwords = (core::mem::size_of::<FisRegH2D>() / 4) as u16;
        let mut flags = fis_dwords & 0x1F;
        if is_write {
            flags |= 1 << 6; // Write bit
        }
        cmd_header.flags = flags;
        cmd_header.prdtl = 1; // 1 PRDT entry
        cmd_header.prdbc = 0;
        cmd_header.ctba = self.cmd_table_dma.phys().0 as u32;
        cmd_header.ctbau = (self.cmd_table_dma.phys().0 >> 32) as u32;

        // 2. Prepare Command Table: CFIS (at offset 0) + PRDT (at offset 0x80)
        let cfis_ptr = self.cmd_table_dma.as_mut_ptr() as *mut FisRegH2D;
        unsafe {
            write_bytes(self.cmd_table_dma.as_mut_ptr(), 0, 0x80 + core::mem::size_of::<HbaPrdtEntry>());

            let fis = FisRegH2D {
                fis_type: 0x27, // Register H2D
                pmport_c: 1 << 7, // Command
                command: cmd,
                featurel: 0,
                lba0: lba as u8,
                lba1: (lba >> 8) as u8,
                lba2: (lba >> 16) as u8,
                device: 1 << 6, // LBA mode
                lba3: (lba >> 24) as u8,
                lba4: (lba >> 32) as u8,
                lba5: (lba >> 40) as u8,
                featureh: 0,
                countl: count as u8,
                counth: (count >> 8) as u8,
                icc: 0,
                control: 0,
                rsv: [0; 4],
            };
            cfis_ptr.write(fis);

            let prdt_ptr = (self.cmd_table_dma.as_mut_ptr().add(0x80)) as *mut HbaPrdtEntry;
            let prdt = HbaPrdtEntry {
                dba: buf_phys as u32,
                dbau: (buf_phys >> 32) as u32,
                rsv0: 0,
                dbc: (byte_count - 1) & 0x003F_FFFF, // Byte count (0-based)
            };
            prdt_ptr.write(prdt);
        }

        // 3. Wait until port is not busy
        for _ in 0..100_000 {
            let tfd = unsafe { self.read_reg(PORT_TFD) };
            if (tfd & (ATA_DEV_BUSY | ATA_DEV_DRQ)) == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // 4. Issue command by setting bit 0 in PORT_CI
        unsafe {
            self.write_reg(PORT_CI, 1 << 0);
        }

        // 5. Poll for completion
        for _ in 0..2_000_000 {
            let ci = unsafe { self.read_reg(PORT_CI) };
            if (ci & (1 << 0)) == 0 {
                // Command finished!
                let is = unsafe { self.read_reg(PORT_IS) };
                if (is & (1 << 30)) != 0 { // Task File Error Status
                    return Err("AHCI: task file error");
                }
                return Ok(());
            }
            core::hint::spin_loop();
        }

        Err("AHCI: command timed out")
    }

    /// Identify ATA device, read capacity and model name.
    fn identify(&mut self) -> Result<(), &'static str> {
        let phys = self.data_dma.phys().0;
        self.issue_command(ATA_CMD_IDENTIFY, 0, 0, false, phys, 512)?;

        let identify_buf = unsafe {
            core::slice::from_raw_parts(self.data_dma.as_mut_ptr() as *const u16, 256)
        };

        // Words 27..46: Model number (40 ASCII characters, byte swapped)
        for i in 0..20 {
            let word = identify_buf[27 + i];
            self.model[i * 2] = (word >> 8) as u8;
            self.model[i * 2 + 1] = (word & 0xFF) as u8;
        }

        // Words 100..103: 48-bit LBA total sectors count
        let sectors_48 = (identify_buf[100] as u64)
            | ((identify_buf[101] as u64) << 16)
            | ((identify_buf[102] as u64) << 32)
            | ((identify_buf[103] as u64) << 48);

        // Words 60..61: 28-bit LBA total sectors count
        let sectors_28 = (identify_buf[60] as u64) | ((identify_buf[61] as u64) << 16);

        self.capacity = if sectors_48 != 0 { sectors_48 } else { sectors_28 };

        let model_str = core::str::from_utf8(&self.model).unwrap_or("Unknown SATA Drive").trim();
        println!(
            "AHCI: Port {} SATA Drive detected: \"{}\", capacity: {} MB ({} sectors)",
            self.port_idx,
            model_str,
            (self.capacity * 512) / (1024 * 1024),
            self.capacity
        );

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// BlockDevice Trait Implementation for AhciPort
// -----------------------------------------------------------------------------

impl BlockDevice for AhciPort {
    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        self.read_sectors(sector, buf)
    }

    fn write_sector(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        self.write_sectors(sector, data)
    }

    fn read_sectors(&mut self, mut sector: u64, mut buf: &mut [u8]) -> Result<(), &'static str> {
        let sec_sz = 512;
        if buf.len() % sec_sz != 0 {
            return Err("buffer size not a multiple of sector size");
        }
        let total_sectors = (buf.len() / sec_sz) as u64;
        if sector + total_sectors > self.capacity {
            return Err("sector out of range");
        }
        let max_chunk = self.data_dma.len();
        while !buf.is_empty() {
            let chunk_len = buf.len().min(max_chunk);
            let chunk_sectors = (chunk_len / sec_sz) as u16;
            let phys = self.data_dma.phys().0;
            self.issue_command(ATA_CMD_READ_DMA_EXT, sector, chunk_sectors, false, phys, chunk_len as u32)?;
            unsafe {
                copy_nonoverlapping(self.data_dma.as_mut_ptr(), buf.as_mut_ptr(), chunk_len);
            }
            sector += chunk_sectors as u64;
            let (_, rest) = core::mem::take(&mut buf).split_at_mut(chunk_len);
            buf = rest;
        }
        Ok(())
    }

    fn write_sectors(&mut self, mut sector: u64, mut data: &[u8]) -> Result<(), &'static str> {
        let sec_sz = 512;
        if data.len() % sec_sz != 0 {
            return Err("data size not a multiple of sector size");
        }
        let total_sectors = (data.len() / sec_sz) as u64;
        if sector + total_sectors > self.capacity {
            return Err("sector out of range");
        }
        let max_chunk = self.data_dma.len();
        while !data.is_empty() {
            let chunk_len = data.len().min(max_chunk);
            let chunk_sectors = (chunk_len / sec_sz) as u16;
            unsafe {
                copy_nonoverlapping(data.as_ptr(), self.data_dma.as_mut_ptr(), chunk_len);
            }
            let phys = self.data_dma.phys().0;
            self.issue_command(ATA_CMD_WRITE_DMA_EXT, sector, chunk_sectors, true, phys, chunk_len as u32)?;
            sector += chunk_sectors as u64;
            data = &data[chunk_len..];
        }
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }
}

// -----------------------------------------------------------------------------
// Top-Level AHCI Controller Driver
// -----------------------------------------------------------------------------

pub struct AhciDriver {
    pub ports: Vec<AhciPort>,
}

impl AhciDriver {
    /// Initialize the AHCI controller at the given PCI address.
    pub fn init_pci(pci_addr: &PciAddress) -> Option<Self> {
        pci_addr.enable_memory_and_master();

        // BAR 5 is the AHCI Base Address (ABAR)
        let (abar, _) = pci_addr.map_bar(5).ok()?;
        println!("AHCI: Controller found, ABAR mapped at {:#x}", abar.0);

        unsafe {
            // 1. Enable AHCI Mode (GHC.AE = 1)
            let mut ghc = read_volatile((abar.0 + HBA_GHC as u64) as *const u32);
            ghc |= GHC_AE;
            write_volatile((abar.0 + HBA_GHC as u64) as *mut u32, ghc);

            // 2. BIOS / OS Handoff (if supported by CAP2.BOH)
            let cap2 = read_volatile((abar.0 + HBA_CAP2 as u64) as *const u32);
            if (cap2 & 1) != 0 {
                let bohc_ptr = (abar.0 + HBA_BOHC as u64) as *mut u32;
                let mut bohc = read_volatile(bohc_ptr);
                bohc |= BOHC_OOS; // Set OS Ownership
                write_volatile(bohc_ptr, bohc);

                for _ in 0..50_000 {
                    if (read_volatile(bohc_ptr) & BOHC_BOS) == 0 {
                        break;
                    }
                    core::hint::spin_loop();
                }
            }
        }

        let pi = unsafe { read_volatile((abar.0 + HBA_PI as u64) as *const u32) };
        let mut ports = Vec::new();

        for i in 0..32 {
            if (pi & (1 << i)) != 0 {
                let port_base = VirtAddr::new(abar.0 + 0x100 + (i as u64 * 0x80));
                let ssts = unsafe { read_volatile((port_base.0 + PORT_SSTS as u64) as *const u32) };
                let det = ssts & 0x0F;
                let ipm = (ssts >> 8) & 0x0F;

                if det == HBA_PORT_DET_PRESENT && ipm == HBA_PORT_IPM_ACTIVE {
                    let sig = unsafe { read_volatile((port_base.0 + PORT_SIG as u64) as *const u32) };
                    if sig == SATA_SIG_ATA {
                        if let Some(port) = AhciPort::init(port_base, i as u8) {
                            ports.push(port);
                        }
                    }
                }
            }
        }

        if ports.is_empty() {
            println!("AHCI: No active SATA drives found on controller.");
            return None;
        }

        Some(Self { ports })
    }
}
