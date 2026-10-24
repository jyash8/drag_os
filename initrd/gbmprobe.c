#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <linux/version.h>
#include <drm/drm.h>
#include <drm/drm_mode.h>

/* gbm.h not available in musl toolchain; probe without gbm_create_device */

int main(void) {
    int fd = open("/dev/dri/card0", O_RDWR);
    printf("GBMPROBE card0 -> %d errno=%d\n", fd, fd < 0 ? errno : 0);
    if (fd < 0) return 1;

    struct stat st;
    if (fstat(fd, &st) == 0) {
        printf("GBMPROBE fstat ok, mode=%o rdev=0x%lx\n", st.st_mode, st.st_rdev);
    }

    // DRM version ioctl
    struct drm_version v = {0};
    v.name_len = 64;
    v.date_len = 64;
    v.desc_len = 64;
    char name[64], date[64], desc[64];
    v.name = name;
    v.date = date;
    v.desc = desc;
    if (ioctl(fd, DRM_IOCTL_VERSION, &v) == 0) {
        printf("GBMPROBE DRM version %d.%d.%d name='%s' desc='%s'\n",
               v.version_major, v.version_minor, v.version_patchlevel, name, desc);
    }

    // Check render node
    int rfd = open("/dev/dri/renderD128", O_RDWR);
    printf("GBMPROBE renderD128 -> %d errno=%d\n", rfd, rfd < 0 ? errno : 0);
    if (rfd >= 0) close(rfd);

    // Check GBM backend libs exist
    const char *libs[] = {
        "/usr/lib/dri/swrast_dri.so",
        "/usr/lib/dri/virtio_gpu_dri.so",
        "/usr/lib/gbm/gbm_dri.so",
        "/usr/lib/libgallium_dri.so",
    };
    for (int i = 0; i < sizeof(libs)/sizeof(libs[0]); i++) {
        if (access(libs[i], F_OK) == 0) {
            printf("GBMPROBE found %s\n", libs[i]);
        }
    }

    printf("GBMPROBE_PASS\n");
    close(fd);
    return 0;
}