#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(kernel::test_runner::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use shared::boot::boot_info::BootInfo;

#[cfg(test)]
#[unsafe(no_mangle)]
pub extern "sysv64" fn kernel_main(boot_info: *const BootInfo) -> ! {
    kernel::init(boot_info);
    test_main();
    kernel::test_runner::exit_qemu(kernel::test_runner::QemuExitCode::Success);
}

#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "sysv64" fn kernel_main(boot_info: *const BootInfo) -> ! {
    kernel::init(boot_info);
    // ponytail: quiet boot - single line, no duplicate println (serial is slow)
    kernel::tprintln!(
        "DragOS SMP:{}",
        kernel::smp::CPU_COUNT.load(core::sync::atomic::Ordering::Relaxed)
    );

    // Mount the Ext2 root filesystem
    let mounted = kernel::fs::mount_root();
    kernel::fs::mount::register_pseudo_mounts();

    // Always register the initrd so we can free it after a successful mount.
    let boot_info = unsafe { &*boot_info };
    if let Some(initrd) = boot_info.initrd {
        kernel::fs::set_initrd(boot_info.hhdm_info.base.0, initrd.base.0, initrd.size);
    }

    if mounted {
        // Root filesystem mounted — the tar initrd is no longer reachable
        // (tar::find_vnode prefers ROOT_NODE and only falls back to BLK_DEV
        // / INITRD). Free the bootloader-allocated pages to reclaim memory.
        kernel::fs::clear_initrd();
        kernel::tprintln!("ROOT mounted");
    } else {
        kernel::tprintln!("WARNING: ext2/ext4 mount failed; using TAR initrd fallback");
    }

    // Module ELF validator (docs/module-system.md §8.1 phase 2).
    //
    // Runs here rather than next to the VT/TTY self-checks in kernel::init
    // because those run before any filesystem exists: `fs::lookup` there can
    // only fail, so the check would silently report SKIPPED forever. This is
    // the first point where a staged file is actually readable.
    //
    // It parses a real relocatable object built by the real toolchain and
    // verifies the section addresses land inside the window that the BADRIP
    // check in tasks.rs accepts — those two constants live in different files,
    // so a divergence would stay silent until a module thread is preempted.
    //
    // 8 is measured, not guessed: readelf -S reports 9 sections carrying
    // SHF_ALLOC, but `.text` itself is zero-sized and the parser skips it (a
    // zero-length mapping would alias the next section's address).
    match kernel::module::read_whole("/modules/fixture.o") {
        Ok(bytes) => {
            // Both checks must pass for the marker to be printed: the marker
            // means "the module code works", not "the parse worked".
            let mut ok = true;
            // The slot allocator (§4.1), first: it is the only thing here
            // that *reserves* anything, so it must run before anything that
            // would expect the window to be untouched.
            match kernel::module::selftest::check_slots() {
                Ok(()) => kernel::dprintln!("[module] slot selftest OK"),
                Err(e) => {
                    kernel::dprintln!("[module] slot selftest FAILED: {e}");
                    ok = false;
                }
            }
            match kernel::module::selftest::check_module(&bytes, 8) {
                Ok(()) => kernel::dprintln!("[module] selftest OK"),
                Err(e) => {
                    kernel::dprintln!("[module] selftest FAILED: {e}");
                    ok = false;
                }
            }
            // The module-facing API surface, so a module that fails to resolve
            // can be diagnosed: §6.2 requires the unresolved *name* to be
            // reported, which only helps if the available set is known too.
            kernel::module::exports::log_exports();
            // Relocator (docs/module-system.md §8.1 phase 6). Separate from the
            // parse check because the census measures *which* relocation types
            // exist while this measures whether they are computed correctly --
            // a relocator can handle every type and still be wrong by 4 bytes on
            // every call (§6.3), which no type census would catch.
            match kernel::module::selftest::check_relocator(&bytes) {
                Ok(()) => kernel::dprintln!("[module] relocator selftest OK"),
                Err(e) => {
                    kernel::dprintln!("[module] relocator selftest FAILED: {e}");
                    ok = false;
                }
            }
            // The loader (docs/module-system.md §8.1 phases 3-9) and the
            // round trip: the kernel calls into the module, and the module
            // calls back into the kernel.
            match kernel::module::selftest::check_load(&bytes) {
                Ok(()) => kernel::dprintln!("[module] load selftest OK"),
                Err(e) => {
                    kernel::dprintln!("[module] load selftest FAILED: {e}");
                    ok = false;
                }
            }
            // A module that binds to *another module's* export (§6.2's second
            // half). It has to be loaded after the good fixture, which is
            // above, so the ordering is the test.
            match kernel::module::read_whole("/modules/consumer.o") {
                Ok(consumer) => {
                    match kernel::module::selftest::check_cross_module(&consumer) {
                        Ok(()) => kernel::dprintln!("[module] cross-module selftest OK"),
                        Err(e) => {
                            kernel::dprintln!("[module] cross-module selftest FAILED: {e}");
                            ok = false;
                        }
                    }
                }
                Err(e) => {
                    kernel::dprintln!("[module] cross-module selftest SKIPPED: {e}");
                }
            }
            // A module that asks for a symbol the kernel does not export must
            // be refused cleanly, with nothing left behind (§6.2). It is a
            // separate file because the good fixture is, by construction,
            // resolvable.
            match kernel::module::read_whole("/modules/unresolvable.o") {
                Ok(bad) => {
                    match kernel::module::selftest::check_load_refused(&bad) {
                        Ok(()) => kernel::dprintln!("[module] refusal selftest OK"),
                        Err(e) => {
                            kernel::dprintln!("[module] refusal selftest FAILED: {e}");
                            ok = false;
                        }
                    }
                }
                Err(e) => {
                    kernel::dprintln!("[module] refusal selftest SKIPPED: {e}");
                }
            }
            // On the serial as well as the debugcon, so the test suite can
            // assert it: the suite reads the serial, and a self-check that
            // only writes to the debugcon file on the host cannot fail CI.
            if ok {
                kernel::println!("{}", kernel::module::selftest::SELFTEST_MARKER);
            } else {
                kernel::println!("{}", kernel::module::selftest::SELFTEST_FAIL_MARKER);
            }
        }
        Err(e) => {
            kernel::dprintln!("[module] selftest SKIPPED: {e}");
            kernel::println!("{}: skipped: {e}", kernel::module::selftest::SELFTEST_FAIL_MARKER);
        }
    }

    // Devfs/procfs pseudo-resolution + mount-table sanity (boot-time quick check).
    // Stale AGENTS note claimed mount was not dispatched; it is now (165 =>)
    // and basic nodes (/proc, /proc/self, /dev/null) must resolve.
    let mount_ok = kernel::fs::lookup("proc").is_some()
        && kernel::fs::lookup("proc/self").is_some()
        && kernel::fs::lookup("dev/null").is_some()
        && kernel::fs::lookup("dev/zero").is_some();
    if mount_ok {
        kernel::println!("DEVFS_SELFTEST_OK");
    } else {
        kernel::println!("DEVFS_SELFTEST_FAIL");
    }

    // Woke the application processors (SMP) so they can pick up runnable
    // threads from the shared scheduler alongside the BSP.
    let aps = kernel::smp::boot_aps();
    kernel::tprintln!("SMP APs:{}", aps);

    unsafe {
        kernel::arch::x86_64::apic::init_lapic();
    }
    kernel::arch::x86_64::apic::lapic_timer_start(
        kernel::interrupts::TIMER_INTERRUPT_ID,
        kernel::arch::x86_64::apic::lapic_period_us(),
    );

    // Hand control to init: Rust draginit first (daemons via /etc/draginit),
    // falling back to the legacy init.sh if draginit is not staged.
    // (exec_initrd_args returns on lookup failure, so the fallback runs.)
    kernel::exec::exec_initrd_args("/bin/draginit", &["/bin/draginit"], 0, 0);
    kernel::exec::exec_initrd_args("/bin/sh", &["/bin/sh", "/bin/init.sh"], 0, 0);

    loop {
        kernel::arch::x86_64::cpu::hlt();
    }
}
