#define _POSIX_C_SOURCE 200809L
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <poll.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"

// ponytail: single-header JPEG decode for DRGJ color frames; JPEG only, memory API.
#define STBI_ONLY_JPEG
#define STBI_NO_STDIO
#define STB_IMAGE_IMPLEMENTATION
#include "stb_image.h"

#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif

#ifndef __NR_memfd_create
#define __NR_memfd_create 319
#endif

/* --- Global Wayland State --- */
static struct wl_display *display = NULL;
static struct wl_compositor *compositor = NULL;
static struct wl_shm *shm = NULL;
static struct xdg_wm_base *wm_base = NULL;
static struct wl_surface *surface = NULL;
static struct xdg_surface *xdg_surface = NULL;
static struct xdg_toplevel *toplevel = NULL;

static int configured = 0;
static int running = 1;

/* --- Double-buffered SHM structure --- */
struct ShmBuffer {
    int fd;
    size_t size;
    uint32_t *pixels;
    struct wl_shm_pool *pool;
    struct wl_buffer *buffer;
    int busy;
};

static void buffer_release(void *data, struct wl_buffer *wl_buffer) {
    (void)wl_buffer;
    struct ShmBuffer *buf = (struct ShmBuffer *)data;
    buf->busy = 0;
}

static const struct wl_buffer_listener buffer_listener = {
    .release = buffer_release,
};

static int create_shm_fd(size_t size) {
    int fd = syscall(__NR_memfd_create, "wl_player_shm", MFD_CLOEXEC);
    if (fd < 0) {
        char name[] = "/tmp/wl_shm-XXXXXX";
        fd = mkstemp(name);
        if (fd >= 0) unlink(name);
    }
    if (fd >= 0) {
        if (ftruncate(fd, size) < 0) {
            close(fd);
            return -1;
        }
    }
    return fd;
}

static int init_shm_buffer(struct ShmBuffer *b, int width, int height) {
    int stride = width * 4;
    b->size = (size_t)stride * height;
    b->fd = create_shm_fd(b->size);
    if (b->fd < 0) return -1;

    b->pixels = mmap(NULL, b->size, PROT_READ | PROT_WRITE, MAP_SHARED, b->fd, 0);
    if (b->pixels == MAP_FAILED) {
        close(b->fd);
        b->fd = -1;
        return -1;
    }

    memset(b->pixels, 0, b->size);
    b->pool = wl_shm_create_pool(shm, b->fd, b->size);
    b->buffer = wl_shm_pool_create_buffer(b->pool, 0, width, height, stride, WL_SHM_FORMAT_XRGB8888);
    wl_buffer_add_listener(b->buffer, &buffer_listener, b);
    b->busy = 0;
    return 0;
}

static void free_shm_buffer(struct ShmBuffer *b) {
    if (b->buffer) wl_buffer_destroy(b->buffer);
    if (b->pool) wl_shm_pool_destroy(b->pool);
    if (b->pixels && b->pixels != MAP_FAILED) munmap(b->pixels, b->size);
    if (b->fd >= 0) close(b->fd);
}

/* --- Listeners --- */
static void xdg_surface_handle_configure(void *data, struct xdg_surface *s, uint32_t serial) {
    (void)data;
    xdg_surface_ack_configure(s, serial);
    configured = 1;
}

static const struct xdg_surface_listener xdg_surface_listener = {
    .configure = xdg_surface_handle_configure,
};

static void xdg_toplevel_handle_configure(void *data, struct xdg_toplevel *t,
                                          int32_t w, int32_t h, struct wl_array *states) {
    (void)data; (void)t; (void)w; (void)h; (void)states;
}

static void xdg_toplevel_handle_close(void *data, struct xdg_toplevel *t) {
    (void)data; (void)t;
    printf("Wayland Video Player: Window closed by user.\n");
    running = 0;
}

static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = xdg_toplevel_handle_configure,
    .close = xdg_toplevel_handle_close,
};

static void wm_base_ping(void *data, struct xdg_wm_base *b, uint32_t serial) {
    (void)data;
    xdg_wm_base_pong(b, serial);
}

static const struct xdg_wm_base_listener wm_base_listener = {
    .ping = wm_base_ping,
};

static void registry_global(void *data, struct wl_registry *reg, uint32_t id,
                            const char *interface, uint32_t version) {
    (void)data; (void)version;
    if (strcmp(interface, wl_compositor_interface.name) == 0) {
        compositor = wl_registry_bind(reg, id, &wl_compositor_interface, 4);
    } else if (strcmp(interface, wl_shm_interface.name) == 0) {
        shm = wl_registry_bind(reg, id, &wl_shm_interface, 1);
    } else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
        wm_base = wl_registry_bind(reg, id, &xdg_wm_base_interface, 1);
    }
}

static void registry_remove(void *data, struct wl_registry *reg, uint32_t id) {
    (void)data; (void)reg; (void)id;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_remove,
};

static uint64_t get_time_us(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) == 0) {
        return (uint64_t)ts.tv_sec * 1000000ULL + (uint64_t)ts.tv_nsec / 1000ULL;
    }
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    setvbuf(stderr, NULL, _IONBF, 0);

    if (!getenv("XDG_RUNTIME_DIR")) {
        setenv("XDG_RUNTIME_DIR", "/run", 1);
    }

    printf("\n==================================================\n");
    printf("     Drag OS Video Player (Wayland Native Engine) \n");
    printf("==================================================\n");

    const char *target_file = "/video.mp4";
    uint32_t max_frames_limit = 0;

    char *env_limit = getenv("MAX_FRAMES");
    if (env_limit) {
        max_frames_limit = (uint32_t)atoi(env_limit);
    }

    for (int i = 1; i < argc; i++) {
        if ((strcmp(argv[i], "-n") == 0 || strcmp(argv[i], "--frames") == 0) && i + 1 < argc) {
            max_frames_limit = (uint32_t)atoi(argv[++i]);
        } else if (argv[i][0] != '-') {
            target_file = argv[i];
        }
    }

    printf("Target File: %s\n", target_file);

    // Open video stream
    const char *candidates[4];
    int cand_count = 0;
    if (strstr(target_file, ".mp4") != NULL) {
        candidates[cand_count++] = "/video.vid";
        candidates[cand_count++] = "video.vid";
    }
    candidates[cand_count++] = target_file;
    if (strcmp(target_file, "/video.vid") != 0) {
        candidates[cand_count++] = "/video.vid";
    }

    FILE *f = NULL;
    const char *used_file = NULL;
    uint8_t hdr[14];

    int is_color = 0; // DRGJ = color MJPEG frames; DRGV = legacy 1-bit frames
    for (int c = 0; c < cand_count; c++) {
        f = fopen(candidates[c], "rb");
        if (f) {
            if (fread(hdr, 1, 14, f) == 14 &&
                (memcmp(hdr, "DRGV", 4) == 0 || memcmp(hdr, "DRGJ", 4) == 0)) {
                used_file = candidates[c];
                is_color = (hdr[3] == 'J');
                break;
            }
            fclose(f);
            f = NULL;
        }
    }

    if (!f) {
        printf("Video Player: could not open a valid DRGV video stream for %s\n", target_file);
        return 1;
    }

    uint16_t v_width = hdr[4] | (hdr[5] << 8);
    uint16_t v_height = hdr[6] | (hdr[7] << 8);
    uint16_t fps = hdr[8] | (hdr[9] << 8);
    uint32_t total_frames = hdr[10] | (hdr[11] << 8) | (hdr[12] << 16) | (hdr[13] << 24);
    if (fps == 0) fps = 24;

    uint32_t total_sec = total_frames / fps;
    printf("Video Stream: %s (%dx%d @ %d FPS, Total Frames: %u, Duration: %02u:%02u)\n",
           used_file, v_width, v_height, fps, total_frames, total_sec / 60, total_sec % 60);

    if (max_frames_limit > 0 && max_frames_limit < total_frames) {
        printf("Playback Limit: %u frames requested\n", max_frames_limit);
        total_frames = max_frames_limit;
    }

    // Connect to Wayland display (retry up to 120 times with 500ms sleep in case compositor is still initializing)
    printf("Wayland Video Player: Connecting to Wayland compositor...\n");
    for (int retry = 0; retry < 120; retry++) {
        display = wl_display_connect(NULL);
        if (!display) {
            display = wl_display_connect("wayland-1");
        }
        if (!display) {
            display = wl_display_connect("wayland-0");
        }
        if (display) {
            printf("Wayland Video Player: Connected to display successfully! (attempt %d)\n", retry + 1);
            break;
        }
        usleep(500000); // 500ms
    }
    if (!display) {
        fprintf(stderr, "Wayland Video Player: Error: Cannot connect to Wayland display after 60s.\n");
        fprintf(stderr, "Is mywima compositor running? Check WAYLAND_DISPLAY environment variable.\n");
        fclose(f);
        return 1;
    }

    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);

    if (!compositor || !shm || !wm_base) {
        fprintf(stderr, "Wayland Video Player: Missing compositor, shm, or xdg_wm_base interface.\n");
        fclose(f);
        wl_display_disconnect(display);
        return 1;
    }

    xdg_wm_base_add_listener(wm_base, &wm_base_listener, NULL);

    surface = wl_compositor_create_surface(compositor);
    xdg_surface = xdg_wm_base_get_xdg_surface(wm_base, surface);
    xdg_surface_add_listener(xdg_surface, &xdg_surface_listener, NULL);

    toplevel = xdg_surface_get_toplevel(xdg_surface);
    xdg_toplevel_add_listener(toplevel, &toplevel_listener, NULL);
    xdg_toplevel_set_title(toplevel, "DragOS Video Player - Bad Apple");
    xdg_toplevel_set_app_id(toplevel, "dragos-video-player");

    // Commit empty surface to prompt configure event
    wl_surface_commit(surface);
    while (!configured && running) {
        if (wl_display_dispatch(display) < 0) {
            running = 0;
            break;
        }
    }

    // Window dimensions: 2x scaled (640x400)
    int scale = 2;
    int win_width = v_width * scale;
    int win_height = v_height * scale;

    struct ShmBuffer buffers[2];
    if (init_shm_buffer(&buffers[0], win_width, win_height) < 0 ||
        init_shm_buffer(&buffers[1], win_width, win_height) < 0) {
        fprintf(stderr, "Wayland Video Player: Error initializing SHM buffers.\n");
        free_shm_buffer(&buffers[0]);
        free_shm_buffer(&buffers[1]);
        fclose(f);
        wl_display_disconnect(display);
        return 1;
    }

    printf("Wayland Video Window: %dx%d (2x Scale) - Double Buffered SHM Initialized\n",
           win_width, win_height);

    // Audio Engine (/dev/dsp)
    int dsp_fd = open("/dev/dsp", O_WRONLY);
    if (dsp_fd >= 0) {
        uint32_t speed = 44100;
        uint32_t channels = 1;
        uint32_t fmt = 16;
        ioctl(dsp_fd, 0x5002, &speed);    // SNDCTL_DSP_SPEED
        ioctl(dsp_fd, 0x5006, &channels); // SNDCTL_DSP_CHANNELS
        ioctl(dsp_fd, 0x5005, &fmt);      // SNDCTL_DSP_SETFMT
        printf("Video Player: AC97 PCM Audio Engine Initialized (/dev/dsp, 44.1kHz 16-bit mono)\n");
    }

    int samples_per_frame = 44100 / fps;
    int audio_chunk_bytes = samples_per_frame * 2;
    uint8_t *audio_buf = malloc(audio_chunk_bytes);

    int video_frame_bits_bytes = (v_width * v_height) / 8;
    uint8_t *packed_frame = malloc(video_frame_bits_bytes);
    // DRGJ color path staging: JPEG blob (grown as needed)
    size_t jpg_cap = 1 << 20;
    uint8_t *jpg_buf = malloc(jpg_cap);

    printf("Decoding and Playing Synchronized Video + Audio in Wayland...\n");
    fflush(stdout);

    uint64_t start_time_us = get_time_us();
    uint32_t frame_delay_us = 0; // ponytail: zero delay for max speed playback
    uint32_t played = 0;
    int cur_buf = 0;

    int wl_fd = wl_display_get_fd(display);

    for (uint32_t frame = 0; frame < total_frames && running; frame++) {
        // 1. Process incoming Wayland events (non-blocking)
        while (wl_display_prepare_read(display) != 0) {
            wl_display_dispatch_pending(display);
        }
        struct pollfd pfd = { .fd = wl_fd, .events = POLLIN };
        if (poll(&pfd, 1, 0) > 0 && (pfd.revents & POLLIN)) {
            wl_display_read_events(display);
            wl_display_dispatch_pending(display);
        } else {
            wl_display_cancel_read(display);
        }

        if (!running) break;

        // 2. Read & Render Video Frame FIRST (DRGJ order: jpg_len, jpg, audio)
        struct ShmBuffer *buf = &buffers[cur_buf];
        uint32_t *dst = buf->pixels;

        if (is_color) {
            uint32_t jpg_len = 0;
            if (fread(&jpg_len, 1, 4, f) != 4) {
                fprintf(stderr, "wl_player: EOF reading jpg_len at frame %u\n", frame);
                break;
            }
            if (jpg_len > jpg_cap) {
                jpg_cap = (jpg_len + 65535) & ~(size_t)65535;
                jpg_buf = realloc(jpg_buf, jpg_cap);
                if (!jpg_buf) {
                    fprintf(stderr, "wl_player: realloc failed for %zu bytes\n", jpg_cap);
                    break;
                }
            }
            if (fread(jpg_buf, 1, jpg_len, f) != jpg_len) {
                fprintf(stderr, "wl_player: short read on jpg_buf at frame %u (len %u)\n", frame, jpg_len);
                break;
            }
            int jw = 0, jh = 0;
            uint8_t *dec = stbi_load_from_memory(jpg_buf, (int)jpg_len, &jw, &jh, NULL, 3);
            if (!dec || jw != v_width || jh != v_height) {
                fprintf(stderr, "wl_player: jpeg decode error at frame %u: dec=%p jw=%d jh=%d (expected %dx%d)\n",
                        frame, dec, jw, jh, v_width, v_height);
                if (dec) stbi_image_free(dec);
                break;
            }
            for (int y = 0; y < v_height; y++) {
                uint8_t *src_row = dec + (size_t)y * v_width * 3;
                int base_y = y * scale;
                for (int sy = 0; sy < scale; sy++) {
                    uint32_t *row = &dst[(base_y + sy) * win_width];
                    for (int x = 0; x < v_width; x++) {
                        uint32_t color = 0xFF000000u
                            | ((uint32_t)src_row[x * 3] << 16)
                            | ((uint32_t)src_row[x * 3 + 1] << 8)
                            | (uint32_t)src_row[x * 3 + 2];
                        int base_x = x * scale;
                        for (int sx = 0; sx < scale; sx++) {
                            row[base_x + sx] = color;
                        }
                    }
                }
            }
            stbi_image_free(dec);

            // Read audio chunk written after each JPEG frame in DRGJ
            if (fread(audio_buf, 1, audio_chunk_bytes, f) != (size_t)audio_chunk_bytes) {
                fprintf(stderr, "wl_player: EOF reading audio_buf at frame %u\n", frame);
                break;
            }
            if (dsp_fd >= 0) {
                write(dsp_fd, audio_buf, audio_chunk_bytes);
            }
        } else {
            if (fread(packed_frame, 1, video_frame_bits_bytes, f) != (size_t)video_frame_bits_bytes) {
                fprintf(stderr, "wl_player: EOF reading packed_frame at frame %u\n", frame);
                break;
            }

            for (int y = 0; y < v_height; y++) {
                for (int x = 0; x < v_width; x++) {
                    int pixel_idx = y * v_width + x;
                    int byte_idx = pixel_idx / 8;
                    int bit_idx = 7 - (pixel_idx % 8);
                    uint32_t color = ((packed_frame[byte_idx] >> bit_idx) & 1) ? 0xFFFFFFFF : 0xFF000000;

                    int base_x = x * scale;
                    int base_y = y * scale;

                    for (int sy = 0; sy < scale; sy++) {
                        uint32_t *row = &dst[(base_y + sy) * win_width + base_x];
                        for (int sx = 0; sx < scale; sx++) {
                            row[sx] = color;
                        }
                    }
                }
            }
        }

        // 4. Attach & Commit Frame to Wayland surface
        buf->busy = 1;
        wl_surface_attach(surface, buf->buffer, 0, 0);
        wl_surface_damage(surface, 0, 0, win_width, win_height);
        wl_surface_commit(surface);
        wl_display_flush(display);

        cur_buf = 1 - cur_buf;
        played++;

        // Periodic console progress
        if (frame % fps == 0 || frame + 1 == total_frames) {
            uint32_t cur_sec = frame / fps;
            printf("[WAYLAND] Playing Frame %u/%u (%02u:%02u / %02u:%02u)\n",
                   frame + 1, total_frames, cur_sec / 60, cur_sec % 60, total_sec / 60, total_sec % 60);
            fflush(stdout);
        }

        // 5. No frame pacing (ponytail: zero delay for max speed)
    }

    printf("\nWayland Playback Finished! Total frames displayed: %u\n", played);
    fflush(stdout);

    if (dsp_fd >= 0) close(dsp_fd);
    fclose(f);
    free(audio_buf);
    free(packed_frame);
    free(jpg_buf);

    free_shm_buffer(&buffers[0]);
    free_shm_buffer(&buffers[1]);

    if (toplevel) xdg_toplevel_destroy(toplevel);
    if (xdg_surface) xdg_surface_destroy(xdg_surface);
    if (surface) wl_surface_destroy(surface);
    if (wm_base) xdg_wm_base_destroy(wm_base);
    if (shm) wl_shm_destroy(shm);
    if (compositor) wl_compositor_destroy(compositor);
    if (display) wl_display_disconnect(display);

    return 0;
}
