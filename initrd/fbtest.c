#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/mman.h>
#include <stdint.h>

#define SYS_GET_FB_INFO 333
#define MAP_DEVICE 0x40

struct FbInfoSys {
    uint64_t phys_addr;
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint8_t bpp;
};

static long syscall1(long n, long a1) {
    long ret;
    __asm__ volatile ("syscall" : "=a"(ret) : "a"(n), "D"(a1) : "rcx", "r11", "memory");
    return ret;
}

int main() {
    struct FbInfoSys fb;
    if (syscall1(SYS_GET_FB_INFO, (long)&fb) != 0) {
        printf("fbtest: no fb\n");
        return 1;
    }
    printf("fbtest: FB %dx%d pitch %d addr %lx\n", fb.width, fb.height, fb.pitch, fb.phys_addr);

    size_t size = fb.pitch * fb.height * 4;
    uint32_t *p = mmap((void*)fb.phys_addr, size, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_DEVICE, -1, 0);
    printf("fbtest: mmap -> %p\n", p);
    if (p == MAP_FAILED) {
        printf("fbtest: mmap failed\n");
        return 1;
    }

    /* Fill with red */
    for (uint32_t y = 0; y < fb.height; y++) {
        uint32_t *row = p + (size_t)y * fb.pitch;
        for (uint32_t x = 0; x < fb.width; x++) {
            row[x] = 0x0000FF; /* BGR: red */
        }
    }
    printf("fbtest: filled red\n");
    sleep(3);

    /* Fill with green */
    for (uint32_t y = 0; y < fb.height; y++) {
        uint32_t *row = p + (size_t)y * fb.pitch;
        for (uint32_t x = 0; x < fb.width; x++) {
            row[x] = 0x00FF00; /* BGR: green */
        }
    }
    printf("fbtest: filled green\n");
    sleep(3);
    printf("fbtest: done\n");
    return 0;
}
