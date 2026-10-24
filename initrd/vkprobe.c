// vkprobe: Venus Vulkan path check on /dev/dri/card0.
// 1) raw: GETPARAM capsets mask -> GET_CAPS(venus id=4) must be non-empty.
// 2) loader: vkCreateInstance + vkEnumeratePhysicalDevices, print names.
// Prints VKPROBE_DONE always (inspect VENUS_* lines for status).
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
#include <stdlib.h>
#include <sys/ioctl.h>
#include <errno.h>
#include <signal.h>
#include <execinfo.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

static void onfatal(int s) {
    void *bt[32];
    int n = backtrace(bt, 32);
    backtrace_symbols_fd(bt, n, STDERR_FILENO);
    _exit(100 + s - 6);
}

#define VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS 7UL
#define VIRTGPU_CAPSET_VENUS 4UL
#define VIRTGPU_CONTEXT_PARAM_CAPSET_ID 1UL
#define VIRTGPU_CONTEXT_PARAM_DEBUG_NAME 4UL
#define VIRTGPU_CONTEXT_PARAM_NUM_RINGS 2UL
#define VIRTGPU_BLOB_MEM_GUEST 1UL

struct getparam { uint64_t param; uint64_t value; };
struct get_caps { uint32_t name; uint32_t capset_version; uint64_t addr; uint32_t size; uint32_t pad; };
struct res_map { uint64_t offset; uint32_t handle; uint32_t pad; };
/* drm_virtgpu_execbuffer, as the kernel's uapi crate lays it out (64 bytes,
 * no pad between ring_idx and syncobj_stride -- see drm-uapi's note). */
struct virtgpu_execbuffer {
    uint32_t flags; uint32_t size; uint64_t command; uint64_t bo_handles;
    uint32_t num_bo_handles; int32_t fence_fd; uint32_t ring_idx;
    uint32_t syncobj_stride; uint32_t num_in_syncobjs; uint32_t num_out_syncobjs;
    uint64_t in_syncobjs; uint64_t out_syncobjs;
};
#define VIRTGPU_EXECBUF_FENCE_FD_OUT 0x02
#define VIRTGPU_EXECBUF_RING_IDX      0x04

struct ctx_param { uint64_t param; uint64_t value; };
struct ctx_init { uint32_t num_params; uint32_t pad; uint64_t params; };
struct blob_create {
    uint32_t blob_mem; uint32_t blob_flags; uint32_t bo_handle; uint32_t res_handle;
    uint64_t size; uint32_t pad; uint32_t cmd_size; uint64_t cmd; uint64_t blob_id;
    uint32_t blob_hints; uint32_t pad2;
};

int main(void) {
    int ring1_fd = -1;
    setvbuf(stdout, NULL, _IONBF, 0);
    setvbuf(stderr, NULL, _IONBF, 0);
    signal(SIGABRT, onfatal);
    signal(SIGSEGV, onfatal);
    printf("VKPROBE_ALIVE\n");
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { printf("VKPROBE_FAIL: open card0\n"); printf("VKPROBE_DONE\n"); return 1; }

    struct getparam gp = { VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS, 0 };
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &gp) != 0) {
        printf("VKPROBE_FAIL: getparam capsets\n");
        printf("VKPROBE_DONE\n");
        close(fd);
        return 1;
    }
    printf("VENUS_MASK=%llu advertised=%d\n",
           (unsigned long long)gp.value, !!(gp.value & (1ULL << VIRTGPU_CAPSET_VENUS)));

    static unsigned char caps[4096];
    struct get_caps gc = { VIRTGPU_CAPSET_VENUS, 0, (uint64_t)(uintptr_t)caps, sizeof(caps), 0 };
    memset(caps, 0, sizeof(caps));
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_GET_CAPS, &gc) != 0) {
        printf("VENUS_CAPS=FAIL errno=%d\n", errno);
    } else {
        int nonzero = 0;
        for (size_t i = 0; i < sizeof(caps); i++) if (caps[i]) { nonzero = 1; break; }
        printf("VENUS_CAPS=OK nonzero=%d w0=%u w1=%u\n", nonzero,
               ((uint32_t *)caps)[0], ((uint32_t *)caps)[1]);
    }
    close(fd);

    /* step 1b: raw Venus context + blob round-trip (no Mesa involved). */
    static const char name[] = "vkprobe";
    /* A Venus client declares how many rings it has; the WSI memory-signal
     * submit goes on the "out" ring (index 1). The kernel used to ignore
     * VIRTGPU_CONTEXT_PARAM_NUM_RINGS and validate against a single ring, so
     * every out-ring submit was rejected with -EINVAL and the client lost the
     * device on its first frame. This probe submits on ring 1 for exactly
     * that reason. */
    struct ctx_param params[3] = {
        { VIRTGPU_CONTEXT_PARAM_CAPSET_ID, VIRTGPU_CAPSET_VENUS },
        { VIRTGPU_CONTEXT_PARAM_NUM_RINGS, 2 },
        { VIRTGPU_CONTEXT_PARAM_DEBUG_NAME, (uint64_t)(uintptr_t)name },
    };
    struct ctx_init cinit = { 3, 0, (uint64_t)(uintptr_t)params };
    fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) {
        printf("VENUS_RAW=FAIL open\n");
    } else {
        errno = 0;
        long ctx = ioctl(fd, DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &cinit);
        /* NOTE: kernel returns 0 on success (Mesa ABI); ctx ids stay inside. */
        printf("VENUS_CTXINIT=%ld errno=%d\n", ctx, errno);
        {
            struct blob_create bc;
            memset(&bc, 0, sizeof(bc));
            bc.blob_mem = VIRTGPU_BLOB_MEM_GUEST;
            bc.blob_flags = 0x1; /* MAPPABLE */
            bc.size = 65536;
            errno = 0;
            int brc = ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE_BLOB, &bc);
            printf("VENUS_BLOB=%d errno=%d bo=%u res=%u id=%llu\n", brc, errno,
                   bc.bo_handle, bc.res_handle, (unsigned long long)bc.blob_id);
            if (brc == 0) {
                struct res_map m = { 0, bc.bo_handle, 0 };
                errno = 0;
                int mrc = ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &m);
                printf("VENUS_MAP=%d errno=%d off=0x%llx\n", mrc, errno,
                       (unsigned long long)m.offset);
            }
        }
        ring1_fd = fd;
    }

    printf("VK_MARK_A\n");
    uint32_t loader_ver = 0;
    vkEnumerateInstanceVersion(&loader_ver);
    printf("VK_LOADER=%u.%u.%u\n", VK_VERSION_MAJOR(loader_ver),
           VK_VERSION_MINOR(loader_ver), VK_VERSION_PATCH(loader_ver));

    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "vkprobe",
        .apiVersion = VK_API_VERSION_1_0,
    };
    VkInstanceCreateInfo ci = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
    };
    VkInstance inst;
    VkResult res = vkCreateInstance(&ci, NULL, &inst);
    if (res != VK_SUCCESS) {
        printf("VKPROBE_FAIL: create instance res=%d\n", (int)res);
        printf("VKPROBE_DONE\n");
        return 1;
    }
    uint32_t n = 0;
    vkEnumeratePhysicalDevices(inst, &n, NULL);
    printf("VK_DEVICES=%u\n", n);
    if (n > 0) {
        VkPhysicalDevice *devs = malloc(n * sizeof(*devs));
        if (devs && vkEnumeratePhysicalDevices(inst, &n, devs) == VK_SUCCESS) {
            for (uint32_t i = 0; i < n; i++) {
                VkPhysicalDeviceProperties p;
                vkGetPhysicalDeviceProperties(devs[i], &p);
                printf("VK_DEV%d name='%s' vendor=%04x device=%04x\n",
                       i, p.deviceName, p.vendorID, p.deviceID);
                if (strstr(p.deviceName, "enus") || strstr(p.deviceName, "irtio"))
                    printf("VENUS_FOUND=1\n");
            }
        }
        free(devs);
    }
    /* The out-ring submit, last.
     *
     * This is the shape the Venus WSI uses to signal a swapchain image's
     * memory: no buffer objects, a small command, and the ring index the
     * client declared. The kernel used to validate the ring index against a
     * single ring, so every such submit was rejected with -EINVAL and the
     * client lost the device on its first frame.
     *
     * It runs last because the payload here is not a real Venus command, so
     * the host rejects the command *content* and flags the context. The
     * point of the check is the kernel's ring validation, which happens
     * before the command reaches the host -- but a poisoned context must not
     * be able to affect the checks above it.
     */
    if (ring1_fd >= 0) {
        uint32_t cmd[6];
        memset(cmd, 0, sizeof(cmd));
        struct virtgpu_execbuffer ex;
        memset(&ex, 0, sizeof(ex));
        ex.flags = VIRTGPU_EXECBUF_RING_IDX | VIRTGPU_EXECBUF_FENCE_FD_OUT;
        ex.size = sizeof(cmd);
        ex.command = (uint64_t)(uintptr_t)cmd;
        ex.ring_idx = 1;
        ex.fence_fd = -1;
        errno = 0;
        int erc = ioctl(ring1_fd, DRM_IOCTL_VIRTGPU_EXECBUFFER, &ex);
        if (erc == 0) {
            printf("VENUS_RING1_OK\n");
            if (ex.fence_fd >= 0) close(ex.fence_fd);
        } else {
            printf("VENUS_RING1_FAIL errno=%d\n", errno);
        }
        close(ring1_fd);
    }

    printf("VKPROBE_DONE\n");
    return 0;
}
