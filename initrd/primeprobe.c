#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

int main(void) {
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { printf("PRIME_FAIL open errno=%d\n", errno); return 1; }

    // create dumb 64x64
    struct drm_mode_create_dumb c = {0};
    c.width = 64; c.height = 64; c.bpp = 32;
    if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &c) != 0) {
        printf("PRIME_FAIL create_dumb errno=%d\n", errno); return 2; }

    // map dumb (not strictly needed, keeps BO alive while exported)
    struct drm_mode_map_dumb m = {0};
    m.handle = c.handle;
    if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &m) != 0) {
        printf("PRIME_FAIL map_dumb errno=%d\n", errno); return 3; }
    struct drm_mode_destroy_dumb d = {0};
    d.handle = c.handle;

    // export
    struct drm_prime_handle ph = {0};
    ph.handle = c.handle; ph.flags = DRM_CLOEXEC;
    if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) != 0 || ph.fd < 0) {
        printf("PRIME_FAIL export errno=%d\n", errno); return 4; }

    // mmap the dma-buf fd and scribble a pattern
    unsigned char *p = mmap(NULL, 64*64*4, PROT_READ|PROT_WRITE, MAP_SHARED, ph.fd, 0);
    if (p == MAP_FAILED) {
        printf("PRIME_FAIL mmap errno=%d\n", errno); return 5; }
    for (int i = 0; i < 64*64*4; i++) p[i] = (unsigned char)(i & 0xff);
    munmap(p, 64*64*4);

    // fork child, re-import inherited fd
    int st = 0;
    pid_t pid = fork();
    if (pid == 0) {
        struct drm_prime_handle ph2 = {0};
        ph2.fd = ph.fd;
        if (ioctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &ph2) != 0) {
            printf("PRIME_FAIL child import errno=%d\n", errno); _exit(6); }
        unsigned char *q = mmap(NULL, 64*64*4, PROT_READ, MAP_SHARED, ph.fd, 0);
        if (q == MAP_FAILED) {
            printf("PRIME_FAIL child mmap errno=%d\n", errno); _exit(7); }
        int ok = 1;
        for (int i = 0; i < 64*64*4; i++) if (q[i] != (unsigned char)(i & 0xff)) { ok = 0; break; }
        munmap(q, 64*64*4);
        printf(ok ? "PRIME_CHILD_READBACK_OK\n" : "PRIME_CHILD_READBACK_BAD\n");
        _exit(ok ? 0 : 8);
    }
    waitpid(pid, &st, 0);
    close(ph.fd);
    ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &d);
    close(fd);

    if (WIFEXITED(st) && WEXITSTATUS(st) == 0) {
        printf("PRIME_PROBE_PASS\n"); return 0; }
    printf("PRIME_FAIL child status=%d\n", st); return 9;
}
