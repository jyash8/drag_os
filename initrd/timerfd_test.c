#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <stdint.h>
#include <sys/syscall.h>

#define CLOCK_MONOTONIC 1

#undef __NR_timerfd_create
#define __NR_timerfd_create 283
#undef __NR_timerfd_settime
#define __NR_timerfd_settime 286

struct timespec {
    int64_t tv_sec;
    int64_t tv_nsec;
};

struct itimerspec {
    struct timespec it_interval;
    struct timespec it_value;
};

int main(void) {
    printf("[timerfd_test] starting\n");
    int tfd = syscall(__NR_timerfd_create, CLOCK_MONOTONIC, 0);
    if (tfd < 0) {
        perror("timerfd_create");
        return 1;
    }

    // Arm 100ms one-shot
    struct itimerspec its;
    its.it_interval.tv_sec = 0;
    its.it_interval.tv_nsec = 0;
    its.it_value.tv_sec = 0;
    its.it_value.tv_nsec = 100 * 1000 * 1000; // 100ms

    if (syscall(__NR_timerfd_settime, tfd, 0, &its, NULL) < 0) {
        perror("timerfd_settime 100ms");
        return 1;
    }

    uint64_t expirations = 0;
    if (read(tfd, &expirations, sizeof(expirations)) != sizeof(expirations)) {
        perror("read one-shot");
        return 1;
    }
    if (expirations != 1) {
        printf("Expected 1 expiration for one-shot, got %llu\n", (unsigned long long)expirations);
        return 1;
    }

    // Arm 50ms periodic
    its.it_interval.tv_sec = 0;
    its.it_interval.tv_nsec = 50 * 1000 * 1000; // 50ms
    its.it_value.tv_sec = 0;
    its.it_value.tv_nsec = 50 * 1000 * 1000;    // 50ms

    if (syscall(__NR_timerfd_settime, tfd, 0, &its, NULL) < 0) {
        perror("timerfd_settime 50ms periodic");
        return 1;
    }

    // Read first expiry
    expirations = 0;
    if (read(tfd, &expirations, sizeof(expirations)) != sizeof(expirations)) {
        perror("read periodic 1");
        return 1;
    }
    if (expirations != 1) {
        printf("Expected 1 expiration for first periodic read, got %llu\n", (unsigned long long)expirations);
        return 1;
    }

    // Wait 120ms (gap of ~2.4 periods)
    usleep(120000);

    // Second read should return 2
    expirations = 0;
    if (read(tfd, &expirations, sizeof(expirations)) != sizeof(expirations)) {
        perror("read periodic 2");
        return 1;
    }
    if (expirations < 2) {
        printf("Expected >= 2 expirations for second periodic read after 120ms, got %llu\n", (unsigned long long)expirations);
        return 1;
    }

    close(tfd);
    printf("TIMERFD TEST PASSED\n");
    return 0;
}
