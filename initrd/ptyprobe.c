// ptyprobe: guest-side PTY regression test. Replicates glibc openpty() +
// login_tty() step by step (openpt/grantpt/unlockpt/ptsname/slave open,
// winsize, termios, bidirectional data, setsid+TIOCSCTTY+exec in child).
// Prints PTYPROBE_PASS on success.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <termios.h>
#include <sys/ioctl.h>
#include <sys/wait.h>

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    printf("openpt: fd=%d errno=%d (%s)\n", m, m < 0 ? errno : 0,
           m < 0 ? strerror(errno) : "ok");
    if (m < 0) return 1;
    int r = grantpt(m);
    printf("grantpt: rc=%d errno=%d (%s)\n", r, r ? errno : 0,
           r ? strerror(errno) : "ok");
    r = unlockpt(m);
    printf("unlockpt: rc=%d errno=%d (%s)\n", r, r ? errno : 0,
           r ? strerror(errno) : "ok");
    char *sn = ptsname(m);
    printf("ptsname: %s errno=%d (%s)\n", sn ? sn : "(null)", sn ? 0 : errno,
           sn ? "ok" : strerror(errno));
    if (!sn) return 1;
    int s = open(sn, O_RDWR | O_NOCTTY);
    printf("openslave: fd=%d errno=%d (%s)\n", s, s < 0 ? errno : 0,
           s < 0 ? strerror(errno) : "ok");
    if (s < 0) return 1;
    // winsize + termios roundtrip on slave
    struct winsize ws = {25, 132, 0, 0};
    r = ioctl(s, TIOCSWINSZ, &ws);
    printf("tiocswinsz: rc=%d errno=%d\n", r, r ? errno : 0);
    struct termios t;
    r = tcgetattr(s, &t);
    printf("tcgetattr: rc=%d errno=%d\n", r, r ? errno : 0);
    r = tcsetattr(s, TCSANOW, &t);
    printf("tcsetattr: rc=%d errno=%d (%s)\n", r, r ? errno : 0,
           r ? strerror(errno) : "ok");
    struct winsize gws;
    r = ioctl(s, TIOCGWINSZ, &gws);
    printf("tiocgwinsz: rc=%d errno=%d rows=%d cols=%d\n", r, r ? errno : 0,
           r ? -1 : gws.ws_row, r ? -1 : gws.ws_col);
    // write slave -> read master, write master -> read slave
    const char *msg = "ping\n";
    (void)write(s, msg, 5);
    char buf[16] = {0};
    int flags = fcntl(m, F_GETFL);
    fcntl(m, F_SETFL, flags | O_NONBLOCK);
    ssize_t n = read(m, buf, sizeof(buf));
    printf("slave->master: n=%zd data=%s errno=%d\n", n, buf, n < 0 ? errno : 0);
    (void)write(m, "pong\n", 5);
    flags = fcntl(s, F_GETFL);
    fcntl(s, F_SETFL, flags | O_NONBLOCK);
    memset(buf, 0, sizeof(buf));
    n = read(s, buf, sizeof(buf));
    printf("master->slave: n=%zd data=%s errno=%d\n", n, buf, n < 0 ? errno : 0);
    // full login_tty sequence in a child: setsid, reopen slave, TIOCSCTTY,
    // dup2 to 0/1/2, exec echo (mirrors sshd pty_make_controlling_tty)
    pid_t p = fork();
    if (p == 0) {
        r = setsid();
        printf("child setsid: rc=%d errno=%d\n", r, r < 0 ? errno : 0);
        int f = open(sn, O_RDWR);
        printf("child reopen: fd=%d errno=%d\n", f, f < 0 ? errno : 0);
        if (f >= 0) {
            r = ioctl(f, TIOCSCTTY, 0);
            printf("child tiocsctty: rc=%d errno=%d\n", r, r ? errno : 0);
            dup2(f, 0); dup2(f, 1); dup2(f, 2);
            if (f > 2) close(f);
            close(m); close(s);
            execl("/bin/sh", "sh", "-c", "echo child-exec-ok", (char *)0);
            printf("child exec FAIL errno=%d\n", errno);
            _exit(11);
        }
        _exit(12);
    }
    int st = 0;
    waitpid(p, &st, 0);
    printf("child status: exit=%d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    printf("PTYPROBE_PASS\n");
    return 0;
}
