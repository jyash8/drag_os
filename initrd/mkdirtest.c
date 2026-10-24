// Probe: which syscall does glibc's mkdir() actually issue here, and what does
// the kernel answer? Written because zellij dies with ENOSYS (38) inside
// fs::create_dir, and the failing syscall number was not visible from the
// census (it only reports the top 12, and mkdir/mkdirat did not appear).
//
// Prints raw syscall results rather than going through libc, so the number and
// the errno are both unambiguous.
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <sys/syscall.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>
#include <fcntl.h>

static long raw6(long n, long a, long b, long c, long d, long e, long f)
{
    long ret;
    register long r10 __asm__("r10") = d;
    register long r8  __asm__("r8")  = e;
    register long r9  __asm__("r9")  = f;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(n), "D"(a), "S"(b), "d"(c), "r"(r10), "r"(r8), "r"(r9)
                     : "rcx", "r11", "memory");
    return ret;
}

int main(void)
{
    const char *dir = "/tmp/mkprobe";
    const char *sub = "/tmp/mkprobe/inner";

    printf("--- raw syscall probe ---\n");

    // 83 = mkdir
    long r = raw6(83, (long)dir, 0755, 0, 0, 0, 0);
    printf("mkdir(\"%s\")      = %ld errno=%d (%s)\n", dir, r, r < 0 ? errno : 0,
           r < 0 ? strerror(errno) : "ok");

    // 258 = mkdirat with AT_FDCWD (-100)
    r = raw6(258, -100, (long)dir, 0755, 0, 0, 0);
    printf("mkdirat(AT_FDCWD) = %ld errno=%d (%s)\n", r, r < 0 ? errno : 0,
           r < 0 ? strerror(errno) : "ok");

    // Does the directory exist now?
    struct stat st;
    int s = stat(dir, &st);
    printf("stat(\"%s\")    = %d", dir, s);
    if (s == 0)
        printf(" mode=0o%o isdir=%d", st.st_mode & 07777, S_ISDIR(st.st_mode));
    else
        printf(" errno=%d (%s)", errno, strerror(errno));
    printf("\n");

    // Nested create: the parent must exist for the child.
    r = raw6(258, -100, (long)sub, 0755, 0, 0, 0);
    printf("mkdirat(nested)  = %ld errno=%d (%s)\n", r, r < 0 ? errno : 0,
           r < 0 ? strerror(errno) : "ok");
    s = stat(sub, &st);
    printf("stat(nested)     = %d", s);
    if (s == 0)
        printf(" isdir=%d", S_ISDIR(st.st_mode));
    else
        printf(" errno=%d (%s)", errno, strerror(errno));
    printf("\n");

    // What libc's own mkdir does, which is what zellij actually calls.
    errno = 0;
    r = mkdir("/tmp/mkprobe_libc", 0755);
    printf("libc mkdir()     = %ld errno=%d (%s)\n", r, errno, strerror(errno));

    // zellij's atomic_create_dir() is create_dir() followed by set_permissions(),
    // and its set_permissions() is fs::metadata() then fs::set_permissions().
    // mkdir alone is not the whole path, so walk all three the way zellij does.
    // This is where the ENOSYS actually came from: the first probe showed
    // mkdir and mkdirat both succeed, so the failure was in a later step.
    printf("--- zellij's atomic_create_dir path ---\n");

    mkdir("/tmp/zjprobe", 0755);
    errno = 0;
    if (stat("/tmp/zjprobe", &st) == 0) {
        printf("fs::metadata     = ok mode=0o%o\n", st.st_mode & 07777);
    } else {
        printf("fs::metadata     = FAIL errno=%d (%s)\n", errno, strerror(errno));
    }

    // fs::set_permissions -> chmod(2) = 90, or fchmodat(2) = 268.
    errno = 0;
    r = chmod("/tmp/zjprobe", 0700);
    printf("chmod(90)        = %ld errno=%d (%s)\n", r, errno, strerror(errno));
    s = stat("/tmp/zjprobe", &st);
    printf("  mode after     = 0%o\n", s == 0 ? (st.st_mode & 07777) : 0);

    // 268 = fchmodat, which glibc's chmod() may route through on some builds.
    errno = 0;
    r = raw6(268, -100, (long)"/tmp/zjprobe", 0755, 0, 0, 0);
    printf("fchmodat(268)    = %ld errno=%d (%s)\n", r, errno, strerror(errno));
    s = stat("/tmp/zjprobe", &st);
    printf("  mode after     = 0%o\n", s == 0 ? (st.st_mode & 07777) : 0);

    // The next thing atomic_create_dir does is create a file in the dir.
    errno = 0;
    int fd = open("/tmp/zjprobe/zellij.log", O_CREAT | O_WRONLY, 0600);
    printf("open(O_CREAT)    = %d errno=%d (%s)\n", fd, errno, strerror(errno));
    if (fd >= 0)
        close(fd);

    printf("MKDIRTEST_DONE\n");
    return 0;
}
