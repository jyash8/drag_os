/* Bypass glibc's ioctl() wrapper entirely and issue the syscall directly.
 *
 * glibc's ioctl() has a per-code table; codes it does not recognise are
 * rejected client-side with ENOTTY and never reach the kernel. That is why the
 * probe's TIOCSCTTY produced errno 25 with no corresponding [ioctl] trace:
 * the syscall was never made. Issuing it directly tests the kernel, which is
 * the thing under test. */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <sys/ioctl.h>

static long raw_ioctl(int fd, unsigned long req, void *arg)
{
    return syscall(SYS_ioctl, fd, req, arg);
}

int main(void)
{
    int tty = open("/dev/tty", O_RDWR);
    if (tty < 0) { printf("open /dev/tty: %s\n", strerror(errno)); return 1; }
    printf("open(/dev/tty) fd=%d\n", tty);

    errno = 0;
    long r = raw_ioctl(tty, 0x540E, (void *)(long)tty);   /* TIOCSCTTY */
    printf("raw TIOCSCTTY -> %ld errno=%d (%s)\n", r, r < 0 ? errno : 0,
           r < 0 ? strerror(errno) : "ok");

    /* Compare with the libc wrapper on the same fd. */
    errno = 0;
    r = ioctl(tty, 0x540E, (void *)(long)tty);
    printf("libc TIOCSCTTY -> %ld errno=%d (%s)\n", r, r < 0 ? errno : 0,
           r < 0 ? strerror(errno) : "ok");
    return 0;
}
