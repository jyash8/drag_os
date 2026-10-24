#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <gbm.h>

int main(void) {
    // Exercise non-static glibc dynamic loading + libgbm backend selection.
    // We intentionally do NOT force GBM_BACKEND so we test the default working path.

    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd < 0) {
        fprintf(stderr, "FAIL: open /dev/dri/card0 -> %d errno=%d\n", fd, errno);
        return 1;
    }

    errno = 0;
    struct gbm_device *dev = gbm_create_device(fd);
    if (!dev) {
        fprintf(stderr, "FAIL: gbm_create_device -> NULL errno=%d\n", errno);
        close(fd);
        return 2;
    }

    const char *backend = gbm_device_get_backend_name(dev);
    printf("OK: gbm_create_device succeeded, backend=%s\n",
           backend ? backend : "(null)");

    gbm_device_destroy(dev);
    close(fd);

    printf("GBM_PASS\n");
    return 0;
}
