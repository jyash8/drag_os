// ptyrun: run an interactive program on a fresh pty slave, pumping the
// master to our own stdio.
//
// Why: bash-as-PID1 dies instantly on bare TtyDevice fds (no termios owner,
// no winsize, no pgrp, no line discipline). On a real pty slave it gets
// isatty + termios + winsize + echo/canon + ISIG delivery and behaves.
// Usage: ptyrun prog [args...]  (typically: ptyrun /bin/sh)
//
// Layout: parent pumps master->stdout; one child pumps stdin->master; the
// shell child has slave dup'd to 0/1/2. On master EOF (shell gone) the
// parent reaps the shell, kills the forwarder, and exits with its status.
// Limitation: if stdin hits EOF first, the forwarder exits unnoticed and
// the shell keeps running input-less until it exits (keyboard stdin never
// EOFs, so this only matters for piped stdin).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

// Copy fd -> fd until EOF/error. Single direction; returns when done.
static void pump(int from, int to) {
    char buf[512];
    for (;;) {
        ssize_t n = read(from, buf, sizeof buf);
        if (n <= 0)
            return;
        ssize_t off = 0;
        while (off < n) {
            ssize_t w = write(to, buf + off, (size_t)(n - off));
            if (w <= 0)
                return;
            off += w;
        }
    }
}

int main(int argc, char **argv) {
    if (argc < 2) {
        dprintf(2, "usage: ptyrun prog [args...]\n");
        return 2;
    }
    int m = open("/dev/ptmx", O_RDWR);
    if (m < 0) {
        perror("ptyrun: /dev/ptmx");
        return 1;
    }
    if (unlockpt(m) != 0) {
        perror("ptyrun: unlockpt");
        return 1;
    }
    char *slave = ptsname(m);
    if (!slave) {
        perror("ptyrun: ptsname");
        return 1;
    }
    // Open the slave once, in the parent, and keep it across the fork so the
    // child inherits a live reference. Do NOT open-and-close a throwaway
    // "probe" fd: that drops slave_count to zero, clears slave_open, and the
    // parent's first read of the master can race into that window and see a
    // spurious EOF, silently ending the output pump (the shell keeps running
    // but nothing reaches our stdout). The parent closes its copy only after
    // the fork; the child's inherited fd holds the slave side open until the
    // shell exits, which is when the master read should report EOF.
    int s = open(slave, O_RDWR);
    if (s < 0) {
        perror("ptyrun: slave");
        return 1;
    }

    pid_t shell = fork();
    if (shell < 0) {
        perror("ptyrun: fork");
        return 1;
    }
    if (shell == 0) {
        setsid();
        ioctl(s, TIOCSCTTY, 0);
        pid_t me = getpid();
        ioctl(s, TIOCSPGRP, &me);
        dup2(s, 0);
        dup2(s, 1);
        dup2(s, 2);
        if (s > 2)
            close(s);
        close(m);
        execvp(argv[1], &argv[1]);
        perror("ptyrun: exec");
        _exit(127);
    }
    // Parent no longer needs its slave reference; the child has its own.
    if (s > 2)
        close(s);

    pid_t fwd = fork();
    if (fwd < 0) {
        perror("ptyrun: fork");
        return 1;
    }
    if (fwd == 0) {
        pump(0, m);
        _exit(0);
    }

    pump(m, 1);
    // Master EOF: shell's slave fds are all closed. Reap, stop forwarder.
    int st = 0;
    while (waitpid(shell, &st, 0) < 0 && errno == EINTR) {
    }
    kill(fwd, SIGKILL);
    while (waitpid(fwd, NULL, 0) < 0 && errno == EINTR) {
    }
    if (WIFEXITED(st))
        return WEXITSTATUS(st);
    return 0;
}
