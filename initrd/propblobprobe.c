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

// Property IDs exposed by the kernel atomic handler (drivers/drm/mod.rs PROP_*).
#define DRG_PROP_FB_ID   2
#define DRG_PROP_CRTC_ID 3
#define DRG_PROP_ACTIVE  12
#define DRG_PROP_MODE_ID 13

static int fail_stage = 0;
static void stage(int s) {
    fail_stage = s;
    char buf[32];
    int n = snprintf(buf, sizeof buf, "[propblob] stage %d\n", s);
    if (n > 0) write(1, buf, (size_t)n);
}
#define STAGE(s) stage(s)
#define CHECK(cond) do {                                    \
    if (!(cond)) {                                          \
        char msg[96];                                       \
        int n = snprintf(msg, sizeof msg,                   \
            "PROPBLOB_PROBE_FAIL stage=%d line=%d errno=%d\n", \
            fail_stage, __LINE__, errno);                   \
        if (n > 0) write(1, msg, (size_t)n);                \
        return 1;                                           \
    }                                                       \
} while (0)

// Local wire-format structs matching the kernel UAPI. Host musl headers may
// predate these APIs so we define them locally with exact field order/size.
struct drg_drm_mode_create_blob {
    uint64_t data;
    uint32_t length;
    uint32_t blob_id;
};
struct drg_drm_mode_destroy_blob {
    uint32_t blob_id;
};
struct drg_drm_mode_get_prop_blob {
    uint32_t blob_id;
    uint32_t length;
    uint64_t data;
};
struct drg_drm_mode_get_property {
    uint64_t values_ptr;
    uint64_t enum_blob_ptr;
    uint32_t prop_id;
    uint32_t flags;
    char name[32];
    uint32_t count_values;
    uint32_t count_enum_blobs;
};

int main(void) {
    STAGE(0);
    int fd = open("/dev/dri/card0", O_RDWR);
    CHECK(fd >= 0);

    /* ---- 1. Get resources → crtc id ---- */
    {
        struct drm_mode_card_res res = {0};
        uint32_t crtc_ids[64], conn_ids[64], fb_ids[64], enc_ids[64];
        res.crtc_id_ptr = (uintptr_t)crtc_ids;
        res.connector_id_ptr = (uintptr_t)conn_ids;
        res.fb_id_ptr = (uintptr_t)fb_ids;
        res.encoder_id_ptr = (uintptr_t)enc_ids;
        res.count_crtcs = 64; res.count_connectors = 64;
        res.count_fbs = 64; res.count_encoders = 64;
        STAGE(1);
        CHECK(ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) == 0);
        STAGE(2);
        CHECK(res.count_crtcs > 0);
        uint32_t crtc_id = crtc_ids[0];

        /* ---- 2. Property GET: scan prop IDs 0..31 on the crtc object ---- */
        {
            int found_targets = 0;
            for (uint32_t prop_id = 0; prop_id < 32; prop_id++) {
                struct drg_drm_mode_get_property gp = {0};
                gp.prop_id = prop_id;
                gp.count_values = 0;
                gp.count_enum_blobs = 0;
                STAGE(3 + prop_id);
                if (ioctl(fd, DRM_IOCTL_MODE_GETPROPERTY, &gp) != 0)
                    continue; /* prop_id not defined on this object */
                if (gp.name[0] == '\0')
                    continue;
                /* Record if we found one of our expected properties */
                if (prop_id == DRG_PROP_FB_ID ||
                    prop_id == DRG_PROP_CRTC_ID ||
                    prop_id == DRG_PROP_ACTIVE ||
                    prop_id == DRG_PROP_MODE_ID) {
                    found_targets++;
                    /* Verify sensible flags */
                    STAGE(35 + prop_id);
                    CHECK(gp.flags != 0);
                }
            }
            STAGE(35);
            CHECK(found_targets >= 1); /* at least one expected property present */
        }

        /* ---- 3. Create prop blob (16 bytes) ---- */
        {
            const char payload[16] = "propblob-test-16";
            struct drg_drm_mode_create_blob cb = {0};
            cb.data = (uintptr_t)payload;
            cb.length = 16;
            STAGE(36);
            CHECK(ioctl(fd, DRM_IOCTL_MODE_CREATEPROPBLOB, &cb) == 0);
            STAGE(37);
            CHECK(cb.blob_id != 0);
            uint32_t blob_id = cb.blob_id;

            /* ---- 4. Get prop blob back → length 16, data matches ---- */
            {
                uint8_t readback[16] = {0};
                struct drg_drm_mode_get_prop_blob gb = {0};
                gb.blob_id = blob_id;
                gb.length = sizeof(readback);
                gb.data = (uintptr_t)readback;
                STAGE(38);
                CHECK(ioctl(fd, DRM_IOCTL_MODE_GETPROPBLOB, &gb) == 0);
                STAGE(39);
                CHECK(gb.length == 16);
                STAGE(40);
                CHECK(memcmp(readback, payload, 16) == 0);
            }

            /* ---- 5. Destroy prop blob ---- */
            {
                struct drg_drm_mode_destroy_blob db = {0};
                db.blob_id = blob_id;
                STAGE(41);
                CHECK(ioctl(fd, DRM_IOCTL_MODE_DESTROYPROPBLOB, &db) == 0);
            }

            /* ---- 6. Get prop blob again → length 0 (destroyed) ---- */
            {
                uint8_t readback[16] = {0xAA};
                struct drg_drm_mode_get_prop_blob gb = {0};
                gb.blob_id = blob_id;
                gb.length = sizeof(readback);
                gb.data = (uintptr_t)readback;
                STAGE(42);
                CHECK(ioctl(fd, DRM_IOCTL_MODE_GETPROPBLOB, &gb) == 0);
                STAGE(43);
                CHECK(gb.length == 0); /* blob gone */
            }
        }
    }

    printf("PROPBLOB_PROBE_PASS\n");
    return 0;
}
