#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <poll.h>
#include <errno.h>
#include <libinput.h>

static int open_restricted(const char *path, int flags, void *ud) {
    return open(path, flags);
}
static void close_restricted(int fd, void *ud) { close(fd); }
static const struct libinput_interface iface = {
    .open_restricted = open_restricted,
    .close_restricted = close_restricted,
};

int main(void) {
    struct libinput *li = libinput_path_create_context(&iface, NULL);
    if (!li) { fprintf(stderr, "create_context failed\n"); return 1; }
    libinput_log_set_priority(li, LIBINPUT_LOG_PRIORITY_DEBUG);

    struct libinput_device *dev =
        libinput_path_add_device(li, "/dev/input/event0");
    if (!dev) { fprintf(stderr, "add_device failed\n"); return 1; }
    printf("device added: %s\n", libinput_device_get_name(dev));
    fflush(stdout);

    int fd = libinput_get_fd(li);
    struct pollfd pfd = { .fd = fd, .events = POLLIN };
    // ponytail: counter-based loop spins out because an epoll fd is always
    // "ready"; run a fixed 12-second window instead.
    long deadline_ms = 12000;
    for (long waited = 0; waited < deadline_ms;) {
        int r = poll(&pfd, 1, 100);
        if (r > 0) {
            // drain everything available without counting toward the clock
            goto handle;
        }
        waited += 100;
        continue;
handle:;
        {
        static int beats = 0;
        if (++beats % 20 == 0) { printf("[heartbeat %d]\n", beats); fflush(stdout); }
        if (libinput_dispatch(li) != 0) { printf("dispatch failed\n"); }
        libinput_dispatch(li);
        struct libinput_event *ev;
        while ((ev = libinput_get_event(li)) != NULL) {
            enum libinput_event_type t = libinput_event_get_type(ev);
            switch (t) {
            case LIBINPUT_EVENT_KEYBOARD_KEY: {
                struct libinput_event_keyboard *k =
                    (struct libinput_event_keyboard *)ev;
                printf("key %u %s\n",
                       libinput_event_keyboard_get_key(k),
                       libinput_event_keyboard_get_key_state(k) == LIBINPUT_KEY_STATE_PRESSED ?
                         "pressed" : "released");
                break;
            }
            case LIBINPUT_EVENT_POINTER_MOTION: {
                struct libinput_event_pointer *p =
                    (struct libinput_event_pointer *)ev;
                printf("motion dx=%.2f dy=%.2f\n",
                       libinput_event_pointer_get_dx(p),
                       libinput_event_pointer_get_dy(p));
                break;
            }
            case LIBINPUT_EVENT_POINTER_BUTTON: {
                struct libinput_event_pointer *p =
                    (struct libinput_event_pointer *)ev;
                printf("button %u %s\n",
                       libinput_event_pointer_get_button(p),
                       libinput_event_pointer_get_button_state(p) == LIBINPUT_BUTTON_STATE_PRESSED ?
                         "pressed" : "released");
                break;
            }
            case LIBINPUT_EVENT_DEVICE_ADDED:
                printf("LIBINPUT: device-added %s\n",
                       libinput_device_get_name(libinput_event_get_device(ev)));
                break;
            default:
                printf("event type %d\n", t);
            }
            libinput_event_destroy(ev);
            fflush(stdout);
        }
        // events drained; keep the 12s window running
        waited += 0;
        }
    }
    printf("libinput_test done\n");
    return 0;
}
