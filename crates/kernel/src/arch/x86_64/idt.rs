use core::arch::asm;

#[derive(Debug, Clone, Copy)]
#[repr(C, packed(2))]
pub struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    pub const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    pub fn set_handler(&mut self, handler: u64, cs: u16) {
        self.offset_low = handler as u16;
        self.selector = cs;
        self.ist = 0;
        self.type_attr = 0x8E; // Present (0x80) | Interrupt Gate (0xE) | DPL=0 (0x00)
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
    }

    pub fn set_user_handler(&mut self, handler: u64, cs: u16) {
        self.set_handler(handler, cs);
        self.type_attr |= 0x60; // Set DPL=3
    }

    pub fn set_ist(&mut self, ist: u8) {
        self.ist = ist;
    }
}

/// The Interrupt Descriptor Table containing 256 entries.
#[repr(C)]
#[repr(align(16))]
pub struct Idt {
    pub entries: [IdtEntry; 256],
}

impl Idt {
    pub const fn new() -> Self {
        Self {
            entries: [IdtEntry::missing(); 256],
        }
    }
}

/// The structure passed to the `lidt` instruction.
#[repr(C, packed)]
pub struct IdtPtr {
    limit: u16,
    base: u64,
}

impl Idt {
    /// Loads this IDT into the CPU.
    pub unsafe fn load(&'static self) {
        let ptr = IdtPtr {
            limit: (core::mem::size_of::<Self>() - 1) as u16,
            base: self as *const _ as u64,
        };

        // Load IDT
        unsafe {
            asm!("lidt [{}]", in(reg) &ptr, options(readonly, nostack, preserves_flags));
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct InterruptStackFrame {
    pub instruction_pointer: u64,
    pub code_segment: u64,
    pub cpu_flags: u64,
    pub stack_pointer: u64,
    pub stack_segment: u64,
}
