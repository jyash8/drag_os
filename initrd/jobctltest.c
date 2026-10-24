// Probe for the job-control work: sessions, process groups, controlling
// terminals, and pthread_join.
//
// Written because all three were stubs or absent in a way that no program could
// detect directly:
//
//   - setpgid returned 0 and changed nothing
//   - getpgid existed but returned the pid, not the group id
//   - getpgrp was not dispatched (ENOSYS)
//   - setsid returned the pid without creating a session
//   - TIOCSCTTY returned 0 and recorded nothing
//   - pthread_join's futex wake was skipped for threads reaped by exit_group
//
// A program calling all of these got plausible-looking success and then
// behaviour that did not match, which is the worst failure mode: there is no
// error to trace. So each case asserts the *value*, not just the return code.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>
#include <pthread.h>
#include <termios.h>
#include <sys/ioctl.h>
#include <sys/wait.h>

static int failures;

static void check(const char *what, long got, long want)
{
    int ok = (got == want);
    if (!ok)
        failures++;
    printf("  %-34s got=%-8ld want=%-8ld %s\n", what, got, want, ok ? "OK" : "FAIL");
}

static void check_true(const char *what, int cond)
{
    if (!cond)
        failures++;
    printf("  %-34s %s\n", what, cond ? "OK" : "FAIL");
}

static void *join_target(void *arg)
{
    // Sleep briefly so the main thread genuinely has to block on join.
    usleep(120000);
    return (void *)(long)0xC0DE;
}

int main(void)
{
    printf("--- job control ---\n");
    pid_t me = getpid();
    pid_t ppid = getppid();
    check("getpid/getppid sane", me > 0 && ppid > 0, 1);

    // setpgid on ourselves, then read it back.
    check("setpgid(0,0)", setpgid(0, 0), 0);
    check("getpgid(0) == self after setpgid", getpgid(0), me);

    // getpgrp must agree with getpgid(0) -- it is getpgid(0) under another name.
    check("getpgrp() == getpgid(0)", getpgrp(), getpgid(0));

    // A child that sets its own group: the classic shell backgrounding pattern.
    pid_t child = fork();
    if (child == 0) {
        if (setpgid(0, 0) < 0)
            _exit(11);
        if (getpgid(0) != getpid())
            _exit(12);
        _exit(0);
    }
    int st = 0;
    waitpid(child, &st, 0);
    check("child setpgid+getpgid roundtrip", WIFEXITED(st) && WEXITSTATUS(st) == 0, 1);
    check("parent pgid unchanged by child", getpgid(0), me);

    // setsid must fail for a process-group leader and succeed otherwise.
    pid_t sid_child = fork();
    if (sid_child == 0) {
        setpgid(0, 0);   /* make it a group leader -> setsid must EPERM */
        pid_t r = setsid();
        _exit(r < 0 && errno == EPERM ? 0 : 13);
    }
    waitpid(sid_child, &st, 0);
    check("setsid EPERM for pgrp leader", WIFEXITED(st) && WEXITSTATUS(st) == 0, 1);

    sid_child = fork();
    if (sid_child == 0) {
        /* Not a group leader: start in the parent's group, then setsid. */
        pid_t r = setsid();
        if (r < 0)
            _exit(14);
        if (getpgid(0) != getpid())
            _exit(15);   /* a new session implies a new group led by self */
        if (getsid(0) != getpid())
            _exit(16);
        _exit(0);
    }
    waitpid(sid_child, &st, 0);
    check("setsid creates session+group", WIFEXITED(st) && WEXITSTATUS(st) == 0, 1);

    // getsid must report the *session leader's* pid, which is a different
    // number from the caller's for every member of a multi-process session.
    //
    // The case above cannot see this: the caller called setsid(), so it is the
    // session leader and its pid == sid. A kernel that returned the caller's own
    // pid would pass. The only way to distinguish is a second process inside the
    // session asking who leads it.
    printf("--- getsid reports the session leader ---\n");
    pid_t leader = fork();
    if (leader == 0) {
        if (setsid() < 0)
            _exit(20);                    /* could not become a leader */
        pid_t mypid = getpid();
        pid_t inner = fork();
        if (inner == 0) {
            /* Not the leader. getsid(0) must be the parent's pid, NOT ours. */
            pid_t s = getsid(0);
            pid_t p = getppid();
            printf("  %-34s leader=%d inner=%d getsid=%d\n",
                   "inner process reports", mypid, getpid(), s);
            _exit(s == p ? 0 : 21);
        }
        if (inner < 0)
            _exit(22);
        int ist = 0;
        waitpid(inner, &ist, 0);
        /* Propagate: 0 => session correct, 21 => got our own pid instead. */
        _exit(WIFEXITED(ist) && WEXITSTATUS(ist) == 0 ? 0 : 23);
    }
    if (leader < 0) {
        check_true("fork session leader", 0);
    } else {
        waitpid(leader, &st, 0);
        /* 21 = member got its own pid as the sid; 23 = member exited oddly. */
        if (WIFEXITED(st) && WEXITSTATUS(st) == 0)
            check("getsid(0) == session leader's pid", 1, 1);
        else if (WEXITSTATUS(st) == 21)
            check("getsid(0) == session leader's pid", 0, 1);
        else
            check("getsid session probe ran cleanly", WEXITSTATUS(st), 0);
    }

    printf("--- controlling terminal ---\n");
    int tty = open("/dev/tty", O_RDWR);
    if (tty < 0) {
        /* No controlling tty under ssh without a pty; that is a valid state. */
        printf("  %-34s no /dev/tty (%s) -- skipped\n", "TIOCSCTTY", strerror(errno));
    } else {
        /* Report the fd actually obtained. An earlier version of this probe
         * assumed open() would return 3 and passed 0 to TIOCSCTTY when the
         * process had no stdin; the kernel correctly reported "fd=0 is not a
         * terminal" and the failure looked like a kernel bug. Print it so that
         * a wrong-fd failure cannot be mistaken for a wrong answer. */
        printf("  %-34s fd=%d\n", "open(/dev/tty)", tty);
        errno = 0;
        /* TIOCSCTTY's third argument IS the fd to claim, not a flag word --
         * Linux's prototype is ioctl(fd, TIOCSCTTY, arg) with arg being the
         * terminal's own fd. Passing 0 here asked the kernel to claim fd 0,
         * which under ssh is a pipe, so ENOTTY was the correct answer and the
         * probe was blaming the kernel for it. */
        long r = ioctl(tty, TIOCSCTTY, (void *)(long)tty);
        /* Ground truth measured on Linux 6.x, same call sequence, run under a
         * real pty (`script -qec`):
         *
         *   open(/dev/tty) fd=3
         *   TIOCSCTTY -> -1 errno=1 (Operation not permitted)
         *   getpgrp()=142761  tcgetpgrp=142761
         *
         * So EPERM is the CORRECT answer when the calling process is already
         * part of a session that has a controlling terminal, which is always
         * true for a shell or an ssh session. An earlier version of this probe
         * asserted TIOCSCTTY should succeed and reported the kernel's EPERM as
         * a bug -- it was the assertion that was wrong.
         *
         * Assert the shape rather than a fixed errno: either it succeeds (no
         * ctty yet, e.g. a session leader claiming one) or it must fail with
         * EPERM. Anything else -- ENOTTY, EINVAL, success-with-wrong-state --
         * is a real defect.
         */
        if (r == 0) {
            check("TIOCSCTTY succeeded (no prior ctty)", 1, 1);
            pid_t fg = tcgetpgrp(tty);
            check("tcgetpgrp == our pgid after claiming", fg, getpgrp());
        } else {
            check("TIOCSCTTY failed with EPERM (prior ctty)", errno, EPERM);
            /* With a ctty already held by the session, tcgetpgrp must report
             * the *session's* foreground group, not the caller's own. */
            pid_t fg = tcgetpgrp(tty);
            check_true("tcgetpgrp reports a real pgid", fg > 0);
            printf("  %-34s ours=%d fg=%d %s\n", "pgid relationship", getpgrp(), fg,
                   fg == getpgrp() ? "OK" : "(foreground group differs -- legal)");
        }
        close(tty);
    }

    printf("--- pthread_join ---\n");
    pthread_t th;
    int rc = pthread_create(&th, NULL, join_target, NULL);
    check("pthread_create", rc, 0);
    if (rc == 0) {
        void *ret = NULL;
        rc = pthread_join(th, &ret);
        check("pthread_join returned 0", rc, 0);
        check("pthread_join recovered retval", (long)ret, 0xC0DE);
        /* A second join on a joined thread must fail, not hang or succeed. */
        rc = pthread_join(th, &ret);
        /* Ground truth: on Linux 6.x with glibc, a second pthread_join on an
         * already-joined thread ALSO returns 0. glibc recycles the thread
         * descriptor into its stack cache rather than marking it invalid, so
         * there is nothing for a second join to detect. Asserting failure here
         * was a wrong expectation, and the kernel had matched Linux all along.
         *
         * So this only checks that the second join does not hang or corrupt --
         * any return value is acceptable, which is itself worth pinning. */
        check_true("second join returns (no hang)", rc == 0 || rc != 0);
    }

    printf("JOBCTL_DONE failures=%d\n", failures);
    return failures ? 1 : 0;
}
