// mesarepro: walk Mesa virgl init sequence step-by-step with flushed prints,
// isolating the aborting call without mywima. Unbuffered output so the log
// pinpoints the dying call even on abort()/crash.
#define _GNU_SOURCE
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
#include <pthread.h>
#include <sys/ioctl.h>
#include <sys/eventfd.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <poll.h>
#include <fcntl.h>
#include <dirent.h>
#include <sys/stat.h>
#include <errno.h>

#define STEP(s) do { printf("STEP: %s\n", s); fflush(stdout); } while (0)
#define OK(s) do { printf("OK: %s\n", s); fflush(stdout); } while (0)

struct version { int major, minor, patch; size_t nlen, dlen, desclen; char *name, *date, *desc; };
struct getcap { uint64_t cap, val; };
struct getparam { uint64_t param; uint64_t value; };
// drm_virtgpu_get_caps: the two 32-bit words come first, then the 64-bit
// buffer pointer. Declaring the pointer first (as this probe used to) makes
// the struct 8-byte aligned, so the compiler inserts 4 bytes of padding after
// `ver` and every field after it is read from the wrong offset — the kernel
// then sees a size of 0 and rejects the query. A 24-byte struct matches
// Linux's.
struct get_caps { uint32_t name; uint32_t capset_version; uint64_t addr; uint32_t size; uint32_t pad; };
struct res_create { uint32_t target, format, bind, width, height, depth, array_size,
                    last_level, nr_samples, flags, bo_handle, res_handle, size, stride; };
struct res_info { uint32_t bo_handle, res_handle, size, blob_mem; };
struct sync_create { uint32_t handle; uint32_t flags; };
struct sync_destroy { uint32_t handle; uint32_t pad; };
struct sync_wait { uint64_t handles; int64_t timeout; uint32_t count; uint32_t flags; uint32_t first; uint32_t pad; uint64_t deadline; };
struct sync_array { uint64_t handles; uint32_t count; uint32_t pad; };

static void *thread_fn(void *a) { (void)a; return (void*)42; }

// ponytail: replicate mywima's exact env, then gbm_create_device (the call
// that aborts under virpipe). Needs -lgbm.
#include <gbm.h>
static int try_gbm_with_mywima_env(int fd) {
    unsetenv("GALLIUM_DRIVER"); // ponytail: "virpipe" selects vtest socket, not DRM; unset auto-picks virtio_gpu
    setenv("LIBGL_DRIVERS_PATH", "/usr/lib/dri", 1);
    setenv("GBM_BACKEND", "dri", 1);
    setenv("GBM_BACKENDS_PATH", "/usr/lib/gbm", 1);
    printf("STEP: gbm_create_device with mywima virpipe env\n");
    struct gbm_device *dev = gbm_create_device(fd);
    if (!dev) { printf("RESULT: gbm_create_device NULL errno=%d\n", errno); return 1; }
    printf("RESULT: gbm backend=%s\n", gbm_device_get_backend_name(dev));
    gbm_device_destroy(dev);
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    printf("MESAREPRO start\n");

    STEP("open card0");
    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) { printf("FAIL open errno=%d\n", errno); return 1; }
    OK("open");

    STEP("version");
    char nbuf[64] = {0}, dbuf[64] = {0}, desc[64] = {0};
    struct version v = {0,0,0,sizeof(nbuf),sizeof(dbuf),sizeof(desc),nbuf,dbuf,desc};
    if (ioctl(fd, DRM_IOCTL_VERSION, &v) != 0) { printf("FAIL version\n"); return 1; }
    printf("OK: version %d.%d name=%s\n", v.major, v.minor, nbuf);

    STEP("getcap loop 1..0x15");
    for (uint64_t c = 1; c <= 0x15; c++) {
        struct getcap gc = {c, 0};
        int rc = ioctl(fd, DRM_IOCTL_GET_CAP, &gc);
        printf("  cap %#lx -> rc=%d val=%llu\n", (unsigned long)c, rc, (unsigned long long)gc.val);
    }
    OK("getcap");

    STEP("getparam loop 1..8");
    for (uint64_t p = 1; p <= 8; p++) {
        struct getparam gp = {p, 0};
        int rc = ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &gp);
        printf("  param %llu -> rc=%d val=%llu\n", (unsigned long long)p, rc, (unsigned long long)gp.value);
    }
    OK("getparam");

    STEP("get_caps virgl");
    static uint8_t caps[4096];
    struct get_caps cp;
    memset(&cp, 0, sizeof(cp));
    cp.name = 1; cp.capset_version = 0; cp.addr = (uint64_t)caps; cp.size = sizeof(caps);
    printf("  get_caps struct size=%zu name=%u ver=%u addr=%llx size=%u\n",
           sizeof(cp), cp.name, cp.capset_version, (unsigned long long)cp.addr, cp.size);
    errno = 0;
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_GET_CAPS, &cp) != 0) { printf("FAIL get_caps errno=%d\n", errno); return 1; }
    printf("OK: caps w0=%u\n", (unsigned)(caps[0]|caps[1]<<8|caps[2]<<16|caps[3]<<24));

    STEP("pthread smoke");
    pthread_t th; void *ret = NULL;
    pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
    pthread_mutex_lock(&mu);
    if (pthread_create(&th, NULL, thread_fn, NULL) != 0) { printf("FAIL pthread_create\n"); return 1; }
    pthread_mutex_unlock(&mu);
    if (pthread_join(th, &ret) != 0 || ret != (void*)42) { printf("FAIL pthread_join\n"); return 1; }
    OK("pthread");

    STEP("eventfd+memfd");
    int efd = eventfd(0, 0);
    if (efd < 0) { printf("FAIL eventfd errno=%d\n", errno); return 1; }
    close(efd);
    int mfd = memfd_create("repro", 0);
    if (mfd < 0) { printf("FAIL memfd errno=%d\n", errno); return 1; }
    close(mfd);
    OK("eventfd+memfd");

    STEP("syncobj create/wait-zero/signal/destroy");
    struct sync_create sc = {0, 0};
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &sc) != 0) { printf("FAIL sync create\n"); return 1; }
    printf("OK: sync handle=%u\n", sc.handle);
    struct sync_wait sw = {(uint64_t)&sc.handle, 0, 1, 0, 0, 0, 0};
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &sw) == 0) { printf("FAIL wait-zero should time out\n"); return 1; }
    printf("OK: wait-zero rc!=0 errno=%d\n", errno);
    struct sync_array sa = {(uint64_t)&sc.handle, 1, 0};
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_SIGNAL, &sa) != 0) { printf("FAIL signal\n"); return 1; }
    sw.timeout = 1000000000LL;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &sw) != 0) { printf("FAIL wait-signaled\n"); return 1; }
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_RESET, &sa) != 0) { printf("FAIL reset\n"); return 1; }
    struct sync_destroy sd = {sc.handle, 0};
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &sd) != 0) { printf("FAIL destroy\n"); return 1; }
    OK("syncobj");

    STEP("resource info");
    // VIRTGPU_RESOURCE_INFO is the real nr 0x45 ioctl a client uses to map a
    // bo_handle onto the host resource it must hand to the renderer. The
    // kernel used to claim 0x45 for a CONTEXT_CREATE that Linux does not have,
    // so this query silently created a context instead of answering.
    {
        struct res_create ri_rc;
        memset(&ri_rc, 0, sizeof(ri_rc));
        ri_rc.target = 2; ri_rc.format = 21; ri_rc.bind = 0x18;
        ri_rc.width = 32; ri_rc.height = 32; ri_rc.depth = 1; ri_rc.array_size = 1;
        if (ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &ri_rc) != 0) { printf("FAIL res_create\n"); return 1; }
        struct res_info ri;
        memset(&ri, 0, sizeof(ri));
        ri.bo_handle = ri_rc.bo_handle;
        if (ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &ri) != 0) { printf("FAIL resource_info\n"); return 1; }
        printf("  resource_info bo=%u res=%u size=%u blob_mem=%u\n",
               ri.bo_handle, ri.res_handle, ri.size, ri.blob_mem);
        if (ri.res_handle == 0 || ri.size == 0) { printf("FAIL resource_info empty\n"); return 1; }
        OK("resource_info");
    }

    STEP("libdrm enum walk (sysfs+udev+dev)");
    const char *dirs[] = {"/sys/dev/char", "/dev/dri", "/sys/class/drm", "/sys/bus/pci/devices", "/run/udev/data", NULL};
    for (int d = 0; dirs[d]; d++) {
        printf("  walk %s\n", dirs[d]);
        DIR *dp = opendir(dirs[d]);
        if (!dp) { printf("  walk %s opendir FAIL errno=%d\n", dirs[d], errno); continue; }
        struct dirent *de; int n = 0;
        while ((de = readdir(dp)) != NULL && n < 40) {
            char full[256];
            snprintf(full, sizeof(full), "%s/%s", dirs[d], de->d_name);
            struct stat st;
            int sr = stat(full, &st);
            printf("    %s dt=%d stat=%d\n", de->d_name, (int)de->d_type, sr);
            n++;
        }
        closedir(dp);
    }
    OK("enum-walk");

    STEP("fcntl dup");
    int dfd = fcntl(fd, 1030, 3);
    if (dfd < 0) { printf("FAIL dup errno=%d\n", errno); return 1; }
    printf("OK: dup -> %d\n", dfd);
    struct version v2 = {0,0,0,sizeof(nbuf),sizeof(dbuf),sizeof(desc),nbuf,dbuf,desc};
    if (ioctl(dfd, DRM_IOCTL_VERSION, &v2) != 0) { printf("FAIL version-on-dup\n"); return 1; }
    OK("version-on-dup");

    STEP("epoll+eventfd fence-thread pattern");
    int efd2 = eventfd(0, 0);
    int epfd = epoll_create1(0);
    if (epfd < 0) { printf("FAIL epoll_create errno=%d\n", errno); return 1; }
    struct epoll_event ev = {1, {.fd = efd2}};
    if (epoll_ctl(epfd, 1, efd2, &ev) != 0) { printf("FAIL epoll_ctl errno=%d\n", errno); return 1; }
    uint64_t one = 1;
    if (write(efd2, &one, 8) != 8) { printf("FAIL efd write\n"); return 1; }
    struct epoll_event out[4];
    int n = epoll_wait(epfd, out, 4, 1000);
    if (n <= 0) { printf("FAIL epoll_wait n=%d errno=%d\n", n, errno); return 1; }
    printf("OK: epoll_wait n=%d\n", n);

    STEP("poll drm fd with timeout (must return 0, not hang)");
    {
        struct pollfd dpfd = {fd, 1, 0};
        struct timespec t0, t1;
        clock_gettime(1, &t0);
        int pr = poll(&dpfd, 1, 200);
        clock_gettime(1, &t1);
        long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
        printf("OK: drm poll rc=%d elapsed=%ldms\n", pr, ms);
        if (pr != 0 || ms < 100 || ms > 2000) { printf("FAIL drm-poll rc=%d ms=%ld\n", pr, ms); return 1; }
    }

    STEP("poll on eventfd");
    struct pollfd pfd = {efd2, 1, 0};
    /* drain first */
    { uint64_t tmp; read(efd2, &tmp, 8); }
    if (write(efd2, &one, 8) != 8) { printf("FAIL efd write2\n"); return 1; }
    n = poll(&pfd, 1, 1000);
    if (n <= 0) { printf("FAIL poll n=%d errno=%d\n", n, errno); return 1; }
    OK("poll");

    if (try_gbm_with_mywima_env(fd) != 0) { printf("FAIL gbm-virpipe\n"); return 1; }
    OK("gbm-virpipe");

    printf("MESAREPRO_PASS\n");
    return 0;
}
