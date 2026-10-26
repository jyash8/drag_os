#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <drm/drm.h>
#include <drm/virtgpu_drm.h>

static int fail_stage = 0;
static void stage(int s) {
    fail_stage = s;
    char buf[32];
    int n = snprintf(buf, sizeof buf, "[ctx] stage %d\n", s);
    if (n > 0) write(1, buf, (size_t)n);
}
#define STAGE(s) stage(s)
#define CHECK(cond) do { if (!(cond)) { printf("CTX_PROBE_FAIL stage=%d errno=%d\n", fail_stage, errno); return 1; } } while(0)

int main(void) {
    STAGE(0);
    int fd = open("/dev/dri/card0", O_RDWR);
    CHECK(fd >= 0);

    /* get_caps on capset 1 (virgl) — info only, may fail on certain GPUs */
    {
        struct drm_virtgpu_get_caps caps = {0};
        void *buf = malloc(1024);
        CHECK(buf != NULL);
        caps.cap_set_id = 1;
        caps.cap_set_ver = 0;
        caps.addr = (uint64_t)buf;
        caps.size = 1024;
        /* errno ignored here because some QEMU builds don't support caps — but we
           still want to test context_init on virgl capset 1 regardless. */
        ioctl(fd, DRM_IOCTL_VIRTGPU_GET_CAPS, &caps);
    }

    uint32_t valid_ids[3] = { 1, 2, 4 };  /* virgl, virgl2, venus */
    int n_valid = 0;
    for (int i = 0; i < 3; i++) {
        STAGE(2 + i);
        struct drm_virtgpu_context_set_param params[2] = {
            { VIRTGPU_CONTEXT_PARAM_CAPSET_ID, valid_ids[i] },
            { VIRTGPU_CONTEXT_PARAM_DEBUG_NAME, (uint64_t)"probectx" },
        };
        struct drm_virtgpu_context_init init = {0};
        init.num_params = 2;
        init.pad = 0;
        init.ctx_set_params = (uint64_t)params;
        int rc = ioctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &init);
        if (rc != 0) {
            printf("CTX_PROBE_FAIL ctx_init(capset=%u) rc=%d errno=%d\n", valid_ids[i], rc, errno);
            return 1;
        }
        printf("[ctx] context_init capset=%d -> rc=0\n", valid_ids[i]);
        n_valid++;
    }
    CHECK(n_valid == 3);

    /* unknown capsets must return -EINVAL (22) */
    uint32_t unknown_ids[5] = { 0, 3, 5, 6, 999 };
    for (int i = 0; i < 5; i++) {
        STAGE(10 + i);
        struct drm_virtgpu_context_set_param params[2] = {
            { VIRTGPU_CONTEXT_PARAM_CAPSET_ID, unknown_ids[i] },
            { VIRTGPU_CONTEXT_PARAM_DEBUG_NAME, (uint64_t)"badctx" },
        };
        struct drm_virtgpu_context_init init = {0};
        init.num_params = 2;
        init.pad = 0;
        init.ctx_set_params = (uint64_t)params;
        int rc = ioctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &init);
        if (rc == 0) {
            printf("CTX_PROBE_FAIL capset=%u succeeded unexpectedly\n", unknown_ids[i]);
            return 1;
        }
        if (errno != 22) {
            printf("CTX_PROBE_FAIL capset=%u errno=%d (expected 22)\n", unknown_ids[i], errno);
            return 1;
        }
    }

    close(fd);
    printf("CTX_PROBE_PASS\n");
    return 0;
}
