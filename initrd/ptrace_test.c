#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <errno.h>
#include <signal.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <sys/ptrace.h>

int main(void) {
    printf("[ptrace_test] starting\n");

    pid_t pid = fork();
    if (pid < 0) { perror("fork"); return 1; }
    if (pid == 0) {
        if (ptrace(PTRACE_TRACEME, 0, 0, 0) != 0) {
            printf("FAIL TRACEME\n");
            _exit(1);
        }
        raise(SIGSTOP);
        printf("[child] after stop\n");
        _exit(42);
    }

    int status;
    pid_t r = waitpid(pid, &status, WUNTRACED);
    printf("[parent] waitpid=%d status=%d WIFS=%d WSTOPSIG=%d\n", r, status, WIFSTOPPED(status), WSTOPSIG(status));
    if (r != pid || !WIFSTOPPED(status) || WSTOPSIG(status) != SIGSTOP) {
        printf("PTRACE_TEST_FAIL first\n");
        return 1;
    }

    siginfo_t si;
    if (ptrace(PTRACE_GETSIGINFO, pid, 0, &si) != 0) {
        printf("PTRACE_TEST_FAIL getsiginfo\n");
        return 1;
    }
    if (si.si_signo != SIGSTOP) {
        printf("PTRACE_TEST_FAIL getsiginfo signo=%d\n", si.si_signo);
        return 1;
    }

    /* Use GETREGSET to also exercise the kernel's regset path. */
    struct iovec {
        void *iov_base;
        size_t iov_len;
    };
    unsigned long long buf[27];
    struct iovec iov;
    iov.iov_base = buf;
    iov.iov_len = sizeof(buf);
    if (ptrace(PTRACE_GETREGSET, pid, 1, &iov) != 0) {
        printf("PTRACE_TEST_FAIL getregset errno=%d\n", errno);
        return 1;
    }
    (void)buf;

    if (ptrace(PTRACE_CONT, pid, 0, 0) == -1) {
        perror("cont");
        return 1;
    }
    r = waitpid(pid, &status, 0);
    printf("[parent] exit waitpid=%d WIFEXITED=%d status=%d\n", r, WIFEXITED(status), WEXITSTATUS(status));
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 42) {
        printf("PTRACE_TEST_FAIL exit\n");
        return 1;
    }

    printf("PTRACE_TEST_PASS\n");
    return 0;
}
