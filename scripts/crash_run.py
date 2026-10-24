import pexpect
import sys

child = pexpect.spawn('./scripts/qemu.sh -display none', encoding='utf-8',
                      logfile=sys.stdout)
try:
    child.expect('/ #', timeout=60)
    child.sendline('/doom')
    child.expect("Drag OS DOOM backend starting...", timeout=15)
    try:
        child.expect("KERNEL PANIC", timeout=60)
    except pexpect.exceptions.TIMEOUT:
        print("NO PANIC WITHIN 60s")
    child.close(force=True)
except Exception as e:
    print(f"Error: {e}")
    child.close(force=True)
    sys.exit(1)
