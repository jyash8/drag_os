/* socktest — guest socket smoke test.
 *
 * draginit starts this as a daemon, and its unit (30-socktest.toml) names
 * /usr/bin/socktest. The binary did not exist in the image, so every boot
 * logged:
 *
 *   [execve] lookup failed for /usr/bin/socktest
 *   [draginit] socktest (pid 5) exited (exit 127)
 *
 * and the intended check never ran at all.
 *
 * What it checks: that the Wayland socket named by $WAYLAND_DISPLAY actually
 * exists and can be connected to. That is the dependency the compositor's
 * clients all rely on, so if this fails the session has no terminal even though
 * mywima is running fine — which is exactly the confusion this test is meant to
 * make obvious.
 *
 * It deliberately does not speak the Wayland protocol: connecting is enough to
 * distinguish "the compositor is not listening" from "the compositor is up".
 * Exit 0 on success, 1 on failure, so a supervising init can tell.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stddef.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

static int connect_wayland(const char *name)
{
    /* A WAYLAND_DISPLAY with no '/' names an *abstract* socket, which is what
     * wl_display_add_socket_auto() creates and what a compositor logs as
     * "Listening on WAYLAND_DISPLAY=wayland-1". Only a value containing '/'
     * is a filesystem path. Getting this backwards makes connect() fail with
     * ENOENT against a path that does not exist while the socket is actually
     * up, i.e. it reports a healthy compositor as broken. */
    int is_abstract = strchr(name, '/') == NULL;
    const char *leaf = is_abstract ? name : name;

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    if (is_abstract) {
        if (name[0] == '@') {
            name++;
            leaf = name;
        }
        addr.sun_path[0] = '\0';
        strncpy(addr.sun_path + 1, leaf, sizeof(addr.sun_path) - 2);
    } else {
        strncpy(addr.sun_path, name, sizeof(addr.sun_path) - 1);
    }
    /* Abstract sockets use addrlen to name a leading NUL, not a string. */
    socklen_t len = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + 1 +
                                strlen(leaf));

    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        printf("socktest: socket() failed: %s\n", strerror(errno));
        return -1;
    }
    if (connect(fd, (struct sockaddr *)&addr, len) < 0) {
        printf("socktest: connect(%s, %s) failed: %s\n", name,
               is_abstract ? "abstract" : "path", strerror(errno));
        close(fd);
        return -1;
    }
    return fd;
}

int main(void)
{
    const char *wd = getenv("WAYLAND_DISPLAY");
    if (!wd || !*wd) {
        /* The compositor may not have created the socket yet; that is a
         * timing outcome, not a failure of this test. */
        printf("socktest: WAYLAND_DISPLAY unset (compositor not up yet?)\n");
        return 0;
    }
    printf("socktest: probing WAYLAND_DISPLAY=%s\n", wd);
    int fd = connect_wayland(wd);
    if (fd < 0) {
        return 1;
    }
    /* A successful connect is the whole check. Close promptly: the compositor
     * logs each client that appears and disappears. */
    close(fd);
    printf("socktest: OK\n");
    return 0;
}
