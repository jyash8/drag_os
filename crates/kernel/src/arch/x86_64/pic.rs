use crate::arch::{
    sync::spinlock::SpinLock,
    x86_64::io::{inb, outb},
};

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const PIC_EOI: u8 = 0x20; // End of interrupt command

pub struct ChainedPics {
    offset1: u8,
    offset2: u8,
}

impl ChainedPics {
    pub const fn new(offset1: u8, offset2: u8) -> Self {
        Self { offset1, offset2 }
    }

    pub unsafe fn init(&mut self) {
        let wait = || unsafe { outb(0x80, 0) };

        unsafe {
            // Save current masks (in case UEFI set them)
            let a1 = inb(PIC1_DATA);
            let a2 = inb(PIC2_DATA);

            // ICW1: Start initialization sequence in cascade mode
            outb(PIC1_CMD, 0x11);
            wait();
            outb(PIC2_CMD, 0x11);
            wait();

            // ICW2: Set the vector offsets
            outb(PIC1_DATA, self.offset1);
            wait();
            outb(PIC2_DATA, self.offset2);
            wait();

            // ICW3: Tell Master PIC about Slave PIC at IRQ2 (0000 0100)
            outb(PIC1_DATA, 4);
            wait();
            // Tell Slave PIC its cascade identity (0000 0010)
            outb(PIC2_DATA, 2);
            wait();

            // ICW4: Set 8086/88 (x86) mode
            outb(PIC1_DATA, 0x01);
            wait();
            outb(PIC2_DATA, 0x01);
            wait();

            // Restore masks (we will unmask specific IRQs later)
            outb(PIC1_DATA, a1);
            outb(PIC2_DATA, a2);
        }
    }

    pub unsafe fn notify_end_of_interrupt(&mut self, interrupt_id: u8) {
        if self.handles_interrupt(interrupt_id) {
            if interrupt_id >= self.offset2 {
                unsafe {
                    outb(PIC2_CMD, PIC_EOI);
                }
            }
            unsafe {
                outb(PIC1_CMD, PIC_EOI);
            }
        }
    }

    fn handles_interrupt(&self, interrupt_id: u8) -> bool {
        interrupt_id >= self.offset1 && interrupt_id < self.offset1 + 16
    }

    /// Unmasks (enables) a specific IRQ line.
    pub unsafe fn unmask(&mut self, irq: u8) {
        let port = if irq < 8 { PIC1_DATA } else { PIC2_DATA };
        let bit = irq % 8;
        let value = unsafe { inb(port) } & !(1 << bit);
        unsafe {
            outb(port, value);
        }
    }

    /// Masks all IRQs and disables the 8259 PIC cascade.
    pub unsafe fn disable(&mut self) {
        unsafe {
            outb(PIC1_DATA, 0xFF);
            outb(PIC2_DATA, 0xFF);
        }
    }
}

pub static PICS: SpinLock<ChainedPics> = SpinLock::new(ChainedPics::new(32, 40));
