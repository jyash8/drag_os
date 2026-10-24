import pexpect
import sys

child = pexpect.spawn(
    './scripts/qemu.sh -display none',
    encoding='utf-8',
    logfile=sys.stdout,
)
try:
    child.expect('/ #', timeout=60)
    child.sendline('/songplayer /clip20.wav')
    try:
        child.expect("KERNEL PANIC", timeout=30)
        print("PANIC OCCURRED")
        child.close(force=True)
        sys.exit(1)
    except pexpect.exceptions.TIMEOUT:
        print("NO PANIC: INV1 fix holds")
    child.sendcontrol('c')
    child.expect('/ #', timeout=10)
    child.sendline('exit')
    child.close(force=True)
except Exception as e:
    print(f"Error: {e}")
    child.close(force=True)
    sys.exit(1)
