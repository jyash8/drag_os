#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <errno.h>
#include <fcntl.h>

#ifndef __NR_init_module
#define __NR_init_module 175
#endif

#ifndef __NR_finit_module
#define __NR_finit_module 313
#endif

static void fail(const char *msg) {
    fprintf(stderr, "insmod: %s\n", msg);
    if (errno) perror("insmod");
    exit(1);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: insmod <module.o>\n");
        return 1;
    }
    const char *path = argv[1];
    int fd = open(path, O_RDONLY);
    if (fd < 0) fail("open failed");
    struct stat st;
    if (fstat(fd, &st) != 0) fail("stat failed");
    if (st.st_size < 0 || st.st_size > 32 * 1024 * 1024) {
        fprintf(stderr, "insmod: file too large or empty\n");
        close(fd);
        return 1;
    }
    size_t len = (size_t)st.st_size;
    void *buf = malloc(len);
    if (!len || !buf) { close(fd); fail("alloc failed"); }
    size_t off = 0;
    while (off < len) {
        ssize_t n = read(fd, (char*)buf + off, len - off);
        if (n <= 0) { close(fd); fail("read failed"); }
        off += (size_t)n;
    }
    close(fd);

    long r = syscall(__NR_init_module, buf, len, "");
    free(buf);
    if (r < 0) {
        fprintf(stderr, "insmod: init_module failed, errno=%d\n", errno);
        return 1;
    }
    printf("insmod: '%s' loaded successfully\n", path);
    return 0;
}