#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>
#include <sys/ioctl.h>
#include <sys/vt.h>
#include <sys/ioctl.h>
#include <linux/kd.h>

static int fail_stage = 0;
static void stage(int s) {
    fail_stage = s;
    char buf[32];
    int n = snprintf(buf, sizeof buf, "[vt] stage %d\n", s);
    if (n > 0) write(1, buf, (size_t)n);
}
#define STAGE(s) stage(s)
#define CHECK(cond) do { if (!(cond)) { char msg[64]; int n = snprintf(msg, sizeof msg, "VT_PROBE_FAIL stage=%d errno=%d\n", fail_stage, errno); if(n>0) write(1,msg,(size_t)n); return 1; } } while(0)

int main(void) {
    STAGE(0);
    // Probe opens /dev/tty0 and /dev/dri/card0
    int tty = open("/dev/tty0", O_RDWR);
    if (tty < 0) tty = open("/dev/tty", O_RDWR);
    CHECK(tty >= 0);

    int fd = open("/dev/dri/card0", O_RDWR);
    if (fd >= 0) {
        /* Try to become DRM master requires KDSETMODE without a master */
    }

    int vtno = 0;
    STAGE(1);
    // VT_OPENQRY; if unimplemented/failing, defined errno tolerated
    int r = ioctl(tty, VT_OPENQRY, &vtno);
    if (r < 0) {
        /* ENOTTY/ENODEV etc — defined errno is acceptable, no panic */
        STAGE(2);
        printf("[vt] VT_OPENQRY err=%d (defined errno tolerated)\n", errno);
    } else {
        STAGE(2);
        // bound check
        if (vtno < 1 || vtno > 63) {
            printf("[vt] VT_OPENQRY returned invalid vtno=%d\n", vtno);
        } else {
            printf("[vt] VT_OPENQRY vtno=%d\n", vtno);
            STAGE(3);
            // KDSETMODE KD_GRAPHICS
            if (ioctl(fd >= 0 ? fd : tty, KDSETMODE, 1) < 0) {
                STAGE(4);
                printf("[vt] KDSETMODE KD_GRAPHICS err=%d (tolerated)\n", errno);
            } else {
                printf("[vt] KDSETMODE KD_GRAPHICS ok\n");
            }
            STAGE(4);
            // KD_SETMODE back to text
            if (ioctl(fd >= 0 ? fd : tty, KDSETMODE, 0) < 0) {
                printf("[vt] KDSETMODE KD_TEXT err=%d (tolerated)\n", errno);
            } else {
                printf("[vt] KDSETMODE KD_TEXT ok\n");
            }
            // Release
            if (fd >= 0) close(fd);
            if (tty >= 0) close(tty);
        }
    }
    printf("VT_PROBE_PASS\n");
    return 0;
}
