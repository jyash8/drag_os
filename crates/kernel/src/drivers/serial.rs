use crate::arch::x86_64::port::Port;
use core::fmt;

pub const COM1: u16 = 0x3F8;

pub struct SerialPort {
    data: Port<u8>,
    int_en: Port<u8>,
    fifo_ctrl: Port<u8>,
    line_ctrl: Port<u8>,
    modem_ctrl: Port<u8>,
    line_status: Port<u8>,
}

impl SerialPort {
    pub const fn new(base: u16) -> Self {
        Self {
            data: Port::new(base),
            int_en: Port::new(base + 1),
            fifo_ctrl: Port::new(base + 2),
            line_ctrl: Port::new(base + 3),
            modem_ctrl: Port::new(base + 4),
            line_status: Port::new(base + 5),
        }
    }

    pub unsafe fn init(&mut self) {
        unsafe {
            self.int_en.write(0x00); //     Disable all interrupts
            self.line_ctrl.write(0x80); //  Enable DLAB (set baud rate divisor)
            self.data.write(0x03); //       Set divisor to 3 (lo byte) 38400 baud
            self.int_en.write(0x00); //                      (hi byte)
            self.line_ctrl.write(0x03); //  8 bits, no parity, one stop bit
            // Trigger a receive interrupt for every byte. A 14-byte threshold is
            // suitable for bulk serial traffic, but leaves interactive input
            // buffered until enough keys have been pressed.
            self.fifo_ctrl.write(0x07); // Enable FIFO, clear them, 1-byte threshold
            self.modem_ctrl.write(0x0B); // IRQs enabled, RTS/DSR set
            self.int_en.write(0x01); // Enable Data Available interrupt
        }
    }

    fn is_transmit_empty(&self) -> bool {
        unsafe { self.line_status.read() & 0x20 != 0 }
    }

    pub fn write_byte(&mut self, b: u8) {
        while !self.is_transmit_empty() {
            core::hint::spin_loop();
        }
        unsafe {
            self.data.write(b);
        }
    }
}

impl fmt::Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // QEMU serial console expects \r\n for newlines
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}
