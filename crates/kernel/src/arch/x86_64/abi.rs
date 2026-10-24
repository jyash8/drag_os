pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;
pub const SA_NOCLDSTOP: u64 = 0x0000_0001;
pub const SA_SIGINFO: u64 = 0x0000_0004;
pub const SA_RESTORER: u64 = 0x0400_0000;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;
pub const SA_RESETHAND: u64 = 0x8000_0000;

pub const SIGINT: i32 = 2;
pub const SIGILL: i32 = 4;
pub const SIGFPE: i32 = 8;
pub const SIGKILL: i32 = 9;
pub const SIGUSR1: i32 = 10;
pub const SIGSEGV: i32 = 11;
pub const SIGUSR2: i32 = 12;
pub const SIGPIPE: i32 = 13;
pub const SIGALRM: i32 = 14;
pub const SIGTERM: i32 = 15;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGTSTP: i32 = 20;
// Job control: background-group read/write of the controlling terminal.
// Kernel-generated, but a process may also block/ignore/handle them, so they
// occupy real numbers in the disposition table like any other signal.
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;
pub const SIGURG: i32 = 23;
pub const SIGWINCH: i32 = 28;
pub const SIGIO: i32 = 29;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct SigAction {
    pub sa_handler: u64,
    pub sa_flags: u64,
    pub sa_restorer: u64,
    pub sa_mask: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct StackT {
    pub ss_sp: u64,
    pub ss_flags: i32,
    pub ss_size: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SigContext {
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64, // 0..31
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64, // 32..63
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64, // 64..95
    pub rdx: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rsp: u64, // 96..127
    pub rip: u64,
    pub eflags: u64, // 128..143
    pub cs: u16,
    pub gs: u16,
    pub fs: u16,
    pub ss: u16, // 144..151
    pub err: u64,
    pub trapno: u64,
    pub oldmask: u64,
    pub cr2: u64,           // 152..183
    pub fpstate: u64,       // 184
    pub reserved: [u64; 8], // 192..255  (keeps SigContext at 256 bytes)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UContext {
    pub uc_flags: u64,           // 0
    pub uc_link: u64,            // 8
    pub uc_stack: StackT,        // 16
    pub uc_mcontext: SigContext, // 40
    pub uc_sigmask: u64,         // 296..304
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RtSigFrame {
    pub pretcode: u64,   // 0  — the restorer address; handler's `ret` lands here
    pub uc: UContext,    // 8
    pub info: [u8; 128], // 312
}

impl Default for RtSigFrame {
    fn default() -> Self {
        Self {
            pretcode: 0,
            uc: Default::default(),
            info: [0; 128],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn signal_numbers_distinct() {
        let sigs = [SIGINT, SIGILL, SIGFPE, SIGKILL, SIGUSR1, SIGSEGV, SIGUSR2,
                    SIGPIPE, SIGALRM, SIGTERM, SIGCHLD, SIGCONT, SIGSTOP, SIGTSTP,
                    SIGTTIN, SIGTTOU, SIGURG, SIGWINCH, SIGIO];
        let mut seen = alloc::vec::Vec::new();
        for s in sigs {
            assert!(!seen.contains(&s), "duplicate signal {s}");
            seen.push(s);
        }
    }

    #[test_case]
    fn sigaction_layout_repr_c() {
        use core::mem::{align_of, size_of};
        assert_eq!(size_of::<SigAction>(), 32);
        assert_eq!(align_of::<SigAction>(), 8);
    }

    #[test_case]
    fn sigcontext_layout() {
        use core::mem::{align_of, size_of};
        assert_eq!(size_of::<SigContext>(), 256, "kept at 256 bytes");
        assert_eq!(align_of::<SigContext>(), 8);
    }

    #[test_case]
    fn sigcontext_offsets() {
        use core::mem::offset_of;
        // 16 GPRs (r8..r11, r12..r15, rdi/rsi/rbp/rbx, rdx/rax/rcx/rsp) = 128 bytes,
        // then rip (128), eflags (136), 4x u16 segs (144..152), err(152), trapno(160),
        // oldmask(168), cr2(176).
        assert_eq!(offset_of!(SigContext, rip), 128);
        assert_eq!(offset_of!(SigContext, eflags), 136);
        assert_eq!(offset_of!(SigContext, cr2), 176);
        assert_eq!(offset_of!(SigContext, fpstate), 184);
    }

    #[test_case]
    fn ucontext_layout() {
        use core::mem::size_of;
        assert_eq!(size_of::<StackT>(), 24);
        // flags(8) + link(8) + stack(24) + mcontext(256) + sigmask(8)
        assert_eq!(size_of::<UContext>(), 304);
        assert_eq!(size_of::<RtSigFrame>(), 8 + 304 + 128, "pretcode + uc + info");
    }

    #[test_case]
    fn handler_dispositions() {
        assert_eq!(SIG_DFL, 0);
        assert_eq!(SIG_IGN, 1);
        assert!(SA_SIGINFO != 0 && SA_RESTORER != 0);
        // Default action for SIGKILL/SIGSTOP must be fatal/unstoppable.
        assert!(SIGKILL > 0 && SIGSTOP > SIGKILL);
    }
}

pub const RT_SIGFRAME_SIZE: u64 = core::mem::size_of::<RtSigFrame>() as u64; // 440
