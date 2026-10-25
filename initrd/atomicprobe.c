#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/poll.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

// DragOS-local event constants (must match kernel drivers/drm/mod.rs).
#define DRG_DRM_EVENT_VBLANK 0x01
#define DRG_DRM_EVENT_FLIP_COMPLETE 0x02

struct drg_event_vblank {
    uint32_t event_type;   // 0x01 VBLANK, 0x02 FLIP_COMPLETE
    uint32_t length;       // sizeof = 32
    uint64_t user_data;    // client's user_data from page_flip
    uint32_t tv_sec;
    uint32_t tv_usec;
    uint32_t sequence;
    uint32_t crtc_id;
};

// Property IDs exposed by the kernel atomic handler (drivers/drm/mod.rs PROP_*).
#define DRG_PROP_FB_ID    2
#define DRG_PROP_CRTC_ID  3
#define DRG_PROP_CRTC_X   4
#define DRG_PROP_CRTC_Y   5
#define DRG_PROP_CRTC_W   6
#define DRG_PROP_CRTC_H   7
#define DRG_PROP_SRC_X    8
#define DRG_PROP_SRC_Y    9
#define DRG_PROP_SRC_W   10
#define DRG_PROP_SRC_H   11
#define DRG_PROP_ACTIVE  12
#define DRG_PROP_MODE_ID 13

#define DRG_ATOMIC_TEST_ONLY 0x100
#define DRG_PAGE_FLIP_EVENT  0x01

// Local wire-format structs matching the kernel UAPI (drivers/drm/mod.rs,
// drm-uapi/src/lib.rs). Host musl headers predate these APIs so we define
// them locally with the exact field order/size the kernel reads.
struct drg_drm_mode_crtc {
    uint64_t set_connectors_ptr;
    uint32_t count_connectors;
    uint32_t crtc_id;
    uint32_t fb_id;
    uint32_t x;
    uint32_t y;
    uint32_t gamma_size;
    uint32_t mode_valid;
    /* mode info follows, not needed here */
};

struct drg_drm_mode_page_flip {
    uint32_t crtc_id;
    uint32_t fb_id;
    uint32_t flags;
    uint32_t reserved;
    uint64_t user_data;
};

#include <unistd.h>
static int fail_stage = 0;
static void stage(int s) {
    fail_stage = s;
    char buf[32];
    int n = snprintf(buf, sizeof buf, "[atomic] stage %d\n", s);
    if (n > 0) write(1, buf, (size_t)n);
}
#define STAGE(s) stage(s)
#define CHECK(cond) do {                                    \
    if (!(cond)) {                                          \
        char msg[96];                                       \
        int n = snprintf(msg, sizeof msg,                   \
            "ATOMIC_PROBE_FAIL stage=%d line=%d errno=%d\n", \
            fail_stage, __LINE__, errno);                   \
        if (n > 0) write(1, msg, (size_t)n);                \
        return 1;                                           \
    }                                                       \
} while (0)

int main(void) {
    STAGE(0);
    int fd = open("/dev/dri/card0", O_RDWR);
    CHECK(fd >= 0);

    /* ---- 1. Get resources: crtc + connector ids ---- */
    {
        struct drm_mode_card_res res = {0};
        uint32_t fb_ids[64], crtc_ids[64], conn_ids[64], enc_ids[64];
        res.fb_id_ptr = (uintptr_t)fb_ids;
        res.crtc_id_ptr = (uintptr_t)crtc_ids;
        res.connector_id_ptr = (uintptr_t)conn_ids;
        res.encoder_id_ptr = (uintptr_t)enc_ids;
        res.count_fbs = 64; res.count_crtcs = 64;
        res.count_connectors = 64; res.count_encoders = 64;
        STAGE(1);
        CHECK(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) == 0);
        STAGE(2);
        CHECK(res.count_crtcs > 0 && res.count_connectors > 0);
        uint32_t crtc_id = crtc_ids[0];
        uint32_t conn_id = conn_ids[0];

        /* ---- 2. Plane resources ---- */
        {
            struct drm_mode_get_plane_res pres = {0};
            uint32_t plane_ids[64];
            pres.plane_id_ptr = (uintptr_t)plane_ids;
            pres.count_planes = 64;
            STAGE(3);
            CHECK(ioctl(fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &pres) == 0);
            STAGE(4);
            CHECK(pres.count_planes > 0);
            uint32_t plane_id = plane_ids[0];

            /* ---- 3. Create + map dumb buffer ---- */
            {
                struct drm_mode_create_dumb c = {0};
                c.width = 1280; c.height = 800; c.bpp = 32;
                STAGE(5);
                CHECK(ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &c) == 0);
                uint32_t handle = c.handle;

                struct drm_mode_map_dumb m = {0};
                m.handle = c.handle;
                STAGE(6);
                CHECK(ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &m) == 0);

                /* ---- 4. Add FB ---- */
                {
                    struct drm_mode_fb_cmd fb = {0};
                    fb.width = c.width; fb.height = c.height;
                    fb.pitch = c.pitch; fb.bpp = c.bpp; fb.depth = 24;
                    fb.handle = c.handle;
                    STAGE(7);
                    CHECK(ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) == 0);
                    uint32_t fb_id = fb.fb_id;

                    /* ---- 5. TEST_ONLY atomic commit (must not mutate) ---- */
                    {
                        uint32_t objs[3] = {plane_id, crtc_id, conn_id};
                        uint32_t nprops[3] = {10, 2, 1};
                        uint32_t props[13];
                        uint64_t values[13];
                        int pi = 0;
                        props[pi]=DRG_PROP_FB_ID;          values[pi++]=fb_id;
                        props[pi]=DRG_PROP_CRTC_ID;        values[pi++]=crtc_id;
                        props[pi]=DRG_PROP_CRTC_X;         values[pi++]=0;
                        props[pi]=DRG_PROP_CRTC_Y;         values[pi++]=0;
                        props[pi]=DRG_PROP_CRTC_W;         values[pi++]=1280;
                        props[pi]=DRG_PROP_CRTC_H;         values[pi++]=800;
                        props[pi]=DRG_PROP_SRC_X;          values[pi++]=0;
                        props[pi]=DRG_PROP_SRC_Y;          values[pi++]=0;
                        props[pi]=DRG_PROP_SRC_W;          values[pi++]=1280u<<16;
                        props[pi]=DRG_PROP_SRC_H;          values[pi++]=800u<<16;
                        props[pi]=DRG_PROP_ACTIVE;         values[pi++]=1;
                        props[pi]=DRG_PROP_MODE_ID;        values[pi++]=0;
                        props[pi]=DRG_PROP_CRTC_ID;        values[pi++]=crtc_id;
                        CHECK(pi == 13);

                        struct drm_mode_atomic a = {0};
                        a.flags = DRG_ATOMIC_TEST_ONLY;
                        a.count_objs = 3;
                        a.objs_ptr = (uintptr_t)objs;
                        a.count_props_ptr = (uintptr_t)nprops;
                        a.props_ptr = (uintptr_t)props;
                        a.prop_values_ptr = (uintptr_t)values;
                        STAGE(8);
                        CHECK(ioctl(fd, DRM_IOCTL_MODE_ATOMIC, &a) == 0);
                    }

                    /* ---- 6. READ BACK: crtc.fb_id must still be 0 (TEST_ONLY no-op) ---- */
                    {
                        struct drg_drm_mode_crtc crtc = {0};
                        crtc.crtc_id = crtc_id;
                        STAGE(9);
                        CHECK(ioctl(fd, DRM_IOCTL_MODE_GETCRTC, &crtc) == 0);
                        STAGE(10);
                        CHECK(crtc.fb_id == 0); /* TEST_ONLY must not have applied fb */
                    }

                    /* ---- 7. REAL atomic commit (applies state) ---- */
                    {
                        uint32_t objs[3] = {plane_id, crtc_id, conn_id};
                        uint32_t nprops[3] = {10, 2, 1};
                        uint32_t props[13];
                        uint64_t values[13];
                        int pi = 0;
                        props[pi]=DRG_PROP_FB_ID;          values[pi++]=fb_id;
                        props[pi]=DRG_PROP_CRTC_ID;        values[pi++]=crtc_id;
                        props[pi]=DRG_PROP_CRTC_X;         values[pi++]=0;
                        props[pi]=DRG_PROP_CRTC_Y;         values[pi++]=0;
                        props[pi]=DRG_PROP_CRTC_W;         values[pi++]=1280;
                        props[pi]=DRG_PROP_CRTC_H;         values[pi++]=800;
                        props[pi]=DRG_PROP_SRC_X;          values[pi++]=0;
                        props[pi]=DRG_PROP_SRC_Y;          values[pi++]=0;
                        props[pi]=DRG_PROP_SRC_W;          values[pi++]=1280u<<16;
                        props[pi]=DRG_PROP_SRC_H;          values[pi++]=800u<<16;
                        props[pi]=DRG_PROP_ACTIVE;         values[pi++]=1;
                        props[pi]=DRG_PROP_MODE_ID;        values[pi++]=0;
                        props[pi]=DRG_PROP_CRTC_ID;        values[pi++]=crtc_id;
                        CHECK(pi == 13);

                        struct drm_mode_atomic a = {0};
                        a.flags = 0; /* real commit */
                        a.count_objs = 3;
                        a.objs_ptr = (uintptr_t)objs;
                        a.count_props_ptr = (uintptr_t)nprops;
                        a.props_ptr = (uintptr_t)props;
                        a.prop_values_ptr = (uintptr_t)values;
                        STAGE(11);
                        CHECK(ioctl(fd, DRM_IOCTL_MODE_ATOMIC, &a) == 0);
                    }

                    /* ---- 8. READ BACK: crtc.fb_id must now equal fb_id ---- */
                    {
                        struct drg_drm_mode_crtc crtc = {0};
                        crtc.crtc_id = crtc_id;
                        STAGE(12);
                        CHECK(ioctl(fd, DRM_IOCTL_MODE_GETCRTC, &crtc) == 0);
                        STAGE(13);
                        CHECK(crtc.fb_id == fb_id);
                    }

                    /* ---- 9. Page flip with event ---- */
                    {
                        struct drg_drm_mode_page_flip pf = {0};
                        pf.crtc_id = crtc_id;
                        pf.fb_id = fb_id;
                        pf.flags = DRG_PAGE_FLIP_EVENT;
                        pf.reserved = 0;
                        pf.user_data = (uint64_t)0xDEADBEEF;
                        STAGE(14);
                        CHECK(ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &pf) == 0);

                        /* ---- 10. Poll + read flip-complete event ---- */
                        {
                            struct pollfd pfd = {fd, POLLIN, 0};
                            STAGE(15);
                            CHECK(poll(&pfd, 1, 5000) > 0);
                            char buf[128];
                            STAGE(16);
                            ssize_t n = read(fd, buf, sizeof buf);
                            CHECK(n >= 32);
                            int found_flip_complete = 0;
                            struct drg_event_vblank *ev = (struct drg_event_vblank *)buf;
                            int count = n / sizeof(struct drg_event_vblank);
                            for (int i = 0; i < count; i++) {
                                if (ev[i].event_type == DRG_DRM_EVENT_FLIP_COMPLETE
                                    && ev[i].user_data == (uint64_t)0xDEADBEEF) {
                                    found_flip_complete = 1;
                                }
                            }
                            STAGE(17);
                            CHECK(found_flip_complete);
                        }
                    }
                }
            }
        }
    }

    printf("ATOMIC_PROBE_PASS\n");
    return 0;
}
