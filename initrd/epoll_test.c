#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <stdint.h>
#include <string.h>
#include <sys/syscall.h>

#define EPOLL_CTL_ADD 1
#define EPOLL_CTL_DEL 2
#define EPOLL_CTL_MOD 3
#define EPOLLIN 0x001

#undef __NR_epoll_create1
#define __NR_epoll_create1 291
#undef __NR_epoll_ctl
#define __NR_epoll_ctl 233
#undef __NR_epoll_wait
#define __NR_epoll_wait 232
#undef __NR_eventfd
#define __NR_eventfd 284

struct epoll_event {
    uint32_t events;
    uint64_t data;
} __attribute__((packed));

int main(void) {
    printf("[epoll_test] starting\n");

    int epfd = syscall(__NR_epoll_create1, 0);
    if (epfd < 0) {
        perror("epoll_create1");
        return 1;
    }

    int pipefd[2];
    if (pipe(pipefd) < 0) {
        perror("pipe");
        return 1;
    }

    // Register pipe read end
    struct epoll_event ev;
    ev.events = EPOLLIN;
    ev.data = 0x12345;
    if (syscall(__NR_epoll_ctl, epfd, EPOLL_CTL_ADD, pipefd[0], &ev) < 0) {
        perror("epoll_ctl ADD pipe");
        return 1;
    }

    struct epoll_event events[4];

    // 1. Initially empty pipe must return 0
    int n = syscall(__NR_epoll_wait, epfd, events, 4, 0);
    if (n != 0) {
        printf("initial epoll_wait on empty pipe expected 0, got %d\n", n);
        return 1;
    }

    // 2. Write data to pipe -> epoll_wait must report 1 event
    char c = 'x';
    if (write(pipefd[1], &c, 1) != 1) {
        perror("write to pipe");
        return 1;
    }

    n = syscall(__NR_epoll_wait, epfd, events, 4, 100);
    if (n != 1) {
        printf("epoll_wait after write expected 1 event, got %d\n", n);
        return 1;
    }
    if (!(events[0].events & EPOLLIN)) {
        printf("epoll_wait expected EPOLLIN, got 0x%x\n", events[0].events);
        return 1;
    }
    if (events[0].data != 0x12345) {
        printf("epoll_wait expected data 0x12345, got 0x%llx\n", (unsigned long long)events[0].data);
        return 1;
    }

    // 3. Drain pipe -> subsequent poll must return 0
    char buf[16] = {0};
    if (read(pipefd[0], buf, 1) != 1 || buf[0] != 'x') {
        printf("read from pipe failed\n");
        return 1;
    }

    n = syscall(__NR_epoll_wait, epfd, events, 4, 0);
    if (n != 0) {
        printf("drained epoll_wait expected 0, got %d\n", n);
        return 1;
    }

    // 4. Test eventfd under epoll
    int efd = syscall(__NR_eventfd, 0, 0);
    if (efd >= 0) {
        struct epoll_event ev_efd;
        ev_efd.events = EPOLLIN;
        ev_efd.data = 0x67890;
        if (syscall(__NR_epoll_ctl, epfd, EPOLL_CTL_ADD, efd, &ev_efd) == 0) {
            uint64_t val = 1;
            if (write(efd, &val, sizeof(val)) == sizeof(val)) {
                n = syscall(__NR_epoll_wait, epfd, events, 4, 100);
                if (n == 1 && events[0].data == 0x67890) {
                    uint64_t read_val = 0;
                    read(efd, &read_val, sizeof(read_val));
                }
            }
            syscall(__NR_epoll_ctl, epfd, EPOLL_CTL_DEL, efd, NULL);
        }
        close(efd);
    }

    // 5. Test EPOLL_CTL_DEL on pipe
    if (syscall(__NR_epoll_ctl, epfd, EPOLL_CTL_DEL, pipefd[0], NULL) < 0) {
        perror("epoll_ctl DEL pipe");
        return 1;
    }
    write(pipefd[1], &c, 1);
    n = syscall(__NR_epoll_wait, epfd, events, 4, 0);
    if (n != 0) {
        printf("epoll_wait after DEL expected 0, got %d\n", n);
        return 1;
    }

    close(pipefd[0]);
    close(pipefd[1]);
    close(epfd);

    printf("EPOLL TEST PASSED\n");
    return 0;
}
