ENTRY(kernel_main)

SECTIONS
{
    . = 0xffffffff80000000;

    .text ALIGN(4K) :
    {
        *(.text*)
    }

    /* AGENTS.md §16.3 / BADRIP. The dispatch path panics on a kernel-mode RIP
     * outside .text, so the bound has to track .text exactly. A hand-written
     * Rust constant silently rotted: .text grew from 0x1425a4 to 0x1653cf when
     * the module loader landed, overran the constant by ~21 KiB, and every
     * thread preempted in the new code panicked with
     *   BADRIP[enter]: tid=10 would iretq to data 0xffffffff80165186
     * -- an address that was valid kernel text.
     *
     * These two symbols make the bound impossible to get wrong: the linker knows
     * the real extents, so nothing has to be updated by hand. They are defined
     * here rather than parsed from the ELF at runtime because the ELF headers
     * sit at file offset 0, *below* the first PT_LOAD (offset 0x1000), and are
     * therefore never mapped into memory.
     */
    __kernel_text_start = ADDR(.text);
    __kernel_text_end = ADDR(.text) + SIZEOF(.text);

    .rodata ALIGN(4K) :
    {
        *(.rodata*)
    }

    .data ALIGN(4K) :
    {
        *(.data*)
    }

    .bss ALIGN(4K) :
    {
        *(.bss*)
        *(COMMON)
    }
}
