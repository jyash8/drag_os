import pexpect
import sys

def boot_test(cmd, expect, timeout=60):
    child = pexpect.spawn('./scripts/qemu.sh -display none', encoding='utf-8',
                          logfile=sys.stdout)
    try:
        child.expect('/ #', timeout=timeout)
        child.sendline(cmd)
        child.expect(expect, timeout=timeout)
        child.sendline('exit')
        child.close(force=True)
        print(f"PASS: {cmd} -> {expect}")
    except Exception as e:
        print(f"FAIL: {cmd} -> {expect}: {e}")
        child.close(force=True)
        sys.exit(1)

if __name__ == '__main__':
    boot_test(sys.argv[1], sys.argv[2])
