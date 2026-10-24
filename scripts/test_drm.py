import pexpect
import sys

print("Starting QEMU for DRM/KMS verification...")
child = pexpect.spawn('./scripts/qemu.sh -display none', encoding='utf-8')
child.logfile = sys.stdout

try:
    child.expect('/ #', timeout=40)
    print("\n--- Shell ready. Executing /drm_test ---")
    child.sendline("/drm_test")
    child.expect("All DRM & KMS operations PASSED!", timeout=15)
    print("\n[SUCCESS] /drm_test executed and verified successfully!")
    child.sendline("poweroff")
    child.close()
except Exception as e:
    print(f"\n[FAILURE] DRM test encountered error: {e}")
    child.close()
    sys.exit(1)
