use core::{cell::UnsafeCell, mem::MaybeUninit, ops::Deref};

use crate::arch::sync::once::Once;

pub struct Lazy<T, F = fn() -> T> {
    once: Once,
    data: UnsafeCell<MaybeUninit<T>>,
    init: F,
}

unsafe impl<T: Send + Sync, F: Send> Sync for Lazy<T, F> {}
unsafe impl<T: Send, F: Send> Send for Lazy<T, F> {}

impl<T, F: Fn() -> T> Lazy<T, F> {
    pub const fn new(init: F) -> Self {
        Self {
            once: Once::new(),
            data: UnsafeCell::new(MaybeUninit::uninit()),
            init,
        }
    }

    pub fn force(&self) -> &T {
        self.once.call_once(|| unsafe {
            (*self.data.get()).write((self.init)());
        });

        unsafe { (*self.data.get()).assume_init_ref() }
    }
}

impl<T, F: Fn() -> T> Deref for Lazy<T, F> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.force()
    }
}
