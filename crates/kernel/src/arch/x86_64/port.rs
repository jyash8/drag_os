use core::{arch::asm, marker::PhantomData};

pub trait PortIO {
    unsafe fn read(port: u16) -> Self;
    unsafe fn write(port: u16, value: Self);
}

impl PortIO for u8 {
    #[inline(always)]
    unsafe fn read(port: u16) -> Self {
        let value: u8;
        unsafe {
            asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    #[inline(always)]
    unsafe fn write(port: u16, value: Self) {
        unsafe {
            asm!("out dx, al", in("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
    }
}

impl PortIO for u16 {
    #[inline(always)]
    unsafe fn read(port: u16) -> Self {
        let value: u16;
        unsafe {
            asm!("in ax, dx", out("ax") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    #[inline(always)]
    unsafe fn write(port: u16, value: Self) {
        unsafe {
            asm!("out dx, ax", in("ax") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
    }
}

impl PortIO for u32 {
    #[inline(always)]
    unsafe fn read(port: u16) -> Self {
        let value: u32;
        unsafe {
            asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
        value
    }

    #[inline(always)]
    unsafe fn write(port: u16, value: Self) {
        unsafe {
            asm!("out dx, eax", in("eax") value, in("dx") port, options(nomem, nostack, preserves_flags));
        }
    }
}

pub struct Port<T: PortIO> {
    port: u16,
    phantom: PhantomData<T>,
}

impl<T: PortIO> Port<T> {
    pub const fn new(port: u16) -> Self {
        Self {
            port,
            phantom: PhantomData,
        }
    }

    #[inline(always)]
    pub unsafe fn read(&self) -> T {
        unsafe { T::read(self.port) }
    }

    #[inline(always)]
    pub unsafe fn write(&mut self, value: T) {
        unsafe { T::write(self.port, value) }
    }
}
