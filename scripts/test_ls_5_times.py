#!/usr/bin/env python3
import pexpect
import sys
import time
import os

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

QEMU_CMD = (
    "/usr/bin/qemu-system-x86_64 "
    "-enable-kvm -smp 1 -m 2G "
    "-audiodev none,id=snd0 -machine pcspk-audiodev=snd0 "
    f"-drive if=pflash,format=raw,readonly=on,file={ROOT}/OVMF_CODE.fd "
    f"-drive if=pflash,format=raw,readonly=on,file={ROOT}/OVMF_VARS.fd "
    f"-drive format=raw,file=fat:rw:{ROOT}/esp "
    f"-drive file={ROOT}/disk.img,format=raw,if=none,id=vd0 "
    "-device virtio-blk-pci,drive=vd0 "
    "-device edu "
    f"-debugcon file:{ROOT}/uefi_debug.log -global isa-debugcon.iobase=0x402 "
    "-serial mon:stdio -device virtio-vga,xres=1280,yres=800 "
    "-no-reboot -display none -nographic"
)

def test_ls_5_times():
    print("=" * 60)
    print(" Running Test: ls 5 times in the same shell")
    print("=" * 60)

    child = pexpect.spawn(QEMU_CMD, encoding='utf-8', logfile=sys.stdout, timeout=120)
    start_time = time.time()
    try:
        child.expect('/ #', timeout=60)
        print(f"\n[SHELL READY in {time.time() - start_time:.2f}s]")

        for i in range(1, 6):
            print(f"\n>>> [RUN {i}/5] Sending 'ls'...")
            child.sendline("ls")
            # Expect directory entry 'bin' or 'doom'
            child.expect("bin", timeout=20)
            # Expect shell prompt to return
            child.expect('/ #', timeout=20)
            print(f"\n>>> [RUN {i}/5] Completed successfully!")

        print("\n" + "=" * 60)
        print(" SUCCESS: Successfully ran 'ls' 5 times inside the same shell!")
        print("=" * 60)
        child.close(force=True)
        return True
    except Exception as e:
        print(f"\n[FAIL] Test failed: {e}")
        child.close(force=True)
        return False

if __name__ == "__main__":
    success = test_ls_5_times()
    sys.exit(0 if success else 1)
