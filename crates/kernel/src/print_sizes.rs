use crate::tasks::{Process, Thread};
pub fn print_sizes() {
    crate::println!("Process size: {:#x}", core::mem::size_of::<Process>());
    crate::println!("Thread size: {:#x}", core::mem::size_of::<Thread>());
    crate::println!("Canary offset: {:#x}", memoffset::offset_of!(Process, canary));
}
