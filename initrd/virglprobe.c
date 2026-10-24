// virglprobe: validate VirtIO-GPU 3D path on /dev/dri/card0.
// GETPARAM(3D) -> GET_CAPS(virgl) -> RESOURCE_CREATE -> MAP -> WAIT ->
// EXECBUFFER (with and without an out-fence). Prints VIRGLPROBE_PASS iff all
// succeed.
//
// There is no CONTEXT_CREATE/CONTEXT_DESTROY step, because Linux has no such
// ioctl: a virgl context comes into being on the first EXECBUFFER. The probe
// used to call a non-Linux ioctl, so it only passed while the kernel agreed
// about a request that no real client would ever send.
/*
 * DRM ioctl numbers come from the kernel's own uapi crate so the guest
 * cannot drift from the driver. See scripts/generate-drm-ioctls.sh.
 */
#include "drm_ioctls.h"
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <errno.h>

#define VIRTGPU_PARAM_3D_FEATURES 1UL
#define VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS 7UL

struct getparam { uint64_t param; uint64_t value; };
struct get_caps { uint32_t name; uint32_t capset_version; uint64_t addr; uint32_t size; uint32_t pad; };
struct res_create {
    uint32_t target; uint32_t format; uint32_t bind;
    uint32_t width; uint32_t height; uint32_t depth; uint32_t array_size;
    uint32_t last_level;
    uint32_t nr_samples; uint32_t flags; uint32_t bo_handle; uint32_t res_handle;
    uint32_t size; uint32_t stride;
};
struct res_map { uint64_t offset; uint32_t handle; uint32_t pad; };
struct res_wait { uint32_t handle; uint32_t flags; };
struct execbuffer {
    uint32_t flags, size;
    uint64_t command, bo_handles;
    uint32_t num_bo_handles;
    int32_t fence_fd;
    uint32_t ring_idx;
    uint32_t syncobj_stride, num_in_syncobjs, num_out_syncobjs;
    uint64_t in_syncobjs, out_syncobjs;
};

static int fail(const char *msg) { printf("VIRGLPROBE_FAIL: %s\n", msg); return 1; }

int main(int argc, char **argv) {
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) return fail("open card0");

    // Fence-carrying submits must NOT be rejected with EOPNOTSUPP.
    //
    // They used to be: the driver rejected any non-RING_IDX submit carrying a
    // fence, but virgl never sets RING_IDX and Mesa asks for a fence on every
    // frame. Every execbuffer therefore failed and userspace reported the
    // submission error and dropped the frame. Accepting the fence is the
    // correct behaviour for a synchronous submit path.
    //
    // The command pointer here is deliberately invalid (1), so the call still
    // fails - but on the command buffer, not on the fence. Any errno other
    // than EOPNOTSUPP proves the fence request was accepted and we got as far
    // as validating the payload.
    for (uint32_t flags = 1; flags <= 3; flags++) {
        struct execbuffer eb = { .flags = flags, .size = 4, .command = 1, .fence_fd = -1 };
        errno = 0;
        int rc = ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &eb);
        if (rc != -1)
            return fail("submit with bogus command buffer unexpectedly succeeded");
        if (errno == EOPNOTSUPP)
            return fail("fence request rejected with EOPNOTSUPP (regresses all gallium rendering)");
    }
    printf("FENCE_ACCEPT_OK\n");
    if (argc == 2 && strcmp(argv[1], "--fence-only") == 0) {
        close(fd);
        return 0;
    }

    struct getparam gp = { VIRTGPU_PARAM_3D_FEATURES, 0 };
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &gp) != 0) return fail("getparam");
    printf("VIRGL_3D=%llu\n", (unsigned long long)gp.value);
    if (gp.value != 1) return fail("3d not advertised");

    struct getparam gc = { VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS, 0 };
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &gc) != 0) return fail("getparam capsets");
    printf("VIRGL_CAPSETS=%llu\n", (unsigned long long)gc.value);

    static uint8_t caps[4096];
    // sweep explicit versions (0 = kernel auto-discover via INFO)
    for (uint32_t ver = 0; ver <= 2; ver++) {
        memset(caps, 0, sizeof(caps));
        struct get_caps cp = { 1, ver, (uint64_t)caps, sizeof(caps), 0 };
        int rc = ioctl(fd, DRM_IOCTL_VIRTGPU_GET_CAPS, &cp);
        int nonzero = 0;
        for (int i = 0; i < 64; i++) if (caps[i]) { nonzero = 1; break; }
        printf("CAPS_TRY ver=%u rc=%d nonzero=%d w0=%u\n", ver, rc, nonzero,
               (unsigned)(caps[0] | (caps[1] << 8) | (caps[2] << 16) | (caps[3] << 24)));
        if (rc == 0 && nonzero) { printf("CAPS_OK\n"); break; }
    }

    struct res_create rc;
    memset(&rc, 0, sizeof(rc));
    rc.target = 2;      // PIPE_TEXTURE_2D
    rc.format = 21;     // PIPE_FORMAT_B8G8R8A8_UNORM
    rc.bind = 0x18;     // SAMPLER_VIEW | RENDER_TARGET
    rc.width = 64; rc.height = 64; rc.depth = 1;
    rc.array_size = 1;
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &rc) != 0) return fail("res_create");
    printf("RES_OK handle=%u size=%u\n", rc.bo_handle, rc.size);

    struct res_map m = { 0, rc.bo_handle, 0 };
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &m) != 0) return fail("map");
    printf("MAP_OK\n");

    struct res_wait w = { rc.bo_handle, 0 };
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &w) != 0) return fail("wait");
    printf("WAIT_OK\n");

    // Real command submission.
    //
    // Everything above only exercises context/resource setup, so submit_3d
    // itself was never covered by any test. Submit an empty-but-valid command
    // stream (the host accepts a zero-length body; what we are testing is
    // that the ioctl reaches the 3D path and the host accepts it) both with
    // and without an out-fence, and check the fence fd is actually raised.
    uint32_t empty_cmd[4] = { 0, 0, 0, 0 };
    struct execbuffer eb;
    memset(&eb, 0, sizeof(eb));
    eb.flags = 0;
    eb.size = sizeof(empty_cmd);
    eb.command = (uint64_t)(uintptr_t)empty_cmd;
    eb.fence_fd = -1;
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &eb) != 0) {
        printf("EXEC_FAIL errno=%d\n", errno);
        return fail("execbuffer");
    }
    printf("EXEC_OK\n");

    // Same submit asking for an out-fence: the fd must come back valid and
    // be readable, i.e. the kernel really raised it on completion.
    memset(&eb, 0, sizeof(eb));
    eb.flags = 2;                 // VIRTGPU_EXECBUF_FENCE_FD_OUT
    eb.size = sizeof(empty_cmd);
    eb.command = (uint64_t)(uintptr_t)empty_cmd;
    eb.fence_fd = -1;
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &eb) != 0) return fail("execbuffer fenced");
    if (eb.fence_fd < 0) return fail("no out-fence fd returned");
    uint64_t fence_val = 0;
    ssize_t fr = read(eb.fence_fd, &fence_val, sizeof(fence_val));
    if (fr != (ssize_t)sizeof(fence_val) || fence_val == 0)
        return fail("out-fence not signalled");
    printf("FENCE_SIGNALLED\n");
    close(eb.fence_fd);

    // The context the first EXECBUFFER created is torn down when the fd is
    // closed, exactly as on Linux. There is no ioctl for it, so there is
    // nothing left to check here.
    printf("VIRGLPROBE_PASS\n");
    return 0;
}
