import pexpect
import sys

child = pexpect.spawn(
    './scripts/qemu.sh -display none',
    encoding='utf-8',
    logfile=sys.stdout,
)
try:
    child.expect('/ #', timeout=60)
    child.sendline('/bin/ls')
    child.expect(['hello.txt', 'KERNEL PANIC', 'Error'], timeout=15)
    child.sendline('/bin/ls /')
    child.expect(['/bin', 'KERNEL PANIC', 'Error'], timeout=15)
    child.sendline('exit')
    child.close(force=True)
    print("LS TEST PASSED")
except Exception as e:
    print(f"LS TEST FAILED: {e}")
    child.close(force=True)
    sys.exit(1)
