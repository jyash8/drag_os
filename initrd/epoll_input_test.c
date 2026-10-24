#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/epoll.h>
#include <errno.h>
#include <string.h>
#include <linux/input.h>

int main(void) {
    int fd = open("/dev/input/event0", O_RDONLY | O_NONBLOCK);
    if (fd < 0) { perror("open"); return 1; }
    int ep = epoll_create1(0);
    if (ep < 0) { perror("epoll_create1"); return 1; }
    struct epoll_event ev = { .events = EPOLLIN, .data.u32 = 42 };
    if (epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev) < 0) { perror("epoll_ctl"); return 1; }
    printf("waiting up to 10s for events via epoll...\n"); fflush(stdout);
    for (int i = 0; i < 100; i++) {
        struct epoll_event out;
        int n = epoll_wait(ep, &out, 1, 100);
        if (n > 0 && (out.events & EPOLLIN)) {
            char buf[256];
            int got = 0;
            while (read(fd, buf, sizeof(buf)) > 0) { got++; }
            printf("epoll readable: drained %d events\n", got);
            fflush(stdout);
            if (got > 4) break;
        } else if (n == 0 && i % 25 == 24) {
            printf("still waiting (%d)\n", i); fflush(stdout);
        }
    }
    printf("epoll_input_test done\n");
    return 0;
}
