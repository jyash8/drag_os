/*
 * DRM ioctl numbers come from the kernel's own uapi crate so the guest
 * cannot drift from the driver. See scripts/generate-drm-ioctls.sh.
 */
#include "drm_ioctls.h"
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <stdint.h>

struct drm_mode_modeinfo {
    uint32_t clock;
    uint16_t hdisplay, hsync_start, hsync_end, htotal, hskew;
    uint16_t vdisplay, vsync_start, vsync_end, vtotal, vscan;
    uint32_t vrefresh, flags, type;
    char name[32];
};
struct drm_mode_card_res {
    uint64_t fb_id_ptr, crtc_id_ptr, connector_id_ptr, encoder_id_ptr;
    uint32_t count_fbs, count_crtcs, count_connectors, count_encoders;
    uint32_t min_width, max_width, min_height, max_height;
};
struct drm_mode_get_connector {
    uint64_t encoders_ptr, modes_ptr, props_ptr, prop_values_ptr;
    uint32_t count_modes, count_props, count_encoders, encoder_id;
    uint32_t connector_id, connector_type, connector_type_id, connection;
    uint32_t mm_width, mm_height, subpixel, pad;
};
struct drm_mode_fb_cmd { uint32_t fb_id, width, height, pitch, bpp, depth, handle; };
struct drm_mode_crtc {
    uint64_t set_connectors_ptr;
    uint32_t count_connectors, crtc_id, fb_id, x, y, gamma_size, mode_valid;
    struct drm_mode_modeinfo mode;
};
struct drm_mode_create_dumb { uint32_t height, width, bpp, flags, handle, pitch; uint64_t size; };
struct drm_mode_map_dumb { uint32_t handle, pad; uint64_t offset; };
struct drm_mode_page_flip { uint32_t crtc_id, fb_id, flags, reserved; uint64_t user_data; };

static void fill(uint32_t *px, uint32_t w, uint32_t h, uint32_t pitch, uint32_t color) {
    for (uint32_t y = 0; y < h; y++)
        for (uint32_t x = 0; x < w; x++)
            px[y * (pitch / 4) + x] = color;
}

int main() {
    printf("[FLIP TEST] Starting double-buffer page flip test...\n");
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { perror("open"); return 1; }

    struct drm_mode_card_res res = {0};
    ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res);
    uint32_t crtc_ids[8] = {0}, connector_ids[8] = {0};
    res.crtc_id_ptr = (uint64_t)crtc_ids;
    res.connector_id_ptr = (uint64_t)connector_ids;
    ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res);

    struct drm_mode_modeinfo modes[8] = {0};
    struct drm_mode_get_connector conn = {
        .connector_id = connector_ids[0], .modes_ptr = (uint64_t)modes, .count_modes = 8,
    };
    ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn);
    uint32_t w = modes[0].hdisplay ? modes[0].hdisplay : 1280;
    uint32_t h = modes[0].vdisplay ? modes[0].vdisplay : 800;
    printf("[FLIP TEST] Mode %ux%u\n", w, h);

    int fbs[2]; void *maps[2];
    for (int i = 0; i < 2; i++) {
        struct drm_mode_create_dumb creq = { .width = w, .height = h, .bpp = 32 };
        if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &creq) < 0) { perror("create_dumb"); return 1; }
        struct drm_mode_map_dumb mreq = { .handle = creq.handle };
        ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mreq);
        maps[i] = mmap(NULL, creq.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, mreq.offset);
        if (maps[i] == MAP_FAILED) { perror("mmap"); return 1; }
        struct drm_mode_fb_cmd fb = { .width = w, .height = h, .pitch = creq.pitch, .bpp = 32, .depth = 24, .handle = creq.handle };
        if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) < 0) { perror("addfb"); return 1; }
        fbs[i] = fb.fb_id;
        printf("[FLIP TEST] FB %d mapped at %p\n", fbs[i], maps[i]);
    }

    fill(maps[0], w, h, w * 4, 0xFF0000); /* red   */
    fill(maps[1], w, h, w * 4, 0x00FF00);        /* green */

    struct drm_mode_crtc set = { .crtc_id = crtc_ids[0], .fb_id = fbs[0], .x = 0, .y = 0, .mode_valid = 1, .mode = modes[0] };
    if (ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &set) < 0) { perror("setcrtc"); return 1; }
    printf("[FLIP TEST] Initial red framebuffer on screen\n");

    for (int i = 0; i < 6; i++) {
        struct drm_mode_page_flip flip = { .fb_id = fbs[i % 2], .crtc_id = crtc_ids[0] };
        if (ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &flip) < 0) { perror("page_flip"); return 1; }
        printf("[FLIP TEST] Flipped to %s framebuffer\n", (i % 2) ? "green" : "red");
        usleep(200000);
    }
    printf("[FLIP TEST] Page flip test PASSED!\n");
    close(fd);
    return 0;
}
