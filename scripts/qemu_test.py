import os
import pexpect
import sys
import time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))

QEMU_CMD = (
    "qemu-system-x86_64 -enable-kvm -smp 2 -m 2G "
    "-audiodev none,id=snd0 -machine pcspk-audiodev=snd0 "
    f"-drive if=pflash,format=raw,readonly=on,file={ROOT}/OVMF_CODE.fd "
    f"-drive if=pflash,format=raw,readonly=on,file={ROOT}/OVMF_VARS.fd "
    f"-drive format=raw,file=fat:rw:{ROOT}/esp "
    f"-drive file={ROOT}/disk.img,format=raw,if=none,id=vd0 "
    "-device virtio-blk-pci,drive=vd0 -device edu "
    f"-debugcon file:{ROOT}/uefi_debug.log -global isa-debugcon.iobase=0x402 "
    "-serial mon:stdio -device virtio-vga,xres=1280,yres=800 "
    "-no-reboot -display none -nographic"
)

TEST_CASES = {
    "fork_test": {
        "command": "/fork_test",
        "expected": ["Child exec'd hello!"],
        "timeout": 20,
    },
    "clone_test": {
        "command": "/clone_test",
        "expected": ["C"],
        "timeout": 20,
    },
    "sig_test": {
        "command": "/sig_test",
        "expected": ["sig_test: pid=", "raising SIGTERM"],
        "timeout": 20,
    },
    "test_musl": {
        "command": "/test_musl",
        "expected": ["Starting pipe and fork test...", "Hello from child via pipe!", "Test completed successfully."],
        "timeout": 25,
    },
    "wait4_success": {
        "command": "/bin/sh -c '(/fork_test) && echo WAIT4_OK'",
        "expected": ["WAIT4_OK"],
        "timeout": 25,
    },
    "zombie_reaping": {
        "command": "/bin/sh -c 'for i in 1 2 3; do /hello; done; echo REAP_ALL_OK'",
        "expected": ["REAP_ALL_OK"],
        "timeout": 30,
    },
    "fork_sleep_wait": {
        "command": "/bin/sh -c '(/hello &) && /hello && echo FORK_SLEEP_WAIT_OK'",
        "expected": ["FORK_SLEEP_WAIT_OK"],
        "timeout": 25,
    }
}

def run_single_test(test_name, config, run_idx=1, verbose=True):
    cmd = config["command"]
    expected_patterns = config["expected"]
    timeout = config.get("timeout", 30)

    print(f"\n=======================================================")
    print(f" Running Test [{test_name}] (Run {run_idx})")
    print(f" Command: {cmd}")
    print(f"=======================================================")

    logfile = sys.stdout if verbose else None
    child = pexpect.spawn(QEMU_CMD, encoding='utf-8', logfile=logfile, timeout=120)
    start_time = time.time()
    try:
        child.expect(['/ $', '/ #'], timeout=60)
        print(f"\n[SHELL READY in {time.time() - start_time:.2f}s] Sending command...")
        child.sendline(cmd)
        
        for pat in expected_patterns:
            child.expect(pat, timeout=timeout)
            print(f"\n[MATCHED] '{pat.strip()}'")

        # Verify shell prompt returns cleanly after command completes
        child.sendline("echo __TEST_DONE__")
        child.expect("__TEST_DONE__", timeout=15)
        child.expect(['/ $', '/ #'], timeout=10)

        elapsed = time.time() - start_time
        print(f"\n[PASS] Test '{test_name}' passed cleanly in {elapsed:.2f}s (Run {run_idx})")
        child.close(force=True)
        return True, ""
    except Exception as e:
        elapsed = time.time() - start_time
        print(f"\n[FAIL] Test '{test_name}' failed after {elapsed:.2f}s: {e}")
        child.close(force=True)
        # Capture debug log excerpt if available
        debug_excerpt = ""
        debug_path = os.path.join(ROOT, "uefi_debug.log")
        if os.path.exists(debug_path):
            with open(debug_path, "r", errors="ignore") as f:
                lines = f.readlines()
                debug_excerpt = "".join(lines[-30:])
        return False, debug_excerpt

def main():
    if len(sys.argv) > 1:
        target = sys.argv[1]
    else:
        target = "all"

    if target == "all":
        tests_to_run = list(TEST_CASES.keys())
        runs = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    elif target in TEST_CASES:
        tests_to_run = [target]
        runs = int(sys.argv[2]) if len(sys.argv) > 2 else 1
    else:
        # Ad-hoc command: sys.argv[1] is command, sys.argv[2] is expected output
        custom_name = "custom_cmd"
        expected_str = sys.argv[2] if len(sys.argv) > 2 else "/ #"
        TEST_CASES[custom_name] = {
            "command": target,
            "expected": [expected_str],
            "timeout": 30,
        }
        tests_to_run = [custom_name]
        runs = 1

    results = {}
    for tname in tests_to_run:
        config = TEST_CASES[tname]
        run_results = []
        for r in range(1, runs + 1):
            passed, excerpt = run_single_test(tname, config, run_idx=r, verbose=True)
            run_results.append((passed, excerpt))
            if not passed:
                print(f"\n--- Debug Log Excerpt for {tname} (Run {r}) ---")
                print(excerpt)
        results[tname] = run_results

    print("\n=======================================================")
    print(" SUMMARY OF TEST RESULTS")
    print("=======================================================")
    all_passed = True
    for tname, res in results.items():
        pass_count = sum(1 for p, _ in res if p)
        total_count = len(res)
        status = "PASS" if pass_count == total_count else "FAIL"
        print(f" - {tname:20s}: {status} ({pass_count}/{total_count} runs succeeded)")
        if pass_count != total_count:
            all_passed = False

    sys.exit(0 if all_passed else 1)

if __name__ == "__main__":
    main()

