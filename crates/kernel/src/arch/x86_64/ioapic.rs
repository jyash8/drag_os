//! I/O APIC driver (memory-mapped at 0xFEC0_0000).
//!
//! Provides programmable interrupt routing from hardware IRQ lines to LAPIC vectors.

use shared::boot::boot_info::HHDM_BASE;
use shared::memory::address::VirtAddr;

pub const IOAPIC_BASE_PHYS: u64 = 0xFEC0_0000;

// Direct I/O APIC register offsets
pub const IOREGSEL: u64 = 0x00;
pub const IOWIN: u64 = 0x10;

// Indirect I/O APIC registers (selected via IOREGSEL)
pub const IOAPIC_ID: u8 = 0x00;
pub const IOAPIC_VER: u8 = 0x01;
pub const IOAPIC_ARB: u8 = 0x02;
pub const IOREDTBL_BASE: u8 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    Fixed = 0b000,
    LowestPriority = 0b001,
    SMI = 0b010,
    NMI = 0b100,
    INIT = 0b101,
    ExtINT = 0b111,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationMode {
    Physical = 0,
    Logical = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    ActiveHigh = 0,
    ActiveLow = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerMode {
    Edge = 0,
    Level = 1,
}

/// A 64-bit I/O APIC Redirection Table Entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedirectionEntry {
    pub vector: u8,
    pub delivery_mode: DeliveryMode,
    pub dest_mode: DestinationMode,
    pub polarity: Polarity,
    pub trigger_mode: TriggerMode,
    pub masked: bool,
    pub destination: u8,
}

impl RedirectionEntry {
    pub const fn new(vector: u8, dest_apic_id: u8) -> Self {
        Self {
            vector,
            delivery_mode: DeliveryMode::Fixed,
            dest_mode: DestinationMode::Physical,
            polarity: Polarity::ActiveHigh,
            trigger_mode: TriggerMode::Edge,
            masked: false,
            destination: dest_apic_id,
        }
    }

    pub fn to_raw(&self) -> (u32, u32) {
        let mut low: u32 = self.vector as u32;
        low |= (self.delivery_mode as u32) << 8;
        low |= (self.dest_mode as u32) << 11;
        low |= (self.polarity as u32) << 13;
        low |= (self.trigger_mode as u32) << 15;
        if self.masked {
            low |= 1 << 16;
        }

        let high: u32 = (self.destination as u32) << 24;
        (low, high)
    }

    pub fn from_raw(low: u32, high: u32) -> Self {
        let vector = (low & 0xFF) as u8;
        let delivery_mode = match (low >> 8) & 0b111 {
            0b001 => DeliveryMode::LowestPriority,
            0b010 => DeliveryMode::SMI,
            0b100 => DeliveryMode::NMI,
            0b101 => DeliveryMode::INIT,
            0b111 => DeliveryMode::ExtINT,
            _ => DeliveryMode::Fixed,
        };
        let dest_mode = if (low & (1 << 11)) != 0 {
            DestinationMode::Logical
        } else {
            DestinationMode::Physical
        };
        let polarity = if (low & (1 << 13)) != 0 {
            Polarity::ActiveLow
        } else {
            Polarity::ActiveHigh
        };
        let trigger_mode = if (low & (1 << 15)) != 0 {
            TriggerMode::Level
        } else {
            TriggerMode::Edge
        };
        let masked = (low & (1 << 16)) != 0;
        let destination = ((high >> 24) & 0xFF) as u8;

        Self {
            vector,
            delivery_mode,
            dest_mode,
            polarity,
            trigger_mode,
            masked,
            destination,
        }
    }
}

pub struct IoApic {
    pub base: VirtAddr,
}

impl IoApic {
    pub const fn new(base: VirtAddr) -> Self {
        Self { base }
    }

    pub unsafe fn read(&self, reg: u8) -> u32 {
        let base_ptr = self.base.0 as *mut u32;
        let reg_ptr = base_ptr.add((IOREGSEL / 4) as usize);
        let win_ptr = base_ptr.add((IOWIN / 4) as usize);

        unsafe {
            core::ptr::write_volatile(reg_ptr, reg as u32);
            core::ptr::read_volatile(win_ptr)
        }
    }

    pub unsafe fn write(&self, reg: u8, val: u32) {
        let base_ptr = self.base.0 as *mut u32;
        let reg_ptr = base_ptr.add((IOREGSEL / 4) as usize);
        let win_ptr = base_ptr.add((IOWIN / 4) as usize);

        unsafe {
            core::ptr::write_volatile(reg_ptr, reg as u32);
            core::ptr::write_volatile(win_ptr, val);
        }
    }

    pub fn id(&self) -> u32 {
        unsafe { (self.read(IOAPIC_ID) >> 24) & 0xF }
    }

    pub fn version(&self) -> u32 {
        unsafe { self.read(IOAPIC_VER) & 0xFF }
    }

    pub fn max_redirection_entries(&self) -> u32 {
        unsafe { ((self.read(IOAPIC_VER) >> 16) & 0xFF) + 1 }
    }

    pub fn set_redirection(&self, gsi: u8, entry: RedirectionEntry) {
        let (low, high) = entry.to_raw();
        let reg_low = IOREDTBL_BASE + gsi * 2;
        let reg_high = reg_low + 1;

        unsafe {
            // Mask first to avoid spurious interrupt while updating
            self.write(reg_low, low | (1 << 16));
            self.write(reg_high, high);
            self.write(reg_low, low);
        }
    }

    pub fn get_redirection(&self, gsi: u8) -> RedirectionEntry {
        let reg_low = IOREDTBL_BASE + gsi * 2;
        let reg_high = reg_low + 1;

        let low = unsafe { self.read(reg_low) };
        let high = unsafe { self.read(reg_high) };
        RedirectionEntry::from_raw(low, high)
    }

    pub fn mask(&self, gsi: u8) {
        let reg_low = IOREDTBL_BASE + gsi * 2;
        unsafe {
            let low = self.read(reg_low);
            self.write(reg_low, low | (1 << 16));
        }
    }

    pub fn unmask(&self, gsi: u8) {
        let reg_low = IOREDTBL_BASE + gsi * 2;
        unsafe {
            let low = self.read(reg_low);
            self.write(reg_low, low & !(1 << 16));
        }
    }
}

pub fn ioapic_base_virt() -> VirtAddr {
    VirtAddr(HHDM_BASE.0 + IOAPIC_BASE_PHYS)
}

pub fn default_ioapic() -> IoApic {
    IoApic::new(ioapic_base_virt())
}

pub fn ioapic_read(reg: u8) -> u32 {
    unsafe { default_ioapic().read(reg) }
}

pub fn ioapic_write(reg: u8, val: u32) {
    unsafe { default_ioapic().write(reg, val) }
}

pub fn ioapic_id() -> u32 {
    default_ioapic().id()
}

pub fn ioapic_version() -> u32 {
    default_ioapic().version()
}

pub fn ioapic_max_redirection_entries() -> u32 {
    default_ioapic().max_redirection_entries()
}

pub fn ioapic_set_redirection(gsi: u8, entry: RedirectionEntry) {
    default_ioapic().set_redirection(gsi, entry)
}

pub fn ioapic_get_redirection(gsi: u8) -> RedirectionEntry {
    default_ioapic().get_redirection(gsi)
}

pub fn ioapic_route_irq(irq: u8, vector: u8, dest_apic_id: u32) {
    let entry = RedirectionEntry {
        vector,
        delivery_mode: DeliveryMode::Fixed,
        dest_mode: DestinationMode::Physical,
        polarity: Polarity::ActiveHigh,
        trigger_mode: TriggerMode::Edge,
        masked: false,
        destination: dest_apic_id as u8,
    };
    default_ioapic().set_redirection(irq, entry);
}

pub fn ioapic_mask_irq(irq: u8) {
    default_ioapic().mask(irq)
}

pub fn ioapic_unmask_irq(irq: u8) {
    default_ioapic().unmask(irq)
}

pub unsafe fn init() {
    let ioapic = default_ioapic();
    let count = ioapic.max_redirection_entries();
    // Initially mask all redirection entries
    for i in 0..count {
        ioapic.mask(i as u8);
    }
}
