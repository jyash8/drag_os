#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

// Local wire-format for DrmModeCrtcLut (32 bytes).
struct drg_drm_mode_crtc_lut {
    uint32_t crtc_id;
    uint32_t gamma_size;
    uint64_t red;
    uint64_t green;
    uint64_t blue;
};

static int fail_stage = 0;
static void stage(int s) {
    fail_stage = s;
    char buf[32];
    int n = snprintf(buf, sizeof buf, "[gamma] stage %d\n", s);
    if (n > 0) write(1, buf, (size_t)n);
}
#define STAGE(s) stage(s)
#define CHECK(cond) do { if (!(cond)) { printf("GAMMA_PROBE_FAIL stage=%d errno=%d\n", fail_stage, errno); return 1; } } while(0)

int main(void) {
    STAGE(0);
    int fd = open("/dev/dri/card0", O_RDWR);
    CHECK(fd >= 0);
    struct drm_mode_card_res res = {0};
    uint32_t crtcs[64], conns[64], fbs[64], encs[64];
    res.crtc_id_ptr = (uintptr_t)crtcs;
    res.connector_id_ptr = (uintptr_t)conns;
    res.fb_id_ptr = (uintptr_t)fbs;
    res.encoder_id_ptr = (uintptr_t)encs;
    res.count_crtcs = 64; res.count_connectors = 64; res.count_fbs = 64; res.count_encoders = 64;
    STAGE(1); CHECK(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) == 0);
    STAGE(2); CHECK(res.count_crtcs > 0);
    uint32_t crtc_id = crtcs[0];

    /* GETGAMMA (gamma_size=0 → kernel reports current size) */
    {
        uint8_t  r[256], g[256], b[256];
        memset(r,0,256); memset(g,0,256); memset(b,0,256);
        struct drg_drm_mode_crtc_lut gl = {0};
        gl.crtc_id = crtc_id;
        gl.gamma_size = 0;
        gl.red = (uintptr_t)r; gl.green=(uintptr_t)g; gl.blue=(uintptr_t)b;
        STAGE(3); CHECK(ioctl(fd, DRM_IOCTL_MODE_GETGAMMA, &gl) == 0);
        STAGE(4); CHECK(gl.gamma_size == 256);
    }
    /* SETGAMMA (256 ramps ascending) + GETGAMMA again */
    {
        static uint16_t red[256], green[256], blue[256];
        for (int i=0;i<256;i++){ red[i]=i; green[i]=i; blue[i]=i; }
        struct drg_drm_mode_crtc_lut sl = {0};
        sl.crtc_id = crtc_id;
        sl.gamma_size = 256;
        sl.red = (uintptr_t)red; sl.green=(uintptr_t)green; sl.blue=(uintptr_t)blue;
        STAGE(5); CHECK(ioctl(fd, DRM_IOCTL_MODE_SETGAMMA, &sl) == 0);
        /* GETGAMMA validation against SETGAMMA */
        uint16_t r2[256], g2[256], b2[256];
        struct drg_drm_mode_crtc_lut gl = {0};
        gl.crtc_id = crtc_id;
        gl.gamma_size = 256;
        gl.red = (uintptr_t)r2; gl.green=(uintptr_t)g2; gl.blue=(uintptr_t)b2;
        STAGE(6); CHECK(ioctl(fd, DRM_IOCTL_MODE_GETGAMMA, &gl) == 0);
        STAGE(7); CHECK(gl.gamma_size == 256);
        STAGE(8); CHECK(r2[0] == 0 && r2[255] == 255);
    }
    printf("GAMMA_PROBE_PASS\n");
    return 0;
}
