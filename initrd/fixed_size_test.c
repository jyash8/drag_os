/*
 * fixed_size_test.c — a Wayland client that declares a fixed window size.
 *
 * xdg-shell has no "resizable" boolean. The de-facto convention (used by
 * sway, Hyprland and mutter) is that a client which sets
 * xdg_toplevel.set_min_size(w, h) and set_max_size(w, h) to the *same*
 * non-zero value is telling the compositor "do not resize me". A compositor
 * is expected to float such a window instead of tiling it.
 *
 * This client does exactly that and then holds the window open, so the
 * compositor's decision can be observed (the compositor prints a trace
 * line when it places a window).
 */
#define _POSIX_C_SOURCE 200809L
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <fcntl.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif
#ifndef __NR_memfd_create
#define __NR_memfd_create 319
#endif

static struct wl_compositor *compositor = NULL;
static struct wl_shm *shm = NULL;
static struct xdg_wm_base *wm_base = NULL;
static struct wl_surface *surface = NULL;
static struct xdg_surface *xdg_surface = NULL;
static struct xdg_toplevel *toplevel = NULL;
static int configured = 0;
static int width = 400, height = 300;

static void serial(const char *msg) {
    int fd = open("/dev/ttyS0", O_WRONLY | O_NOCTTY);
    if (fd >= 0) {
        ssize_t n = write(fd, msg, strlen(msg));
        (void)n;
        close(fd);
    }
    fprintf(stderr, "%s", msg);
}

static int create_shm_file(off_t size) {
    long sc_memfd = 319;
    int fd = syscall(sc_memfd, "wl_fixed_shm", 1);
    if (fd < 0) {
        char name[] = "/tmp/wl_shm-XXXXXX";
        fd = mkstemp(name);
        if (fd >= 0)
            unlink(name);
    }
    if (fd >= 0) {
        if (ftruncate(fd, size) < 0) {
            close(fd);
            return -1;
        }
    }
    return fd;
}

static void draw_and_commit(void) {
    int stride = width * 4;
    int size = stride * height;
    int fd = create_shm_file(size);
    if (fd < 0) {
        serial("[fixed_size_test] shm alloc failed\n");
        return;
    }
    void *data = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (data == MAP_FAILED) {
        close(fd);
        return;
    }
    uint32_t *pixels = data;
    for (int y = 0; y < height; y++)
        for (int x = 0; x < width; x++) {
            uint32_t col = 0xFF203040;
            if (x < 4 || x >= width - 4 || y < 4 || y >= height - 4)
                col = 0xFFFF8800; /* orange border: easy to spot */
            pixels[y * width + x] = col;
        }
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, size);
    struct wl_buffer *buffer =
        wl_shm_pool_create_buffer(pool, 0, width, height, stride, WL_SHM_FORMAT_XRGB8888);
    wl_surface_attach(surface, buffer, 0, 0);
    wl_surface_commit(surface);
    wl_shm_pool_destroy(pool);
    close(fd);
    munmap(data, size);
}

static void xdg_surface_handle_configure(void *d, struct xdg_surface *s, uint32_t serial_num) {
    xdg_surface_ack_configure(s, serial_num);
    if (!configured) {
        configured = 1;
        draw_and_commit();
    }
}
static const struct xdg_surface_listener xdg_surface_listener = {xdg_surface_handle_configure};

static void xdg_toplevel_handle_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h,
                                           struct wl_array *s) {
    (void)d;
    (void)t;
    (void)w;
    (void)h;
    (void)s;
    /* A fixed-size client keeps the size it asked for and ignores the
     * compositor's configure: that is the whole point of declaring
     * min_size == max_size. */
}
static void xdg_toplevel_handle_close(void *d, struct xdg_toplevel *t) {
    (void)d;
    (void)t;
    serial("[fixed_size_test] close event\n");
    exit(0);
}
static const struct xdg_toplevel_listener toplevel_listener = {xdg_toplevel_handle_configure,
                                                               xdg_toplevel_handle_close};

static void wm_base_ping(void *d, struct xdg_wm_base *b, uint32_t serial_num) {
    (void)d;
    xdg_wm_base_pong(b, serial_num);
}
static const struct xdg_wm_base_listener wm_base_listener = {wm_base_ping};

static void registry_global(void *d, struct wl_registry *reg, uint32_t id, const char *iface,
                            uint32_t ver) {
    (void)d;
    (void)ver;
    if (strcmp(iface, wl_compositor_interface.name) == 0)
        compositor = wl_registry_bind(reg, id, &wl_compositor_interface, 4);
    else if (strcmp(iface, wl_shm_interface.name) == 0)
        shm = wl_registry_bind(reg, id, &wl_shm_interface, 1);
    else if (strcmp(iface, xdg_wm_base_interface.name) == 0)
        wm_base = wl_registry_bind(reg, id, &xdg_wm_base_interface, 1);
}
static void registry_remove(void *d, struct wl_registry *r, uint32_t id) {
    (void)d;
    (void)r;
    (void)id;
}
static const struct wl_registry_listener registry_listener = {registry_global, registry_remove};

int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) {
        serial("[fixed_size_test] no wayland display\n");
        return 1;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);
    if (!compositor || !shm || !wm_base) {
        serial("[fixed_size_test] missing globals\n");
        return 1;
    }
    xdg_wm_base_add_listener(wm_base, &wm_base_listener, NULL);
    surface = wl_compositor_create_surface(compositor);
    xdg_surface = xdg_wm_base_get_xdg_surface(wm_base, surface);
    xdg_surface_add_listener(xdg_surface, &xdg_surface_listener, NULL);
    toplevel = xdg_surface_get_toplevel(xdg_surface);
    xdg_toplevel_add_listener(toplevel, &toplevel_listener, NULL);
    xdg_toplevel_set_title(toplevel, "fixed-size-test");
    xdg_toplevel_set_app_id(toplevel, "fixed-size-test");
    /* This is the "I am not resizable" signal. */
    xdg_toplevel_set_min_size(toplevel, width, height);
    xdg_toplevel_set_max_size(toplevel, width, height);
    serial("[fixed_size_test] mapped with min_size == max_size == 400x300\n");
    wl_surface_commit(surface);
    /* Stay alive long enough for the compositor to place the window, then
     * exit: the compositor has to give the keyboard focus back to the
     * terminal when this window goes away. */
    struct timespec deadline;
    clock_gettime(CLOCK_MONOTONIC, &deadline);
    deadline.tv_sec += 8;
    while (wl_display_dispatch(display) != -1) {
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (now.tv_sec >= deadline.tv_sec)
            break;
    }
    serial("[fixed_size_test] exiting\n");
    return 0;
}
