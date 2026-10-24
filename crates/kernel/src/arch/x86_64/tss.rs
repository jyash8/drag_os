use crate::arch::sync::lazy::Lazy;

#[derive(Debug, Clone, Copy)]
#[repr(C, packed(4))]
pub struct TaskStateSegment {
    pub reserved_1: u32,
    pub previlege_stack_table: [u64; 3],
    pub reserved_2: u64,
    pub interrupt_stack_table: [u64; 7], // 7 available IST stacks
    pub reserved_3: u64,
    pub reserved_4: u16,
    pub iomap_base: u16,
}

impl TaskStateSegment {
    pub const fn new() -> Self {
        Self {
            reserved_1: 0,
            previlege_stack_table: [0; 3],
            reserved_2: 0,
            interrupt_stack_table: [0; 7],
            reserved_3: 0,
            reserved_4: 0,
            // The size of the TSS tells the CPU there is no I/O map
            iomap_base: core::mem::size_of::<Self>() as u16,
        }
    }
}

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

static mut DOUBLE_FAULT_STACK: [u8; 16384] = [0; 16384];
static mut PREVILEGE_STACK: [u8; 16384] = [0; 16384];

pub static TSS: Lazy<TaskStateSegment> = Lazy::new(|| {
    let mut tss = TaskStateSegment::new();

    let df_stack_start = core::ptr::addr_of_mut!(DOUBLE_FAULT_STACK) as u64;
    let df_stack_end = df_stack_start + 16384;

    // Double fault handler runs on its own IST stack.
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = df_stack_end;

    // RSP0: kernel stack used for every interrupt/exception delivered
    // from ring 3. Without this, a user-mode fault/interrupt pushes its
    // frame at address 0 and escalates to a double fault.
    let priv_stack_start = core::ptr::addr_of_mut!(PREVILEGE_STACK) as u64;
    let priv_stack_end = priv_stack_start + 16384;

    tss.previlege_stack_table[0] = priv_stack_end;

    tss
});

pub fn set_rsp0(tss: &mut TaskStateSegment, top: u64) {
    unsafe {
        let ptr = (tss as *mut TaskStateSegment as *mut u8).add(4) as *mut u64;
        core::ptr::write_volatile(ptr, top);
    }
}

pub fn get_rsp0(tss: &TaskStateSegment) -> u64 {
    unsafe {
        let ptr = (tss as *const TaskStateSegment as *const u8).add(4) as *const u64;
        core::ptr::read_volatile(ptr)
    }
}
