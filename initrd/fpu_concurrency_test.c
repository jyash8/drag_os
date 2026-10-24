#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sched.h>
#include <sys/wait.h>
#include <math.h>

#define ITERATIONS 5000

int main(void) {
    printf("[fpu_concurrency_test] starting\n");

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }

    if (pid == 0) {
        // Child process: compute with constant 2.5
        double val = 1.0;
        for (int i = 0; i < ITERATIONS; i++) {
            val = val * 1.0002 + 0.5;
            if (i % 50 == 0) {
                sched_yield();
            }
        }
        // Compute expected reference
        double expected = 1.0;
        for (int i = 0; i < ITERATIONS; i++) {
            expected = expected * 1.0002 + 0.5;
        }
        double diff = val - expected;
        if (diff < 0) diff = -diff;
        if (diff > 0.001) {
            printf("Child FPU corruption: val=%f expected=%f diff=%f\n", val, expected, diff);
            exit(1);
        }
        exit(0);
    }

    // Parent process: compute with constant 3.7
    double val = 2.0;
    for (int i = 0; i < ITERATIONS; i++) {
        val = val * 1.0003 + 0.3;
        if (i % 50 == 0) {
            sched_yield();
        }
    }
    double expected = 2.0;
    for (int i = 0; i < ITERATIONS; i++) {
        expected = expected * 1.0003 + 0.3;
    }
    double diff = val - expected;
    if (diff < 0) diff = -diff;
    if (diff > 0.001) {
        printf("Parent FPU corruption: val=%f expected=%f diff=%f\n", val, expected, diff);
        return 1;
    }

    int status = 0;
    waitpid(pid, &status, 0);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("Child exited with error status %d\n", status);
        return 1;
    }

    printf("FPU_CONCURRENCY_PASS\n");
    return 0;
}
