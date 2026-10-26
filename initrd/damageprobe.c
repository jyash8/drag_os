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

// Local copy matching kernel DrmClipRect (8 bytes).
struct drg_drm_clip_rect {
    uint16_t x1, y1, x2, y2;
};
struct drg_drm_mode_fb_dirty_cmd {
    uint32_t fb_id;
    uint32_t flags;
    uint32_t color;
    uint32_t num_clips;
    uint64_t clips_ptr;
};

#define DRM_MODE_FB_DIRTY_ANNOTATE_COPY 0x01
#define DRM_MODE_FB_DIRTY_FLAGS         0x03

static int fail_stage = 0;
static void stage(int s) {
    fail_stage = s;
    char buf[32];
    int n = snprintf(buf, sizeof buf, "[damage] stage %d\n", s);
    if (n > 0) write(1, buf, (size_t)n);
}
#define STAGE(s) stage(s)
#define CHECK(cond) do { if (!(cond)) { printf("DAMAGE_PROBE_FAIL stage=%d errno=%d\n", fail_stage, errno); return 1; } } while(0)

int main(void) {
    STAGE(0);
    int fd = open("/dev/dri/card0", O_RDWR);
    CHECK(fd >= 0);

    STAGE(1);
    struct drm_mode_card_res res = {0};
    uint32_t ids[64], conns[64], fbs[64], encs[64];
    res.crtc_id_ptr=(uintptr_t)ids; res.connector_id_ptr=(uintptr_t)conns;
    res.fb_id_ptr=(uintptr_t)fbs; res.encoder_id_ptr=(uintptr_t)encs;
    res.count_crtcs=64; res.count_connectors=64; res.count_fbs=64; res.count_encoders=64;
    CHECK(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) == 0);

    /* create dumb A (1280x800, 32bpp) and ADDFB */
    struct drm_mode_create_dumb c = {0}; c.width=1280; c.height=800; c.bpp=32;
    STAGE(2); CHECK(ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &c) == 0);
    struct drm_mode_fb_cmd fb = {0}; fb.width=c.width; fb.height=c.height;
    fb.pitch=c.pitch; fb.bpp=c.bpp; fb.depth=24; fb.handle=c.handle;
    STAGE(3); CHECK(ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) == 0);
    uint32_t fb_id = fb.fb_id;

    /* no clips, no annotate → no-op success */
    {
        struct drg_drm_mode_fb_dirty_cmd d = {0};
        d.fb_id=fb_id; d.num_clips=0; d.flags=0;
        STAGE(4); CHECK(ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &d) == 0);
    }
    /* 2 clips (not annotate_copy) → files damage */
    {
        struct drg_drm_clip_rect clips[2] = { {0,0,100,100}, {150,100,10,10} };
        struct drg_drm_mode_fb_dirty_cmd d = {0};
        d.fb_id=fb_id; d.num_clips=2; d.flags=0;
        d.clips_ptr = (uintptr_t)clips;
        STAGE(5); CHECK(ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &d) == 0);
    }
    /* ANNOTATE_COPY + odd clip count → EINVAL */
    {
        struct drg_drm_clip_rect clips[3] = { {0,0,10,10},{10,10,10,10},{20,20,10,10} };
        struct drg_drm_mode_fb_dirty_cmd d = {0};
        d.fb_id=fb_id; d.num_clips=3; d.flags=DRM_MODE_FB_DIRTY_ANNOTATE_COPY;
        d.clips_ptr = (uintptr_t)clips;
        STAGE(6); CHECK(ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &d) == EINVAL);
    }
    /* huge count (>256) → full transfer, success */
    {
        struct drg_drm_mode_fb_dirty_cmd d = {0};
        d.fb_id=fb_id; d.num_clips=300; d.flags=0;
        d.clips_ptr = (uintptr_t)NULL;
        STAGE(7); CHECK(ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &d) == 0);
    }
    /* unknown fb_id → ENOENT */
    {
        struct drg_drm_mode_fb_dirty_cmd d = {0};
        d.fb_id=99999; d.num_clips=0;
        STAGE(8); CHECK(ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &d) == ENOENT);
    }
    /* unreadable clips_ptr (addr at high unreachable) → EFAULT */
    {
        struct drg_drm_clip_rect clips[1] = { {0,0,10,10} };
        struct drg_drm_mode_fb_dirty_cmd d = {0};
        d.fb_id=fb_id; d.num_clips=1; d.flags=0;
        d.clips_ptr = (uintptr_t)0x90000000;
        STAGE(9); CHECK(ioctl(fd, DRM_IOCTL_MODE_DIRTYFB, &d) == EFAULT);
    }
    printf("DAMAGE_PROBE_PASS\n");
    return 0;
}
