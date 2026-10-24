use goblin::{elf::Elf, elf64::program_header::PT_LOAD};

use crate::tprintln;

pub mod loader;
pub mod stack;

pub fn inspect_elf(file_data: &[u8]) -> Option<u64> {
    let elf = match Elf::parse(file_data) {
        Ok(elf) => elf,
        Err(e) => {
            tprintln!("Error parsing ELF: {:?}", e);
            return None;
        }
    };

    tprintln!("ELF Entry Point: {:#X}", elf.entry);
    tprintln!("Scanning Program Headers...");

    for phdr in elf.program_headers {
        if phdr.p_type == PT_LOAD {
            tprintln!(
                "  [LOAD] vaddr: {:#X}, file_size: {}B, mem_size: {}B",
                phdr.p_vaddr,
                phdr.p_filesz,
                phdr.p_memsz
            );
        }
    }

    Some(elf.entry)
}
