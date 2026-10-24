// kms_player.c — bypass mywima, render raw RGB frames from stdin via DRM/KMS.
// ponytail: simplest possible video test. Streams raw rgb32 frames from stdin
// onto /dev/dri/card0 via dumb buffers and page flipping. If this works, the
// kernel graphics stack is sound and any issue is in mywima.
/*
 * DRM ioctl numbers come from the kernel's own uapi crate so the guest
 * cannot drift from the driver. See scripts/generate-drm-ioctls.sh.
 */
#include "drm_ioctls.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <poll.h>
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
struct drm_mode_fb_cmd { uint32_t fb_id, width, height, pitch, bpp, depth, handle, pad; };
struct drm_mode_crtc {
    uint64_t set_connectors_ptr;
    uint32_t count_connectors, crtc_id, fb_id, x, y, gamma_size, mode_valid;
    struct drm_mode_modeinfo mode;
};
struct drm_mode_create_dumb { uint32_t height, width, bpp, flags, handle, pitch; uint64_t size; };
struct drm_mode_map_dumb { uint32_t handle, pad; uint64_t offset; };
struct drm_mode_destroy_dumb { uint32_t handle; };
struct drm_mode_page_flip { uint32_t crtc_id, fb_id, flags, reserved; uint64_t user_data; };
struct drm_mode_rmfb { uint32_t fb_id; uint32_t pad; };

static void die(const char *m) { perror(m); exit(1); }

int main(int argc, char **argv) {
    int src_w = 1280, src_h = 800;
    if (argc >= 3) { src_w = atoi(argv[1]); src_h = atoi(argv[2]); }
    // ponytail: argv[3] = input file (bypasses shell stdin redirect entirely).
    int infd = 0;
    if (argc >= 4) {
        infd = open(argv[3], O_RDONLY);
        if (infd < 0) { printf("[KMS-PLAYER] open %s failed\n", argv[3]); return 1; }
        printf("[KMS-PLAYER] src=%dx%d file=%s\n", src_w, src_h, argv[3]);
    } else {
        printf("[KMS-PLAYER] src=%dx%d\n", src_w, src_h);
    }
    // ponytail: argv[4] = s16le 44.1k stereo audio, argv[5] = bytes per video
    // frame. No A/V sync — fire-and-forget test of the AC97 path.
    int aufd = -1, dspfd = -1;
    size_t a_per_frame = 0;
    if (argc >= 6) {
        aufd = open(argv[4], O_RDONLY);
        a_per_frame = (size_t)atoi(argv[5]);
        dspfd = open("/dev/dsp", O_WRONLY);
        if (dspfd >= 0) {
            // ponytail: match the clip (44.1kHz s16le stereo); driver
            // defaults to mono, which plays stereo bytes 2x fast (crackle).
            int sr = 44100, ch = 2, fmt = 16;
            ioctl(dspfd, 0x5002, &sr);  // SNDCTL_DSP_SPEED
            ioctl(dspfd, 0x5003, &ch);  // SNDCTL_DSP_STEREO
            ioctl(dspfd, 0x5005, &fmt); // SNDCTL_DSP_SETFMT
            printf("[KMS-PLAYER] audio=%s aufd=%d dspfd=%d bpf=%zu sr=%d ch=%d fmt=%d\n",
                   argv[4], aufd, dspfd, a_per_frame, sr, ch, fmt);
        } else {
            printf("[KMS-PLAYER] audio=%s aufd=%d dspfd=%d bpf=%zu\n",
                   argv[4], aufd, dspfd, a_per_frame);
        }
    }

    int drm = open("/dev/dri/card0", O_RDWR);
    if (drm < 0) die("open /dev/dri/card0");

    // resources
    struct drm_mode_card_res res = {0};
    ioctl(drm, DRM_IOCTL_MODE_GETRESOURCES, &res);
    uint32_t crtc_ids[8] = {0}, connector_ids[8] = {0};
    res.crtc_id_ptr = (uint64_t)crtc_ids;
    res.connector_id_ptr = (uint64_t)connector_ids;
    ioctl(drm, DRM_IOCTL_MODE_GETRESOURCES, &res);

    struct drm_mode_modeinfo modes[8] = {0};
    struct drm_mode_get_connector conn = {
        .connector_id = connector_ids[0], .modes_ptr = (uint64_t)modes, .count_modes = 8,
    };
    ioctl(drm, DRM_IOCTL_MODE_GETCONNECTOR, &conn);
    uint32_t dw = modes[0].hdisplay;
    uint32_t dh = modes[0].vdisplay;
    printf("[KMS-PLAYER] display=%ux%u\n", dw, dh);

    // two framebuffers
    uint32_t fbs[2];
    void *maps[2];
    uint32_t pitch = 0;
    for (int i = 0; i < 2; i++) {
        struct drm_mode_create_dumb creq = { .width = dw, .height = dh, .bpp = 32 };
        if (ioctl(drm, DRM_IOCTL_MODE_CREATE_DUMB, &creq) < 0) die("create_dumb");
        pitch = creq.pitch;
        struct drm_mode_map_dumb mreq = { .handle = creq.handle };
        ioctl(drm, DRM_IOCTL_MODE_MAP_DUMB, &mreq);
        maps[i] = mmap(NULL, creq.size, PROT_READ | PROT_WRITE, MAP_SHARED, drm, mreq.offset);
        if (maps[i] == MAP_FAILED) die("mmap");
        memset(maps[i], 0, creq.size);
        struct drm_mode_fb_cmd fb = { .width = dw, .height = dh, .pitch = creq.pitch, .bpp = 32, .depth = 24, .handle = creq.handle };
        if (ioctl(drm, DRM_IOCTL_MODE_ADDFB, &fb) < 0) die("addfb");
        fbs[i] = fb.fb_id;
    }
    printf("[KMS-PLAYER] FB0=%u FB1=%u pitch=%u\n", fbs[0], fbs[1], pitch);

    struct drm_mode_crtc set = {
        .crtc_id = crtc_ids[0], .fb_id = fbs[0], .x = 0, .y = 0,
        .mode_valid = 1, .mode = modes[0],
    };
    if (ioctl(drm, DRM_IOCTL_MODE_SETCRTC, &set) < 0) die("setcrtc");
    printf("[KMS-PLAYER] mode set, fb0 on screen\n");

    // frame loop — read rgb32 from stdin, blit (centered if mismatch) to back buf, flip.
    size_t row_bytes = (size_t)pitch;
    size_t frame_bytes = (size_t)dw * dh * 4;

    int cur = 0; // back buffer index
    int n = 0;
    for (;;) {
        // poll input so we can break on EOF (infd = file or stdin)
        struct pollfd pfd = { .fd = infd, .events = POLLIN };
        int pr = poll(&pfd, 1, 100);
        if (pr <= 0) {
            if (pr < 0 && errno != EINTR) break;
            continue;
        }
        void *back = maps[cur ^ 1];
        memset(back, 0, (size_t)dw * dh * 4);
        uint32_t x_off = 0, y_off = 0;
        if (src_w < dw) x_off = (dw - src_w) / 2;
        if (src_h < dh) y_off = (dh - src_h) / 2;
        // ponytail: one read per frame (was 200 syscalls/frame).
        size_t frame_sz = (size_t)src_w * src_h * 4;
        static char *fbuf = 0;
        static size_t fcap = 0;
        if (fcap < frame_sz) {
            fcap = frame_sz;
            fbuf = malloc(fcap);
            if (!fbuf) break;
        }
        size_t got = 0;
        while (got < frame_sz) {
            ssize_t r = read(infd, fbuf + got, frame_sz - got);
            if (r <= 0) break;
            got += (size_t)r;
        }
        if (got != frame_sz) break;
        for (uint32_t y = 0; y < (uint32_t)src_h; y++) {
            uint32_t *dst = (uint32_t *)back + (y + y_off) * (pitch / 4) + x_off;
            memcpy(dst, fbuf + (size_t)y * src_w * 4, (size_t)src_w * 4);
        }

        struct drm_mode_page_flip flip = { .fb_id = fbs[cur ^ 1], .crtc_id = crtc_ids[0] };
        if (ioctl(drm, DRM_IOCTL_MODE_PAGE_FLIP, &flip) < 0) die("page_flip");
        cur ^= 1;
        n++;
        if (aufd >= 0 && dspfd >= 0 && a_per_frame > 0) {
            // ponytail: one audio chunk per video frame; short reads at EOF
            // just play less. Static buffer, no malloc in the loop.
            static char abuf[8192];
            size_t want = a_per_frame < sizeof(abuf) ? a_per_frame : sizeof(abuf);
            ssize_t ar = read(aufd, abuf, want);
            if (ar > 0) {
                ssize_t w = write(dspfd, abuf, (size_t)ar);
                (void)w;
                if (n % 60 == 0) printf("[KMS-PLAYER] %d frames au=%zd\n", n, ar);
            } else if (n % 60 == 0) {
                printf("[KMS-PLAYER] %d frames\n", n);
            }
        } else if (n % 60 == 0) {
            printf("[KMS-PLAYER] %d frames\n", n);
        }
    }

    printf("[KMS-PLAYER] EOF after %d frames\n", n);
    ioctl(drm, DRM_IOCTL_DROP_MASTER, 0);
    close(drm);
    return 0;
}
