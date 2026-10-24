use crate::arch::x86_64::port::Port;

const PIT_CHANNEL_0: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;

pub fn init(frequency: u32) {
    let mut command_port = Port::new(PIT_COMMAND);
    let mut data_port = Port::new(PIT_CHANNEL_0);

    // Calculate the divisor. The PIT base frequency is ~1.193 MHz
    let divisor = 1193182 / frequency;

    unsafe {
        // Command 0x36 means: Channel 0, expects two writes (lo-byte then hi-byte),
        // and operates in Mode 3 (Square Wave Generator).
        command_port.write(0x36_u8);

        // Low byte
        data_port.write((divisor & 0xFF) as u8);
        // High byte
        data_port.write(((divisor >> 8) & 0xFF) as u8);
    }
}
