// Probe the POSIX pty API — the path glibc's openpty(3) actually takes.
//
// Written because zellij stalled at "session initialized, spawning tabs" with
// no panes and no error, while `ptyrun` worked fine. `ptyrun` opens
// /dev/ptmx directly; openpty goes through syscall 622 (posix_openpt), which was
// not dispatched at all. Nothing in the kernel log showed a failed open because
// the call never reached an open().
//
// Every step is done through the libc wrappers on purpose, so this tests the
// same route a real program takes rather than a hand-rolled approximation.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>
#include <pty.h>

int main(void)
{
    int master = -1, slave = -1;
    char name[128];

    // The POSIX route: posix_openpt -> grantpt -> unlockpt -> ptsname -> open.
    errno = 0;
    master = posix_openpt(O_RDWR | O_NOCTTY);
    printf("posix_openpt()  = %d errno=%d (%s)\n", master, errno, strerror(errno));
    if (master < 0) {
        printf("POSIX_PTY_FAILED at openpt\n");
        return 1;
    }

    errno = 0;
    int rc = grantpt(master);
    printf("grantpt()       = %d errno=%d (%s)\n", rc, errno, strerror(errno));

    errno = 0;
    rc = unlockpt(master);
    printf("unlockpt()      = %d errno=%d (%s)\n", rc, errno, strerror(errno));

    errno = 0;
    rc = ptsname_r(master, name, sizeof(name));
    // ptsname_r() returns 0 on success on glibc and writes the path into the
    // buffer. Its *return value* is not the length -- reading it as one is what
    // made this probe report a failure that was not there. The first version
    // of this file did exactly that, and blamed the kernel for it.
    printf("ptsname_r()     = %d name='%s' errno=%d (%s)\n", rc,
           name[0] ? name : "(empty)", errno, strerror(errno));
    if (name[0] == '\0') {
        printf("POSIX_PTY_FAILED at ptsname\n");
        return 1;
    }
    // Cross-check against the plain ptsname(3), which uses a static buffer and
    // ioctl(TIOCGPTN) rather than the syscall.
    const char *plain = ptsname(master);
    printf("ptsname()       = '%s'\n", plain ? plain : "(null)");
    if (plain && strcmp(plain, name) != 0)
        printf("  NOTE: ptsname_r='%s' ptsname='%s' differ\n", name, plain);

    errno = 0;
    slave = open(name, O_RDWR | O_NOCTTY);
    printf("open(slave)     = %d errno=%d (%s)\n", slave, errno, strerror(errno));

    // The real test: can a process on the slave be talked to?
    if (slave >= 0) {
        const char *msg = "posix-pty-roundtrip\n";
        if (write(master, msg, strlen(msg)) == (ssize_t)strlen(msg)) {
            char buf[64] = {0};
            ssize_t n = read(slave, buf, sizeof(buf) - 1);
            printf("master->slave   = %zd '%s'\n", n, n > 0 ? buf : "");
        } else {
            printf("master write failed: %s\n", strerror(errno));
        }
    }

    // And the legacy route, for comparison.
    int l_master = posix_openpt(O_RDWR | O_NOCTTY);
    printf("\nlegacy compare: second posix_openpt = %d\n", l_master);

    if (slave >= 0)
        close(slave);
    if (master >= 0)
        close(master);
    if (l_master >= 0)
        close(l_master);

    printf("POSIX_PTY_DONE\n");
    return slave >= 0 ? 0 : 1;
}
