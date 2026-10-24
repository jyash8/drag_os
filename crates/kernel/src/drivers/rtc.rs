use crate::arch::x86_64::port::Port;

const CMOS_ADDR_PORT: u16 = 0x70;
const CMOS_DATA_PORT: u16 = 0x71;

fn read_register(reg: u8) -> u8 {
    let mut addr = Port::<u8>::new(CMOS_ADDR_PORT);
    let data = Port::<u8>::new(CMOS_DATA_PORT);

    unsafe {
        // Write the register number (with NMI disable bit set)
        addr.write(reg | 0x80);

        // Read the result
        let result = data.read();
        // Re-enable NMIs
        addr.write(0x00);

        result
    }
}

#[derive(Debug)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
}

fn bcd_to_binary(bcd: u8) -> u8 {
    (bcd & 0x0F) + ((bcd / 16) * 10)
}

fn update_in_progress() -> bool {
    (read_register(0x0A) & 0x80) != 0
}

pub fn read_datetime() -> DateTime {
    while update_in_progress() {
        core::hint::spin_loop();
    }

    let seconds = read_register(0x00);
    let minutes = read_register(0x02);
    let hours = read_register(0x04);
    let day = read_register(0x07);
    let month = read_register(0x08);
    let year = read_register(0x09);

    DateTime {
        seconds: bcd_to_binary(seconds),
        minutes: bcd_to_binary(minutes),
        hours: bcd_to_binary(hours & 0x7F), // Strip the AM/PM bit
        day: bcd_to_binary(day),
        month: bcd_to_binary(month),
        year: bcd_to_binary(year) as u16 + 2000,
    }
}
