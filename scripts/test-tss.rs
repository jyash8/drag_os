#[derive(Debug, Clone, Copy)]
#[repr(C, packed(4))]
pub struct TaskStateSegment {
    pub reserved_1: u32,
    pub previlege_stack_table: [u64; 3],
    pub reserved_2: u64,
    pub interrupt_stack_table: [u64; 7],
    pub reserved_3: u64,
    pub reserved_4: u16,
    pub iomap_base: u16,
}

fn main() {
    println!("reserved_1: {}", std::mem::offset_of!(TaskStateSegment, reserved_1));
    println!("previlege_stack_table: {}", std::mem::offset_of!(TaskStateSegment, previlege_stack_table));
    println!("reserved_2: {}", std::mem::offset_of!(TaskStateSegment, reserved_2));
    println!("interrupt_stack_table: {}", std::mem::offset_of!(TaskStateSegment, interrupt_stack_table));
    println!("reserved_3: {}", std::mem::offset_of!(TaskStateSegment, reserved_3));
    println!("reserved_4: {}", std::mem::offset_of!(TaskStateSegment, reserved_4));
    println!("iomap_base: {}", std::mem::offset_of!(TaskStateSegment, iomap_base));
    println!("size: {}", std::mem::size_of::<TaskStateSegment>());
}
