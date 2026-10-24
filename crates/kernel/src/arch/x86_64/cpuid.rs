use core::arch::asm;

use alloc::{string::String, vec::Vec};

pub struct CpuidResult {
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
}

#[inline(always)]
pub fn cpuid(leaf: u32) -> CpuidResult {
    let mut eax: u32;
    let mut ecx: u32;
    let mut edx: u32;

    let mut ebx_tmp: u64;

    unsafe {
        asm!(
            "mov {tmp}, rbx",
            "cpuid",
            "xchg {tmp}, rbx",
            inout("eax") leaf => eax,
            inout("ecx") 0 => ecx,
            out("edx") edx,

            tmp = out(reg) ebx_tmp,
            options(nostack, preserves_flags)
        );
    }

    CpuidResult {
        eax,
        ebx: ebx_tmp as u32,
        ecx,
        edx,
    }
}

pub fn get_processor_brand() -> Option<String> {
    let max_ext_leaf = cpuid(0x80000000).eax;

    if max_ext_leaf < 0x80000004 {
        return None;
    }

    let mut bytes = Vec::with_capacity(48);

    for leaf in 0x80000002..=0x80000004 {
        let res = cpuid(leaf);

        bytes.extend_from_slice(&res.eax.to_le_bytes());
        bytes.extend_from_slice(&res.ebx.to_le_bytes());
        bytes.extend_from_slice(&res.ecx.to_le_bytes());
        bytes.extend_from_slice(&res.edx.to_le_bytes());
    }

    let mut string_bytes = Vec::new();
    for &b in &bytes {
        if b == 0 {
            break;
        }
        string_bytes.push(b);
    }
    String::from_utf8(string_bytes).ok()
}
