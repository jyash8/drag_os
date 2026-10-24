import pexpect
import sys

print("Starting QEMU for DRM scanout (flip_test) verification...")
child = pexpect.spawn('./scripts/qemu.sh -display none', encoding='utf-8',
                      logfile=sys.stdout, timeout=90)
try:
    child.expect('/ #', timeout=90)
    print("\n--- Shell ready. Executing /flip_test ---")
    child.sendline("/flip_test")
    child.expect("Page flip test PASSED!", timeout=30)
    print("\n[SUCCESS] /flip_test verified full DRM scanout + page flip!")
    child.sendline("poweroff")
    child.expect('poweroff', timeout=15)
    child.close()
except Exception as e:
    print(f"\n[FAILURE] flip_test error: {e}")
    child.close(force=True)
    sys.exit(1)
