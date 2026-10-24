use core::{
    hint::spin_loop,
    sync::atomic::{AtomicU8, Ordering},
};

const INCOMPLETE: u8 = 0;
const RUNNING: u8 = 1;
const COMPLETE: u8 = 2;

pub struct Once {
    state: AtomicU8,
}

impl Once {
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(INCOMPLETE),
        }
    }

    #[inline]
    pub fn is_complete(&self) -> bool {
        self.state.load(Ordering::Acquire) == COMPLETE
    }

    pub fn call_once<F>(&self, f: F)
    where
        F: FnOnce(),
    {
        if self.is_complete() {
            return;
        }

        let exchange_result =
            self.state
                .compare_exchange(INCOMPLETE, RUNNING, Ordering::Acquire, Ordering::Relaxed);

        match exchange_result {
            Ok(_) => {
                f();
                self.state.store(COMPLETE, Ordering::Release);
            }
            Err(mut current_state) => {
                while current_state != COMPLETE {
                    spin_loop();
                    current_state = self.state.load(Ordering::Acquire);
                }
            }
        }
    }
}
