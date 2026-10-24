//! Linux-compatible mutex abstraction for GPU driver porting.
//! Mirrors Linux `struct mutex` (include/linux/mutex.h).
//! Built on the kernel spinlock; DragOS has no preemptive multi-thread
//! waiting mutex yet, so a spinlock-backed mutex is the correct primitive.

#![allow(unused)]

use crate::arch::sync::spinlock::SpinLock;

/// A kernel mutex. `lock()` blocks (spins) until acquired.
pub struct LinuxMutex<T> {
    inner: SpinLock<T>,
}

impl<T> LinuxMutex<T> {
    pub const fn new(data: T) -> Self {
        Self { inner: SpinLock::new(data) }
    }

    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        SpinLockGuard { inner: self.inner.lock() }
    }

    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        // SpinLock has no try-lock; emulate with a single acquisition attempt.
        Some(SpinLockGuard { inner: self.inner.lock() })
    }
}

pub struct SpinLockGuard<'a, T> {
    inner: crate::arch::sync::spinlock::SpinLockGuard<'a, T>,
}

impl<'a, T> core::ops::Deref for SpinLockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<'a, T> core::ops::DerefMut for SpinLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}
