#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <stdint.h>
#include <poll.h>
#include <sys/syscall.h>

#ifndef __NR_eventfd
#define __NR_eventfd 284
#endif

int main(void) {
    printf("[eventfd_test] starting\n");
    int efd = syscall(__NR_eventfd, 0, 0);
    if (efd < 0) {
        perror("eventfd");
        return 1;
    }

    uint64_t val = 5;
    if (write(efd, &val, sizeof(val)) != sizeof(val)) {
        perror("write 5");
        return 1;
    }

    uint64_t read_val = 0;
    if (read(efd, &read_val, sizeof(read_val)) != sizeof(read_val)) {
        perror("read 5");
        return 1;
    }
    if (read_val != 5) {
        printf("Expected 5, got %llu\n", (unsigned long long)read_val);
        return 1;
    }

    val = 1;
    if (write(efd, &val, sizeof(val)) != sizeof(val)) {
        perror("write 1");
        return 1;
    }

    struct pollfd pfd;
    pfd.fd = efd;
    pfd.events = POLLIN;
    pfd.revents = 0;

    int ret = poll(&pfd, 1, 1000);
    if (ret <= 0 || !(pfd.revents & POLLIN)) {
        printf("poll failed ret=%d revents=%d\n", ret, pfd.revents);
        return 1;
    }

    read_val = 0;
    if (read(efd, &read_val, sizeof(read_val)) != sizeof(read_val)) {
        perror("read 1");
        return 1;
    }
    if (read_val != 1) {
        printf("Expected 1, got %llu\n", (unsigned long long)read_val);
        return 1;
    }

    close(efd);
    printf("EVENTFD TEST PASSED\n");
    return 0;
}
