/* DRM event path test: PAGE_FLIP -> poll() POLLIN -> read() flip-complete.
 * Verifies the compositor repaint heartbeat end-to-end. */
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
#include <poll.h>
#include <stdint.h>
#include <string.h>
#include <errno.h>

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

/* drm_event_vblank UAPI */
struct drm_event_vblank {
    uint32_t type;     /* 1 = VBLANK, 3 = FLIP_COMPLETE */
    uint32_t length;
    uint64_t user_data;
    uint64_t tv_sec;
    uint32_t tv_usec;
    uint32_t crtc_id;
};
#define DRM_EVENT_VBLANK 0x01
#define DRM_EVENT_FLIP_COMPLETE 0x02

static int fails = 0;
#define CHECK(cond, msg) do { \
    if (cond) printf("  ok: %s\n", msg); \
    else { printf("  FAIL: %s\n", msg); fails++; } \
} while (0)

int main() {
    printf("[DRM EVENT TEST] starting\n");
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { perror("open card0"); return 1; }

    struct drm_mode_card_res res = {0};
    uint32_t crtc_ids[8] = {0}, connector_ids[8] = {0};
    res.crtc_id_ptr = (uint64_t)crtc_ids;
    res.connector_id_ptr = (uint64_t)connector_ids;
    res.count_crtcs = 8;       /* caller presets counts to buffer capacity */
    res.count_connectors = 8;
    ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res);
    if (res.count_crtcs == 0 || crtc_ids[0] == 0) {
        printf("  FAIL: GETRESOURCES returned no CRTCs (count=%u id0=%u)\n",
               res.count_crtcs, crtc_ids[0]);
        return 1;
    }

    struct drm_mode_modeinfo modes[8] = {0};
    struct drm_mode_get_connector conn = {
        .connector_id = connector_ids[0], .modes_ptr = (uint64_t)modes, .count_modes = 8,
    };
    ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &conn);
    uint32_t w = modes[0].hdisplay ? modes[0].hdisplay : 1280;
    uint32_t h = modes[0].vdisplay ? modes[0].vdisplay : 800;

    /* two dumb buffers: A (blue) on screen, B (white) to flip to */
    int fbs[2]; uint8_t *maps[2];
    for (int i = 0; i < 2; i++) {
        struct drm_mode_create_dumb creq = { .width = w, .height = h, .bpp = 32 };
        if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &creq) < 0) { perror("create_dumb"); return 1; }
        struct drm_mode_map_dumb mreq = { .handle = creq.handle };
        ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &mreq);
        maps[i] = mmap(NULL, creq.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, mreq.offset);
        if (maps[i] == MAP_FAILED) { perror("mmap"); return 1; }
        memset(maps[i], i ? 0xFF : 0x40, (size_t)creq.pitch * h);
        struct drm_mode_fb_cmd fb = { .width = w, .height = h, .pitch = creq.pitch,
                                      .bpp = 32, .depth = 24, .handle = creq.handle };
        if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) < 0) { perror("addfb"); return 1; }
        fbs[i] = fb.fb_id;
    }

    struct drm_mode_crtc set = { .crtc_id = crtc_ids[0], .fb_id = fbs[0],
                                 .x = 0, .y = 0, .mode_valid = 1, .mode = modes[0] };
    if (ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &set) < 0) { perror("setcrtc"); return 1; }

    /* 1. poll before any flip: must NOT report readable within 100ms */
    struct pollfd p = { .fd = fd, .events = POLLIN };
    int r = poll(&p, 1, 100);
    CHECK(r == 0, "poll before flip times out (no stale events)");

    /* 2. flip to FB1 with a magic user_data */
    uint64_t magic = 0xDEADBEEFCAFE0001ULL;
    struct drm_mode_page_flip flip = {
        .crtc_id = crtc_ids[0], .fb_id = fbs[1], .user_data = magic,
    };
    CHECK(ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &flip) == 0, "PAGE_FLIP succeeds");

    /* 3. poll must now report readable */
    r = poll(&p, 1, 1000);
    CHECK(r == 1 && (p.revents & POLLIN), "poll after flip reports POLLIN");

    /* 4. read() must yield a 32-byte FLIP_COMPLETE with our user_data */
    uint8_t evbuf[256];
    ssize_t n = read(fd, evbuf, sizeof(evbuf));
    CHECK(n == 32, "read returns exactly one 32-byte event");
    if (n == 32) {
        struct drm_event_vblank *ev = (struct drm_event_vblank *)evbuf;
        CHECK(ev->type == DRM_EVENT_FLIP_COMPLETE, "event type == FLIP_COMPLETE");
        CHECK(ev->length == 32, "event length == 32");
        CHECK(ev->user_data == magic, "user_data round-trips");
        CHECK(ev->crtc_id == crtc_ids[0], "crtc_id matches");
    }

    /* 5. queue is empty again: poll times out */
    r = poll(&p, 1, 100);
    CHECK(r == 0, "poll drains to empty");

    /* 6. read with nothing pending returns EAGAIN, not a hang */
    n = read(fd, evbuf, sizeof(evbuf));
    CHECK(n < 0 && errno == EAGAIN, "empty read returns EAGAIN");

    close(fd);
    if (fails == 0) {
        printf("DRM EVENT TEST PASSED\n");
        return 0;
    }
    printf("DRM EVENT TEST FAILED (%d)\n", fails);
    return 1;
}
