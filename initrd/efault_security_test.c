#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/stat.h>

#define KERNEL_ADDR ((void*)0xffff800000000000ULL)
#define HIGH_ADDR   ((void*)0xffffffff80000000ULL)

int main(void) {
    printf("[efault_security_test] starting\n");

    // 1. read with kernel pointer
    ssize_t ret = read(0, KERNEL_ADDR, 32);
    if (ret != -1 || errno != EFAULT) {
        printf("FAIL: read(KERNEL_ADDR) returned %ld (errno=%d, expected EFAULT=%d)\n", ret, errno, EFAULT);
        return 1;
    }

    // 2. write with kernel pointer
    ret = write(1, KERNEL_ADDR, 32);
    if (ret != -1 || errno != EFAULT) {
        printf("FAIL: write(KERNEL_ADDR) returned %ld (errno=%d, expected EFAULT=%d)\n", ret, errno, EFAULT);
        return 1;
    }

    // 3. stat with kernel path pointer
    struct stat st;
    int sret = stat((const char*)KERNEL_ADDR, &st);
    if (sret != -1 || errno != EFAULT) {
        printf("FAIL: stat(KERNEL_ADDR) returned %d (errno=%d, expected EFAULT=%d)\n", sret, errno, EFAULT);
        return 1;
    }

    // 4. stat with kernel buffer pointer
    sret = stat("/", (struct stat*)KERNEL_ADDR);
    if (sret != -1 || errno != EFAULT) {
        printf("FAIL: stat('/', KERNEL_ADDR) returned %d (errno=%d, expected EFAULT=%d)\n", sret, errno, EFAULT);
        return 1;
    }

    // 5. getcwd with kernel pointer
    char *gret = getcwd((char*)KERNEL_ADDR, 128);
    if (gret != NULL || errno != EFAULT) {
        printf("FAIL: getcwd(KERNEL_ADDR) returned %p (errno=%d, expected EFAULT=%d)\n", gret, errno, EFAULT);
        return 1;
    }

    // 6. open with NULL pointer
    int oret = open(NULL, O_RDONLY);
    if (oret != -1 || errno != EFAULT) {
        printf("FAIL: open(NULL) returned %d (errno=%d, expected EFAULT=%d)\n", oret, errno, EFAULT);
        return 1;
    }

    // 7. open with kernel path pointer
    oret = open((const char*)HIGH_ADDR, O_RDONLY);
    if (oret != -1 || errno != EFAULT) {
        printf("FAIL: open(HIGH_ADDR) returned %d (errno=%d, expected EFAULT=%d)\n", oret, errno, EFAULT);
        return 1;
    }

    printf("EFAULT_SECURITY_PASS\n");
    return 0;
}
