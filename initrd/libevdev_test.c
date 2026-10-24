#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>
#include <poll.h>
#include <libevdev/libevdev.h>

int main(void) {
    int fd = open("/dev/input/event0", O_RDONLY | O_NONBLOCK);
    if (fd < 0) { perror("open"); return 1; }

    struct libevdev *dev = NULL;
    int rc = libevdev_new_from_fd(fd, &dev);
    if (rc < 0) { fprintf(stderr, "new_from_fd: %s\n", strerror(-rc)); return 1; }

    printf("name: %s\n", libevdev_get_name(dev));
    printf("version: %x\n", libevdev_get_driver_version(dev));
    printf("vendor=%x product=%x bustype=%x\n",
        libevdev_get_id_vendor(dev), libevdev_get_id_product(dev),
        libevdev_get_id_bustype(dev));
    printf("has REL_X=%d REL_WHEEL=%d BTN_LEFT=%d KEY_A=%d EV_SYN=%d\n",
        libevdev_has_event_code(dev, EV_REL, REL_X),
        libevdev_has_event_code(dev, EV_REL, REL_WHEEL),
        libevdev_has_event_code(dev, EV_KEY, BTN_LEFT),
        libevdev_has_event_code(dev, EV_KEY, KEY_A),
        libevdev_has_event_type(dev, EV_SYN));
    fflush(stdout);

    // Poll loop: 8 seconds of events.
    struct pollfd pfd = { .fd = fd, .events = POLLIN };
    int deadline = 80;
    while (deadline-- > 0) {
        int pr = poll(&pfd, 1, 100);
        if (pr <= 0) continue;
        struct input_event ev;
        while (read(fd, &ev, sizeof(ev)) == sizeof(ev)) {
            const char *tn = libevdev_event_type_get_name(ev.type);
            const char *cn = libevdev_event_code_get_name(ev.type, ev.code);
            printf("ev %-10s %-22s %d\n", tn ? tn : "?", cn ? cn : "?", ev.value);
            if (++deadline > 8000) break;
            if (fflush(stdout)) {}
        }
    }
    printf("libevdev_test done\n");
    return 0;
}
