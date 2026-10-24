/* Simple DRM test program for DragOS.
 * Verifies: open, version query, dumb buffer allocation,
 * mmap, pixel write, and buffer destruction.
 *
 * Usage: ./drm_test
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <sys/mman.h>

#include "drm.h"

#define WIDTH 640
#define HEIGHT 480

int main(void) {
    int fd;
    int ret;
    uint32_t version_major, version_minor, version_patch;
    struct drm_version version;
    struct drm_mode_create_dumb create;
    struct drm_mode_map_dumb map;
    struct drm_mode_destroy_dumb destroy;
    void *map_data;
    int i, j;

    /* Open the DRM control node */
    fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) {
        fprintf(stderr, "Error: open /dev/dri/card0: %s\n", strerror(errno));
        return 1;
    }
    printf("Opened /dev/dri/card0\n");

    /* Query driver version */
    memset(&version, 0, sizeof(version));
    version.version_major = 1;
    version.version_minor = 4;
    version.version_patchlevel = 0;
    strcpy(version.name, "drm");
    strcpy(version.date, "20140924");
    strcpy(version.desc, "DRM for DragOS");
    if (ioctl(fd, DRM_IOCTL_VERSION, &version) < 0) {
        fprintf(stderr, "Error: DRM_IOCTL_VERSION failed: %s\n", strerror(errno));
        close(fd);
        return 1;
    }
    printf("Driver version: %d.%d.%d\n",
           version.version_major, version.version_minor,
           version.version_patchlevel);

    /* Allocate a dumb buffer (640x480, 32 bpp) */
    memset(&create, 0, sizeof(create));
    create.width = WIDTH;
    create.height = HEIGHT;
    create.bpp = 32;
    create.flags = 0;

    ret = ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create);
    if (ret < 0) {
        fprintf(stderr, "Error: DRM_IOCTL_MODE_CREATE_DUMB failed: %s\n", strerror(errno));
        close(fd);
        return 1;
    }
    printf("Allocated dumb buffer: handle=%u, pitch=%u, size=%llu\n",
           create.handle, create.pitch,
           (unsigned long long)create.size);

    /* Map the dumb buffer into userspace */
    memset(&map, 0, sizeof(map));
    map.handle = create.handle;

    ret = ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map);
    if (ret < 0) {
        fprintf(stderr, "Error: DRM_IOCTL_MODE_MAP_DUMB failed: %s\n", strerror(errno));
        destroy.handle = create.handle;
        ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
        close(fd);
        return 1;
    }
    printf("Mapped dumb buffer at offset %llx (fd offset)\n",
           (unsigned long long)map.offset);

    map_data = mmap(NULL, create.size, PROT_READ | PROT_WRITE,
                    MAP_SHARED, fd, map.offset);
    if (map_data == MAP_FAILED) {
        fprintf(stderr, "Error: mmap failed: %s\n", strerror(errno));
        destroy.handle = create.handle;
        ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
        close(fd);
        return 1;
    }
    printf("mmap succeeded at %p\n", map_data);

    /* Write some RGB test pixels (BGRA format, 32 bpp) */
    uint32_t *pixels = (uint32_t *)map_data;
    for (j = 0; j < HEIGHT && j < 100; j++) {
        for (i = 0; i < WIDTH && i < 100; i++) {
            /* Simple gradient pattern */
            pixels[j * WIDTH + i] =
                ((i & 0xFF) <<  0) | /* B */
                ((j & 0xFF) <<  8) | /* G */
                (((i + j) & 0xFF) << 16) | /* R */
                (0xFF << 24); /* A */
        }
    }
    printf("Wrote test pixels to first 100x100 area\n");

    /* Release the mmap */
    munmap(map_data, create.size);

    /* Destroy the dumb buffer */
    memset(&destroy, 0, sizeof(destroy));
    destroy.handle = create.handle;
    ret = ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
    if (ret < 0) {
        fprintf(stderr, "Warning: DRM_IOCTL_MODE_DESTROY_DUMB failed: %s\n", strerror(errno));
    } else {
        printf("Destroyed dumb buffer\n");
    }

    close(fd);
    printf("DRM test completed successfully!\n");
    return 0;
}