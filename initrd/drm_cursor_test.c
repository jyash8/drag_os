/* Hardware cursor plane test for DragOS DRM.
 * Verifies: MODE_CURSOR BO (bind 64x64 dumb buffer as cursor),
 * MOVE (reposition), and hide (BO with handle=0).
 */

/*
 * DRM ioctl numbers come from the kernel's own uapi crate so the guest
 * cannot drift from the driver. See scripts/generate-drm-ioctls.sh.
 */
#include "drm_ioctls.h"
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <fcntl.h>
#include <string.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <sys/mman.h>

#define DRM_MODE_CURSOR_MOVE 1
#define DRM_MODE_CURSOR_BO   2

struct drm_mode_create_dumb {
    uint32_t height, width, bpp, flags;
    uint32_t handle, pitch;
    uint64_t size;
};
struct drm_mode_map_dumb {
    uint32_t handle, pad;
    uint64_t offset;
};
struct drm_mode_destroy_dumb {
    uint32_t handle;
};
struct drm_mode_cursor {
    uint32_t flags, crtc_id;
    int32_t x, y;
    uint32_t width, height, handle;
};

static int checks = 0, fails = 0;
#define CHECK(cond, name) do { \
    if (cond) { printf("[OK] %s\n", name); checks++; } \
    else { printf("[FAIL] %s\n", name); fails++; } \
} while (0)

int main(void) {
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { perror("open"); return 1; }

    /* Allocate a 64x64 cursor image and paint an arrow pattern */
    struct drm_mode_create_dumb create = {0};
    create.width = 64; create.height = 64; create.bpp = 32;
    CHECK(ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create) == 0, "create 64x64 dumb");

    struct drm_mode_map_dumb map = {0};
    map.handle = create.handle;
    ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map);
    uint32_t *fb = mmap(NULL, create.size, PROT_WRITE, MAP_SHARED, fd, map.offset);
    CHECK(fb != MAP_FAILED, "mmap cursor image");
    for (uint32_t y = 0; y < 64 && fb != MAP_FAILED; y++)
        for (uint32_t x = 0; x < 64; x++)
            fb[y * (create.pitch / 4) + x] = (x <= y) ? 0xFFFFFFFF : 0xFF000000;

    struct drm_mode_cursor cur = {0};
    cur.crtc_id = 1;

    /* Bind the cursor image at (100,100) */
    cur.flags = DRM_MODE_CURSOR_BO;
    cur.x = 100; cur.y = 100;
    cur.width = 64; cur.height = 64;
    cur.handle = create.handle;
    CHECK(ioctl(fd, DRM_IOCTL_MODE_CURSOR, &cur) == 0, "cursor BO bind @100,100");

    /* Move it */
    cur.flags = DRM_MODE_CURSOR_MOVE;
    cur.x = 200; cur.y = 150;
    CHECK(ioctl(fd, DRM_IOCTL_MODE_CURSOR, &cur) == 0, "cursor MOVE @200,150");

    cur.x = -5; cur.y = 300;
    CHECK(ioctl(fd, DRM_IOCTL_MODE_CURSOR, &cur) == 0, "cursor MOVE clamps negative x");

    /* Oversized image must be rejected */
    cur.flags = DRM_MODE_CURSOR_BO;
    cur.width = 128; cur.height = 128;
    cur.handle = create.handle;
    errno = 0;
    int r = ioctl(fd, DRM_IOCTL_MODE_CURSOR, &cur);
    CHECK(r == -1, "cursor BO 128x128 rejected");

    /* Hide */
    cur.flags = DRM_MODE_CURSOR_BO;
    cur.width = 64; cur.height = 64;
    cur.handle = 0;
    CHECK(ioctl(fd, DRM_IOCTL_MODE_CURSOR, &cur) == 0, "cursor hide (handle=0)");

    struct drm_mode_destroy_dumb destroy = { .handle = create.handle };
    ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);

    close(fd);
    printf("drm_cursor_test: %d/%d checks passed\n", checks, checks + fails);
    return fails ? 1 : 0;
}
