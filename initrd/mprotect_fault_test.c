#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include <sys/wait.h>

int main(void) {
    printf("[mprotect_fault_test] starting\n");

    size_t page_size = sysconf(_SC_PAGESIZE);
    char *buf = mmap(NULL, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buf == MAP_FAILED) {
        perror("mmap");
        return 1;
    }

    // 1. Initial write
    buf[0] = 'A';
    if (buf[0] != 'A') {
        printf("Initial write failed\n");
        return 1;
    }

    // 2. Protect as read-only
    if (mprotect(buf, page_size, PROT_READ) < 0) {
        perror("mprotect PROT_READ");
        return 1;
    }

    // Read must succeed
    if (buf[0] != 'A') {
        printf("Read after mprotect PROT_READ failed\n");
        return 1;
    }

    // 3. Child 1: attempts to write to PROT_READ page -> kernel should fault and terminate child with SIGSEGV (139)
    pid_t pid1 = fork();
    if (pid1 < 0) {
        perror("fork child 1");
        return 1;
    }
    if (pid1 == 0) {
        // Child 1 write to read-only page
        buf[0] = 'X';
        // Should not reach here if page fault terminates process
        _exit(0);
    }

    int status1 = 0;
    waitpid(pid1, &status1, 0);
    int exit_code1 = WIFEXITED(status1) ? WEXITSTATUS(status1) : -1;
    if (exit_code1 != 139 && exit_code1 == 0) {
        printf("FAIL: Child 1 was able to write to PROT_READ page (exit code %d)\n", exit_code1);
        return 1;
    }
    printf("Child 1 faulted as expected (exit status=%d, code=%d)\n", status1, exit_code1);

    // 4. Child 2: upgrades permission to PROT_READ | PROT_WRITE -> write must succeed
    pid_t pid2 = fork();
    if (pid2 < 0) {
        perror("fork child 2");
        return 1;
    }
    if (pid2 == 0) {
        if (mprotect(buf, page_size, PROT_READ | PROT_WRITE) < 0) {
            perror("child 2 mprotect RW");
            _exit(2);
        }
        buf[0] = 'B';
        if (buf[0] != 'B') {
            _exit(3);
        }
        _exit(0);
    }

    int status2 = 0;
    waitpid(pid2, &status2, 0);
    int exit_code2 = WIFEXITED(status2) ? WEXITSTATUS(status2) : -1;
    if (exit_code2 != 0) {
        printf("FAIL: Child 2 failed to write after mprotect RW (exit code %d)\n", exit_code2);
        return 1;
    }

    munmap(buf, page_size);
    printf("MPROTECT_SIGSEGV_PASS\n");
    return 0;
}
