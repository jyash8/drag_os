//! NVM Express (NVMe 1.4) Storage Controller Driver.
//! Supports PCIe NVMe enumeration, Admin & I/O queue creation, Identify Controller/Namespace,
//! and polled DMA sector read/write implementing the BlockDevice trait.

#![allow(dead_code)]

use core::ptr::{copy_nonoverlapping, read_volatile, write_bytes, write_volatile};
use shared::memory::address::VirtAddr;

use crate::{
    arch::x86_64::pci::PciAddress,
    drivers::block::BlockDevice,
    drivers::virtio::DmaBuf,
    println,
};

// -----------------------------------------------------------------------------
// NVMe Controller Registers (BAR0 + Offset)
// -----------------------------------------------------------------------------
const NVME_REG_CAP: usize = 0x00;     // Controller Capabilities (64-bit)
const NVME_REG_VS: usize = 0x08;      // Version (32-bit)
const NVME_REG_INTMS: usize = 0x0C;   // Interrupt Mask Set
const NVME_REG_INTMC: usize = 0x10;   // Interrupt Mask Clear
const NVME_REG_CC: usize = 0x14;      // Controller Configuration (32-bit)
const NVME_REG_CSTS: usize = 0x1C;    // Controller Status (32-bit)
const NVME_REG_AQA: usize = 0x24;     // Admin Queue Attributes (32-bit)
const NVME_REG_ASQ: usize = 0x28;     // Admin Submission Queue Base (64-bit)
const NVME_REG_ACQ: usize = 0x30;     // Admin Completion Queue Base (64-bit)

const CC_EN: u32 = 1 << 0;            // Enable
const CC_CSS_NVM: u32 = 0 << 4;       // NVM Command Set
const CC_MPS_4K: u32 = 0 << 7;        // Memory Page Size (2^(12 + 0) = 4096)
const CC_IOSQES_64: u32 = 6 << 16;    // I/O Submission Queue Entry Size (2^6 = 64 bytes)
const CC_IOCQES_16: u32 = 4 << 20;    // I/O Completion Queue Entry Size (2^4 = 16 bytes)

const CSTS_RDY: u32 = 1 << 0;         // Controller Ready

// Admin Command Opcodes
const NVME_ADMIN_CMD_CREATE_IO_SQ: u8 = 0x01;
const NVME_ADMIN_CMD_CREATE_IO_CQ: u8 = 0x05;
const NVME_ADMIN_CMD_IDENTIFY: u8 = 0x06;

// NVM Command Opcodes
const NVME_NVM_CMD_WRITE: u8 = 0x01;
const NVME_NVM_CMD_READ: u8 = 0x02;

const ADMIN_QUEUE_SIZE: u16 = 32;
const IO_QUEUE_SIZE: u16 = 32;

// -----------------------------------------------------------------------------
// NVMe Structures
// -----------------------------------------------------------------------------

/// NVMe Submission Queue Entry (64 bytes).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct NvmeCmd {
    pub opcode: u8,
    pub flags: u8,
    pub cid: u16,
    pub nsid: u32,
    pub reserved0: u64,
    pub mptr: u64,
    pub dptr_prp1: u64,
    pub dptr_prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

/// NVMe Completion Queue Entry (16 bytes).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct NvmeCqe {
    pub result: u32,
    pub reserved: u32,
    pub sq_head: u16,
    pub sq_id: u16,
    pub cid: u16,
    pub status: u16, // bit 0 = Phase Tag (P), bits 1..15 = Status Code
}

pub struct NvmeQueue {
    sq_dma: DmaBuf,
    cq_dma: DmaBuf,
    sq_tail: u16,
    cq_head: u16,
    cq_phase: u16,
    size: u16,
    sq_doorbell: VirtAddr,
    cq_doorbell: VirtAddr,
}

impl NvmeQueue {
    pub fn new(size: u16, sq_doorbell: VirtAddr, cq_doorbell: VirtAddr) -> Option<Self> {
        let sq_dma = DmaBuf::new(size as usize * core::mem::size_of::<NvmeCmd>())?;
        let cq_dma = DmaBuf::new(size as usize * core::mem::size_of::<NvmeCqe>())?;

        unsafe {
            write_bytes(sq_dma.as_mut_ptr(), 0, sq_dma.len());
            write_bytes(cq_dma.as_mut_ptr(), 0, cq_dma.len());
        }

        Some(Self {
            sq_dma,
            cq_dma,
            sq_tail: 0,
            cq_head: 0,
            cq_phase: 1, // Hardware initializes CQ Phase to 1
            size,
            sq_doorbell,
            cq_doorbell,
        })
    }

    pub fn submit_and_wait(&mut self, mut cmd: NvmeCmd) -> Result<u32, &'static str> {
        cmd.cid = self.sq_tail;

        // Write command into SQ
        let sq_ptr = self.sq_dma.as_mut_ptr() as *mut NvmeCmd;
        unsafe {
            sq_ptr.add(self.sq_tail as usize).write(cmd);
        }

        // Advance SQ Tail and ring doorbell
        self.sq_tail = (self.sq_tail + 1) % self.size;
        unsafe {
            write_volatile(self.sq_doorbell.0 as *mut u32, self.sq_tail as u32);
        }

        // Poll CQ for completion
        let cq_ptr = self.cq_dma.as_mut_ptr() as *const NvmeCqe;
        for _ in 0..2_000_000 {
            let cqe = unsafe { read_volatile(cq_ptr.add(self.cq_head as usize)) };
            let phase = cqe.status & 1;

            if phase == self.cq_phase {
                // Command finished!
                let status_code = (cqe.status >> 1) & 0x7FFF;
                let result = cqe.result;

                // Advance CQ Head
                self.cq_head = (self.cq_head + 1) % self.size;
                if self.cq_head == 0 {
                    self.cq_phase ^= 1; // Invert phase bit when wrapping around
                }

                // Ring CQ Doorbell
                unsafe {
                    write_volatile(self.cq_doorbell.0 as *mut u32, self.cq_head as u32);
                }

                if status_code != 0 {
                    return Err("NVMe: command returned non-zero status");
                }
                return Ok(result);
            }
            core::hint::spin_loop();
        }

        Err("NVMe: command timed out")
    }
}

// -----------------------------------------------------------------------------
// Top-Level NVMe Driver Implementation
// -----------------------------------------------------------------------------

pub struct NvmeDriver {
    bar0: VirtAddr,
    admin_q: NvmeQueue,
    io_q: NvmeQueue,
    data_dma: DmaBuf,
    capacity: u64, // Capacity in 512-byte sectors
    model: [u8; 40],
}

impl NvmeDriver {
    unsafe fn read_reg32(&self, off: usize) -> u32 {
        read_volatile((self.bar0.0 + off as u64) as *const u32)
    }

    unsafe fn write_reg32(&self, off: usize, val: u32) {
        write_volatile((self.bar0.0 + off as u64) as *mut u32, val)
    }

    unsafe fn write_reg64(&self, off: usize, val: u64) {
        write_volatile((self.bar0.0 + off as u64) as *mut u64, val)
    }

    pub fn init_pci(pci_addr: &PciAddress) -> Option<Self> {
        pci_addr.enable_memory_and_master();

        // BAR 0 is the NVMe Controller Base Address
        let (bar0, _) = pci_addr.map_bar(0).ok()?;
        println!("NVMe: Controller found, BAR0 mapped at {:#x}", bar0.0);

        let cap = unsafe { read_volatile(bar0.0 as *const u64) };
        let dstrd = ((cap >> 32) & 0xF) as u32; // Doorbell Stride (2^(2 + dstrd) bytes)
        let stride = 4 << dstrd;

        let get_doorbell = |qid: u32, is_cq: bool| -> VirtAddr {
            let idx = (2 * qid) + if is_cq { 1 } else { 0 };
            VirtAddr::new(bar0.0 + 0x1000 + (idx as u64 * stride as u64))
        };

        let admin_q = NvmeQueue::new(ADMIN_QUEUE_SIZE, get_doorbell(0, false), get_doorbell(0, true))?;

        // 1. Reset / Disable Controller (CC.EN = 0)
        unsafe {
            let mut cc = read_volatile((bar0.0 + NVME_REG_CC as u64) as *const u32);
            cc &= !CC_EN;
            write_volatile((bar0.0 + NVME_REG_CC as u64) as *mut u32, cc);

            // Wait until CSTS.RDY == 0
            for _ in 0..100_000 {
                if (read_volatile((bar0.0 + NVME_REG_CSTS as u64) as *const u32) & CSTS_RDY) == 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }

        // 2. Set Admin Queue Attributes & Base Addresses
        let aqa = ((ADMIN_QUEUE_SIZE as u32 - 1) << 16) | (ADMIN_QUEUE_SIZE as u32 - 1);
        unsafe {
            write_volatile((bar0.0 + NVME_REG_AQA as u64) as *mut u32, aqa);
            write_volatile((bar0.0 + NVME_REG_ASQ as u64) as *mut u64, admin_q.sq_dma.phys().0);
            write_volatile((bar0.0 + NVME_REG_ACQ as u64) as *mut u64, admin_q.cq_dma.phys().0);
        }

        // 3. Configure and Enable Controller (CC.EN = 1)
        unsafe {
            let cc = CC_EN | CC_CSS_NVM | CC_MPS_4K | CC_IOSQES_64 | CC_IOCQES_16;
            write_volatile((bar0.0 + NVME_REG_CC as u64) as *mut u32, cc);

            // Wait until CSTS.RDY == 1
            for _ in 0..200_000 {
                if (read_volatile((bar0.0 + NVME_REG_CSTS as u64) as *const u32) & CSTS_RDY) != 0 {
                    break;
                }
                core::hint::spin_loop();
            }
        }

        let io_q = NvmeQueue::new(IO_QUEUE_SIZE, get_doorbell(1, false), get_doorbell(1, true))?;
        let data_dma = DmaBuf::new(4096)?;

        let mut driver = Self {
            bar0,
            admin_q,
            io_q,
            data_dma,
            capacity: 0,
            model: [0u8; 40],
        };

        // 4. Create I/O CQ (QID 1)
        let mut create_cq = NvmeCmd::default();
        create_cq.opcode = NVME_ADMIN_CMD_CREATE_IO_CQ;
        create_cq.dptr_prp1 = driver.io_q.cq_dma.phys().0;
        create_cq.cdw10 = ((IO_QUEUE_SIZE as u32 - 1) << 16) | 1; // QSIZE, QID=1
        create_cq.cdw11 = 1; // Physically Contiguous, Interrupts Disabled
        driver.admin_q.submit_and_wait(create_cq).ok()?;

        // 5. Create I/O SQ (QID 1)
        let mut create_sq = NvmeCmd::default();
        create_sq.opcode = NVME_ADMIN_CMD_CREATE_IO_SQ;
        create_sq.dptr_prp1 = driver.io_q.sq_dma.phys().0;
        create_sq.cdw10 = ((IO_QUEUE_SIZE as u32 - 1) << 16) | 1; // QSIZE, QID=1
        create_sq.cdw11 = (1 << 16) | 1; // CQID=1, Physically Contiguous
        driver.admin_q.submit_and_wait(create_sq).ok()?;

        // 6. Identify Controller & Namespace
        driver.identify().ok()?;

        Some(driver)
    }

    fn identify(&mut self) -> Result<(), &'static str> {
        let phys = self.data_dma.phys().0;

        // Identify Controller (CNS = 1)
        let mut cmd = NvmeCmd::default();
        cmd.opcode = NVME_ADMIN_CMD_IDENTIFY;
        cmd.dptr_prp1 = phys;
        cmd.cdw10 = 1; // CNS = 1 (Controller)
        self.admin_q.submit_and_wait(cmd)?;

        let id_ctrl = self.data_dma.as_mut_ptr();
        unsafe {
            copy_nonoverlapping(id_ctrl.add(24), self.model.as_mut_ptr(), 40);
        }

        // Identify Namespace 1 (CNS = 0, NSID = 1)
        let mut cmd_ns = NvmeCmd::default();
        cmd_ns.opcode = NVME_ADMIN_CMD_IDENTIFY;
        cmd_ns.nsid = 1;
        cmd_ns.dptr_prp1 = phys;
        cmd_ns.cdw10 = 0; // CNS = 0 (Namespace)
        self.admin_q.submit_and_wait(cmd_ns)?;

        let id_ns = self.data_dma.as_mut_ptr();
        let nsze = unsafe { read_volatile(id_ns as *const u64) };
        self.capacity = nsze;

        let model_str = core::str::from_utf8(&self.model).unwrap_or("NVMe SSD").trim();
        println!(
            "NVMe: Namespace 1 detected: \"{}\", capacity: {} MB ({} sectors)",
            model_str,
            (self.capacity * 512) / (1024 * 1024),
            self.capacity
        );

        Ok(())
    }
}

// -----------------------------------------------------------------------------
// BlockDevice Trait Implementation for NvmeDriver
// -----------------------------------------------------------------------------

impl BlockDevice for NvmeDriver {
    fn read_sector(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        if sector >= self.capacity || buf.len() < 512 {
            return Err("sector out of range or buffer too small");
        }

        let mut cmd = NvmeCmd::default();
        cmd.opcode = NVME_NVM_CMD_READ;
        cmd.nsid = 1;
        cmd.dptr_prp1 = self.data_dma.phys().0;
        cmd.cdw10 = sector as u32;
        cmd.cdw11 = (sector >> 32) as u32;
        cmd.cdw12 = 0; // Number of blocks = 0 + 1 = 1 sector

        self.io_q.submit_and_wait(cmd)?;

        unsafe {
            copy_nonoverlapping(self.data_dma.as_mut_ptr(), buf.as_mut_ptr(), 512);
        }
        Ok(())
    }

    fn write_sector(&mut self, sector: u64, data: &[u8]) -> Result<(), &'static str> {
        if sector >= self.capacity || data.len() < 512 {
            return Err("sector out of range or buffer too small");
        }

        unsafe {
            copy_nonoverlapping(data.as_ptr(), self.data_dma.as_mut_ptr(), 512);
        }

        let mut cmd = NvmeCmd::default();
        cmd.opcode = NVME_NVM_CMD_WRITE;
        cmd.nsid = 1;
        cmd.dptr_prp1 = self.data_dma.phys().0;
        cmd.cdw10 = sector as u32;
        cmd.cdw11 = (sector >> 32) as u32;
        cmd.cdw12 = 0; // Number of blocks = 0 + 1 = 1 sector

        self.io_q.submit_and_wait(cmd)?;
        Ok(())
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }
}
