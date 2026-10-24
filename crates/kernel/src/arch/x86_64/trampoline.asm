; AP trampoline: real-mode entry (SIPI vector) -> 32-bit protected -> 64-bit long.
; Copied to physical 0x8000 (TRAMPOLINE_ADDR) at boot and executed by an AP
; woken via INIT-SIPI.
;
; The BSP patches the following fixed offsets before sending SIPI:
;   TRAMPOLINE_ENTRY_OFF  - u64 address of ap_entry (kernel higher-half)
;   TRAMPOLINE_CR3_OFF    - u64 CR3 (kernel page tables, must identity-map 0x8000)
;   TRAMPOLINE_STACK_OFF  - u64 top of this AP's idle stack
;
; Layout: code, then a small GDT + its descriptor, then the 3 patch slots.
; We use absolute (flat) addressing into the trampoline page via segment base 0
; once in 32-bit mode; before that, DS is set to cover the page.

BITS 16
ORG 0x8000

_start:
    cli
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov sp, 0x8FF0        ; scratch stack below the page (below 0x9000, above 0x8000)

    ; Enable A20 (harmless if already on)
    ; (QEMU KVM leaves A20 enabled after UEFI; skip to keep it minimal.)

    ; Load the trampoline GDT (descriptor base patched at assembly to 0x8000+off)
    lgdt [gdt_desc]

    ; Switch to protected mode
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp 0x08:pm_entry

BITS 32
pm_entry:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax

    ; Enable PAE and PGE in CR4
    mov eax, cr4
    or eax, (1 << 5)   ; PAE
    or eax, (1 << 7)   ; PGE
    mov cr4, eax

    ; Load CR3 (kernel page tables, patched by BSP)
    mov eax, [cr3_slot]
    mov cr3, eax

    ; Enable long mode (EFER.LME = bit 8) and No-Execute (EFER.NXE = bit 11)
    mov ecx, 0xC0000080
    rdmsr
    or eax, (1 << 8) | (1 << 11)
    wrmsr

    ; Enable paging (PG) and leave PE set
    mov eax, cr0
    or eax, 0x80000001
    mov cr0, eax

    ; Far jump into 64-bit code (selector 0x18 = 64-bit code segment)
    jmp 0x18:lm_entry

BITS 64
lm_entry:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax

    ; Load the AP's idle stack (patched by BSP)
    mov rsp, [stack_slot]

    ; Call the kernel entry (ap_entry). Use a register-indirect jump so we
    ; don't need relocations; ap_entry is at a higher-half kernel address.
    mov rax, [entry_slot]
    call rax

    ; ap_entry never returns; hang defensively.
.hang:
    hlt
    jmp .hang

align 8
gdt:
    dq 0x0000000000000000        ; 0x00 null
    dq 0x00CF9A000000FFFF        ; 0x08 32-bit code, base 0, limit 4G
    dq 0x00CF92000000FFFF        ; 0x10 32-bit data
    dq 0x00AF9A000000FFFF        ; 0x18 64-bit code
gdt_desc:
    dw (gdt_desc - gdt - 1)      ; limit
    dd gdt                       ; base (absolute 0x8000+ within page)

align 8
entry_slot: dq 0
cr3_slot:   dq 0
stack_slot: dq 0

; Export offsets for the BSP (via symbols in an object, or just by these bytes).
; We reference the slots by their file-relative addresses: since ORG=0x8000 and
; the blob is copied to 0x8000, slot addresses == their ORG addresses.
end:
