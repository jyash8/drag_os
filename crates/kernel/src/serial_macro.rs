use core::fmt::{self, Write};

use crate::{
    arch::sync::{lazy::Lazy, spinlock::SpinLock},
    drivers::serial::{COM1, SerialPort},
};

pub static COM1_PORT: Lazy<SpinLock<SerialPort>> = Lazy::new(|| {
    let mut port = SerialPort::new(COM1);
    unsafe {
        port.init();
    }
    SpinLock::new(port)
});

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let mut port = COM1_PORT.lock();
    port.write_fmt(args).unwrap();
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::serial_macro::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}
