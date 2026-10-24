use acpi::{AcpiTables, PhysicalMapping, Handler as AcpiHandler};
use acpi::sdt::fadt::Fadt;
use core::ptr::NonNull;
use shared::boot::boot_info::HHDM_BASE;
use crate::arch::x86_64::port::Port;
use aml::{AmlContext, AmlName, Handler as AmlHandler, DebugVerbosity};

extern crate alloc;

#[derive(Clone)]
pub struct DragAcpiHandler;

impl AcpiHandler for DragAcpiHandler {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let virtual_address = physical_address + HHDM_BASE.0 as usize;
        PhysicalMapping {
            physical_start: physical_address,
            virtual_start: NonNull::new(virtual_address as *mut T).unwrap(),
            region_length: size,
            mapped_length: size,
            handler: self.clone(),
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}

    fn read_u8(&self, _: usize) -> u8 { 0 }
    fn read_u16(&self, _: usize) -> u16 { 0 }
    fn read_u32(&self, _: usize) -> u32 { 0 }
    fn read_u64(&self, _: usize) -> u64 { 0 }
    fn write_u8(&self, _: usize, _: u8) {}
    fn write_u16(&self, _: usize, _: u16) {}
    fn write_u32(&self, _: usize, _: u32) {}
    fn write_u64(&self, _: usize, _: u64) {}
    fn read_io_u8(&self, _: u16) -> u8 { 0 }
    fn read_io_u16(&self, _: u16) -> u16 { 0 }
    fn read_io_u32(&self, _: u16) -> u32 { 0 }
    fn write_io_u8(&self, _: u16, _: u8) {}
    fn write_io_u16(&self, _: u16, _: u16) {}
    fn write_io_u32(&self, _: u16, _: u32) {}
    fn read_pci_u8(&self, _: acpi::PciAddress, _: u16) -> u8 { 0 }
    fn read_pci_u16(&self, _: acpi::PciAddress, _: u16) -> u16 { 0 }
    fn read_pci_u32(&self, _: acpi::PciAddress, _: u16) -> u32 { 0 }
    fn write_pci_u8(&self, _: acpi::PciAddress, _: u16, _: u8) {}
    fn write_pci_u16(&self, _: acpi::PciAddress, _: u16, _: u16) {}
    fn write_pci_u32(&self, _: acpi::PciAddress, _: u16, _: u32) {}
    fn nanos_since_boot(&self) -> u64 { 0 }
    fn stall(&self, _: u64) {}
    fn sleep(&self, _: u64) {}
    fn create_mutex(&self) -> acpi::Handle { todo!() }
    fn acquire(&self, _: acpi::Handle, _: u16) -> Result<(), acpi::aml::AmlError> { Ok(()) }
    fn release(&self, _: acpi::Handle) {}
}

impl AmlHandler for DragAcpiHandler {
    fn read_u8(&self, _: usize) -> u8 { 0 }
    fn read_u16(&self, _: usize) -> u16 { 0 }
    fn read_u32(&self, _: usize) -> u32 { 0 }
    fn read_u64(&self, _: usize) -> u64 { 0 }
    fn write_u8(&mut self, _: usize, _: u8) {}
    fn write_u16(&mut self, _: usize, _: u16) {}
    fn write_u32(&mut self, _: usize, _: u32) {}
    fn write_u64(&mut self, _: usize, _: u64) {}
    fn read_io_u8(&self, _: u16) -> u8 { 0 }
    fn read_io_u16(&self, _: u16) -> u16 { 0 }
    fn read_io_u32(&self, _: u16) -> u32 { 0 }
    fn write_io_u8(&self, _: u16, _: u8) {}
    fn write_io_u16(&self, _: u16, _: u16) {}
    fn write_io_u32(&self, _: u16, _: u32) {}
    fn read_pci_u8(&self, _: u16, _: u8, _: u8, _: u8, _: u16) -> u8 { 0 }
    fn read_pci_u16(&self, _: u16, _: u8, _: u8, _: u8, _: u16) -> u16 { 0 }
    fn read_pci_u32(&self, _: u16, _: u8, _: u8, _: u8, _: u16) -> u32 { 0 }
    fn write_pci_u8(&self, _: u16, _: u8, _: u8, _: u8, _: u16, _: u8) {}
    fn write_pci_u16(&self, _: u16, _: u8, _: u8, _: u8, _: u16, _: u16) {}
    fn write_pci_u32(&self, _: u16, _: u8, _: u8, _: u8, _: u16, _: u32) {}
}

pub static mut ACPI_TABLES: Option<AcpiTables<DragAcpiHandler>> = None;

pub fn init(rsdp: usize) {
    let handler = DragAcpiHandler;
    if let Ok(tables) = unsafe { AcpiTables::from_rsdp(handler, rsdp) } {
        unsafe {
            ACPI_TABLES = Some(tables);
        }
    }
}

pub fn poweroff() -> ! {
    crate::println!("Initiating system poweroff...");
    if let Some(tables) = unsafe { &*core::ptr::addr_of!(ACPI_TABLES) } {
        if let Some(fadt_mapping) = tables.find_table::<Fadt>() {
            if let Ok(fadt_pm1a_cnt_blk) = fadt_mapping.pm1a_control_block() {
                if let Ok(dsdt_mapping) = tables.dsdt() {
                    let mut aml_context = AmlContext::new(
                        alloc::boxed::Box::new(DragAcpiHandler),
                        DebugVerbosity::None,
                    );

                    let dsdt_data = unsafe {
                        core::slice::from_raw_parts(
                            (dsdt_mapping.phys_address
                                + shared::boot::boot_info::HHDM_BASE.0 as usize)
                                as *const u8,
                            dsdt_mapping.length as usize,
                        )
                    };

                    if aml_context.parse_table(dsdt_data).is_ok() {
                        if let Ok(aml::AmlValue::Package(s5)) = aml_context
                            .namespace
                            .get_by_path(&AmlName::from_str("\\_S5").unwrap())
                        {
                            if let Some(aml::AmlValue::Integer(slp_typa)) = s5.get(0) {
                                let pm1a_port = fadt_pm1a_cnt_blk.address as u16;
                                let val = ((slp_typa & 7) << 10) | (1 << 13);
                                unsafe {
                                    let mut port: Port<u16> = Port::new(pm1a_port);
                                    port.write(val as u16);
                                }
                            }
                            if let Ok(Some(fadt_pm1b_cnt_blk)) = fadt_mapping.pm1b_control_block() {
                                if let Some(aml::AmlValue::Integer(slp_typb)) = s5.get(1) {
                                    let pm1b_port = fadt_pm1b_cnt_blk.address as u16;
                                    let val = ((slp_typb & 7) << 10) | (1 << 13);
                                    unsafe {
                                        let mut port: Port<u16> = Port::new(pm1b_port);
                                        port.write(val as u16);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallbacks for virtualized environments & standard ACPI ports
    unsafe {
        // QEMU / Bochs standard ACPI PM1a port
        Port::<u16>::new(0x604).write(0x2000);
        Port::<u16>::new(0xB004).write(0x2000);
        // VirtualBox poweroff port
        Port::<u16>::new(0x4004).write(0x3400);
        // QEMU debug exit
        Port::<u8>::new(0x501).write(0x31);
    }

    // CPU halt fallback
    unsafe {
        crate::arch::x86_64::cpu::cli();
        loop {
            core::arch::asm!("hlt");
        }
    }
}

pub fn reboot() -> ! {
    crate::println!("Initiating system reboot...");
    // 1. ACPI reset register from FADT
    if let Some(tables) = unsafe { &*core::ptr::addr_of!(ACPI_TABLES) } {
        if let Some(fadt_mapping) = tables.find_table::<Fadt>() {
            if let Ok(reset_reg) = fadt_mapping.reset_register() {
                let reset_port = reset_reg.address as u16;
                let val = fadt_mapping.reset_value;
                unsafe {
                    let mut port: Port<u8> = Port::new(reset_port);
                    port.write(val);
                }
            }
        }
    }

    // 2. 8042 Keyboard Controller pulse reset
    unsafe {
        for _ in 0..10000 {
            if (Port::<u8>::new(0x64).read() & 0x02) == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        Port::<u8>::new(0x64).write(0xFE);
    }

    // 3. PCI system reset (Port 0xCF9)
    unsafe {
        Port::<u8>::new(0xCF9).write(0x02);
        Port::<u8>::new(0xCF9).write(0x06);
    }

    // 4. Triple fault via zeroed IDT
    unsafe {
        #[repr(C, packed)]
        struct Idtr {
            limit: u16,
            base: u64,
        }
        let null_idtr = Idtr { limit: 0, base: 0 };
        core::arch::asm!("lidt [{}]", in(reg) &null_idtr, options(nostack, preserves_flags));
        core::arch::asm!("int3");
    }

    // 5. CPU halt fallback
    unsafe {
        crate::arch::x86_64::cpu::cli();
        loop {
            core::arch::asm!("hlt");
        }
    }
}
