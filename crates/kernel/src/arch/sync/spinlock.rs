use core::{
    cell::UnsafeCell,
    hint::spin_loop,
    ops::{Deref, DerefMut},
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering},
};

use crate::interrupts::{disable_interrupts, enable_interrupts, interrupts_enabled};

pub struct SpinLock<T> {
    is_locked: AtomicBool,
    holder_cpu: AtomicU8,
    holder_rip: AtomicU64,
    holder_line: AtomicU32,
    name: [u8; 32],
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            is_locked: AtomicBool::new(false),
            holder_cpu: AtomicU8::new(u8::MAX),
            holder_rip: AtomicU64::new(0),
            holder_line: AtomicU32::new(0),
            name: Self::make_name("unnamed"),
            data: UnsafeCell::new(data),
        }
    }

    const fn make_name(s: &str) -> [u8; 32] {
        let bytes = s.as_bytes();
        let mut buf = [0u8; 32];
        let mut i = 0;
        while i < bytes.len() && i < 31 {
            buf[i] = bytes[i];
            i += 1;
        }
        buf
    }

    pub const fn named(name: &'static str, data: T) -> Self {
        Self {
            is_locked: AtomicBool::new(false),
            holder_cpu: AtomicU8::new(u8::MAX),
            holder_rip: AtomicU64::new(0),
            holder_line: AtomicU32::new(0),
            name: Self::make_name(name),
            data: UnsafeCell::new(data),
        }
    }

    // ponytail: track_caller puts the real call-site in the reentry panic —
    // the old [rsp] RIP reads were unreliable garbage.
    #[track_caller]
    #[inline(never)]
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        let caller_loc = core::panic::Location::caller();
        let interrupts_enabled = interrupts_enabled();
        disable_interrupts();
        let mut spins: u64 = 0;
        let mut my_cpu: u8 = u8::MAX;
        let mut my_rip: u64 = 0;
        while self
            .is_locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            while self.is_locked.load(Ordering::Relaxed) {
                if my_cpu == u8::MAX {
                    my_cpu = crate::smp::current_cpu_id() as u8;
                    unsafe { core::arch::asm!("mov {}, [rsp]", out(reg) my_rip) };
                    // Same-CPU re-entry is ALWAYS a bug — fail fast with full
                    // context rather than spinning 200M times.
                    let holder = self.holder_cpu.load(Ordering::Relaxed);
                    if holder == my_cpu {
                        let mut end = 0;
                        while end < self.name.len() && self.name[end] != 0 {
                            end += 1;
                        }
                        let name_str = core::str::from_utf8(&self.name[..end]).unwrap_or("?");
                        let h_line = self.holder_line.load(Ordering::Relaxed);
                        let h_rip = self.holder_rip.load(Ordering::Relaxed);
                        let mut stack_words = [0u64; 8];
                        unsafe {
                            core::arch::asm!(
                                "mov {}, [rsp]",
                                "mov {}, [rsp + 8]",
                                "mov {}, [rsp + 16]",
                                "mov {}, [rsp + 24]",
                                "mov {}, [rsp + 32]",
                                "mov {}, [rsp + 40]",
                                "mov {}, [rsp + 48]",
                                "mov {}, [rsp + 56]",
                                out(reg) stack_words[0],
                                out(reg) stack_words[1],
                                out(reg) stack_words[2],
                                out(reg) stack_words[3],
                                out(reg) stack_words[4],
                                out(reg) stack_words[5],
                                out(reg) stack_words[6],
                                out(reg) stack_words[7],
                                options(nomem, nostack, preserves_flags)
                            );
                        }
                        panic!(
                            "SAME-CPU REENTRY on {:#x} ({}) my_cpu={} caller={}:{} holder_line={} holder_rip={:#x} stk={:#x?}",
                            self as *const _ as usize,
                            name_str,
                            my_cpu,
                            caller_loc.file(),
                            caller_loc.line(),
                            h_line,
                            h_rip,
                            stack_words,
                        );
                    }
                }
                spins += 1;
                if spins > 200_000_000 {
                    let mut end = 0;
                    while end < self.name.len() && self.name[end] != 0 {
                        end += 1;
                    }
                    let name_str = core::str::from_utf8(&self.name[..end]).unwrap_or("?");
                    panic!(
                        "SPIN DEADLOCK on {:#x} ({}) my_cpu={} my_rip={:#x} holder_cpu={} holder_rip={:#x}",
                        self as *const _ as usize,
                        name_str,
                        my_cpu,
                        my_rip,
                        self.holder_cpu.load(Ordering::Relaxed),
                        self.holder_rip.load(Ordering::Relaxed),
                    );
                }
                spin_loop();
            }
        }

        let mut caller_rip: u64 = 0;
        unsafe {
            core::arch::asm!("mov {}, [rsp]", out(reg) caller_rip, options(nomem, nostack, preserves_flags));
        }
        self.holder_cpu
            .store(crate::smp::current_cpu_id() as u8, Ordering::Relaxed);
        self.holder_rip.store(caller_rip, Ordering::Relaxed);
        self.holder_line.store(caller_loc.line(), Ordering::Relaxed);

        SpinLockGuard {
            lock: self,
            saved_interrupt_state: interrupts_enabled,
        }
    }

    pub fn is_locked(&self) -> bool {
        self.is_locked.load(Ordering::Relaxed)
    }

    pub fn is_locked_by_current_cpu(&self) -> bool {
        let cpu_id = crate::smp::current_cpu_id() as u8;
        self.is_locked.load(Ordering::Acquire) && self.holder_cpu.load(Ordering::Relaxed) == cpu_id
    }

    pub fn holder_cpu(&self) -> u8 {
        self.holder_cpu.load(Ordering::Relaxed)
    }

    pub fn holder_line(&self) -> u32 {
        self.holder_line.load(Ordering::Relaxed)
    }

    pub fn holder_rip(&self) -> u64 {
        self.holder_rip.load(Ordering::Relaxed)
    }

    #[track_caller]
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        let interrupts_were_enabled = interrupts_enabled();
        disable_interrupts();
        if self
            .is_locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // ponytail: track holder so deadlock diagnostics stay accurate;
            // previously try_lock left stale holder_cpu/holder_rip.
            let mut caller_rip: u64 = 0;
            unsafe {
                core::arch::asm!("mov {}, [rsp]", out(reg) caller_rip, options(nomem, nostack, preserves_flags));
            }
            self.holder_cpu
                .store(crate::smp::current_cpu_id() as u8, Ordering::Relaxed);
            self.holder_rip.store(caller_rip, Ordering::Relaxed);
            self.holder_line.store(core::panic::Location::caller().line(), Ordering::Relaxed);
            Some(SpinLockGuard {
                lock: self,
                saved_interrupt_state: interrupts_were_enabled,
            })
        } else {
            if interrupts_were_enabled {
                enable_interrupts();
            }
            None
        }
    }

    /// Forcibly unlocks the spinlock.
    ///
    /// # Safety
    /// This violates mutual exclusion. It should ONLY be used in a fatal
    /// panic situation where the kernel is halting and we need to bypass
    /// a deadlocked print macro.
    pub unsafe fn force_unlock(&self) {
        self.is_locked
            .store(false, core::sync::atomic::Ordering::Release);
    }
}

/// An RAII guard that manages the lock state.
/// When the guard goes out of scope, the lock is automatically released
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    saved_interrupt_state: bool,
}

impl<'a, T> Deref for SpinLockGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<'a, T> DerefMut for SpinLockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<'a, T> Drop for SpinLockGuard<'a, T> {
    fn drop(&mut self) {
        // Clear holder metadata before releasing the lock so that any check by
        // this CPU (or any other CPU once unlocked) sees holder as MAX.
        self.lock.holder_cpu.store(u8::MAX, Ordering::Relaxed);
        self.lock.holder_rip.store(0, Ordering::Relaxed);
        self.lock.holder_line.store(0, Ordering::Relaxed);
        // Release the lock. Release ordering ensures all previous memory writes
        // are visible to other cores before they see the lock as unlocked.
        self.lock.is_locked.store(false, Ordering::Release);

        if self.saved_interrupt_state {
            enable_interrupts();
        }
    }
}
