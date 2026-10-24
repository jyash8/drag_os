#define _XOPEN_SOURCE 700
#define _DEFAULT_SOURCE
#define _POSIX_C_SOURCE 199309L
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <time.h>
#include <sys/mman.h>
#include <sys/ioctl.h>

#define SYS_GET_FB_INFO 333

struct FbInfoSys {
    uint64_t phys_addr;
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint8_t bpp;
};

static long syscall1(long n, long a1) {
    long ret;
    __asm__ volatile ("syscall" : "=a"(ret) : "a"(n), "D"(a1) : "rcx", "r11", "memory");
    return ret;
}

static uint64_t get_time_us(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) == 0) {
        return (uint64_t)ts.tv_sec * 1000000ULL + (uint64_t)ts.tv_nsec / 1000ULL;
    }
    return 0;
}

int main(int argc, char **argv) {
    // Auto-dispatch to Wayland player if a Wayland compositor socket actually exists AND WAYLAND_DISPLAY is set
    int has_socket = (access("/run/wayland-1", F_OK) == 0) || (access("/run/wayland-0", F_OK) == 0);
    if (getenv("WAYLAND_DISPLAY") && has_socket) {
        if (access("/bin/wl_player", X_OK) == 0) {
            execv("/bin/wl_player", argv);
        } else if (access("/usr/bin/wl_player", X_OK) == 0) {
            execv("/usr/bin/wl_player", argv);
        } else if (access("/wl_player", X_OK) == 0) {
            execv("/wl_player", argv);
        }
    }

    printf("\n==================================================\n");
    printf("     Drag OS Video Player (Musl Engine - Synced)  \n");
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

    // Try opening candidate paths
    const char *candidates[4];
    int cand_count = 0;

    // If target ends with .mp4 or is video.mp4, check .vid first for DRGV container
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

    for (int c = 0; c < cand_count; c++) {
        f = fopen(candidates[c], "rb");
        if (f) {
            if (fread(hdr, 1, 14, f) == 14 && memcmp(hdr, "DRGV", 4) == 0) {
                used_file = candidates[c];
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

    // Framebuffer resolution & mapping
    struct FbInfoSys fb_sys;
    uint32_t *fb_map = NULL;
    uint32_t screen_w = 1280, screen_h = 800, pitch = 1280;
    int fb_fd = -1;

    if (syscall1(SYS_GET_FB_INFO, (long)&fb_sys) == 0 && fb_sys.phys_addr != 0) {
        screen_w = fb_sys.width;
        screen_h = fb_sys.height;
        pitch = fb_sys.pitch;
        fb_map = (uint32_t *)fb_sys.phys_addr;
        printf("Video Player: Framebuffer mapped directly at %p (%ux%u, pitch=%u)\n",
               fb_map, screen_w, screen_h, pitch);
    } else {
        fb_fd = open("/dev/fb0", O_RDWR);
        if (fb_fd >= 0) {
            size_t fb_size = (size_t)pitch * screen_h * 4;
            void *p = mmap(NULL, fb_size, PROT_READ | PROT_WRITE, MAP_SHARED, fb_fd, 0);
            if (p != MAP_FAILED) {
                fb_map = (uint32_t *)p;
                printf("Video Player: /dev/fb0 mapped at %p\n", fb_map);
            }
        }
    }

    // AC'97 PCM Audio Engine
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

    int scale_x = screen_w / (v_width ? v_width : 320);
    if (scale_x < 1) scale_x = 1;
    int scale_y = screen_h / (v_height ? v_height : 200);
    if (scale_y < 1) scale_y = 1;

    int render_w = v_width * scale_x;
    int render_h = v_height * scale_y;
    int off_x = (screen_w > (uint32_t)render_w) ? (int)(screen_w - render_w) / 2 : 0;
    int off_y = (screen_h > (uint32_t)render_h) ? (int)(screen_h - render_h) / 2 : 0;

    uint32_t *line_buf = malloc(render_w * sizeof(uint32_t));

    if (fb_map) {
        memset(fb_map, 0, (size_t)pitch * screen_h * 4);
    }

    printf("Decoding and Playing Synchronized Video + Audio...\n");
    fflush(stdout);

    uint64_t start_time_us = get_time_us();
    uint32_t frame_delay_us = 1000000 / fps;
    uint32_t played = 0;

    for (uint32_t frame = 0; frame < total_frames; frame++) {
        // 1. Read & Stream Audio Chunk
        if (fread(audio_buf, 1, audio_chunk_bytes, f) != (size_t)audio_chunk_bytes) break;
        if (dsp_fd >= 0) {
            write(dsp_fd, audio_buf, audio_chunk_bytes);
        }

        // 2. Read & Render Video Frame
        if (fread(packed_frame, 1, video_frame_bits_bytes, f) != (size_t)video_frame_bits_bytes) break;

        if (fb_map && line_buf) {
            for (int y = 0; y < v_height; y++) {
                for (int x = 0; x < v_width; x++) {
                    int pixel_idx = y * v_width + x;
                    int byte_idx = pixel_idx / 8;
                    int bit_idx = 7 - (pixel_idx % 8);
                    uint32_t color = ((packed_frame[byte_idx] >> bit_idx) & 1) ? 0x00FFFFFF : 0x00000000;

                    int px_base = x * scale_x;
                    for (int sx = 0; sx < scale_x; sx++) {
                        line_buf[px_base + sx] = color;
                    }
                }

                for (int sy = 0; sy < scale_y; sy++) {
                    int cur_y = off_y + y * scale_y + sy;
                    if (cur_y >= 0 && (uint32_t)cur_y < screen_h) {
                        uint32_t *dst_row = &fb_map[cur_y * pitch + off_x];
                        memcpy(dst_row, line_buf, render_w * sizeof(uint32_t));
                    }
                }
            }
        }

        played++;

        // Periodic console progress
        if (frame % fps == 0 || frame + 1 == total_frames) {
            uint32_t cur_sec = frame / fps;
            printf("[PLAYING] Frame %u/%u (%02u:%02u / %02u:%02u)\n",
                   frame + 1, total_frames, cur_sec / 60, cur_sec % 60, total_sec / 60, total_sec % 60);
            fflush(stdout);
        }

        // Monotonic reference clock synchronization
        if (start_time_us > 0) {
            uint64_t target_us = start_time_us + (uint64_t)(frame + 1) * frame_delay_us;
            uint64_t now_us = get_time_us();
            if (target_us > now_us) {
                usleep((uint32_t)(target_us - now_us));
            }
        } else if (dsp_fd < 0) {
            usleep(frame_delay_us);
        }
    }

    printf("\nPlayback Finished! Total frames played: %u\n", played);
    fflush(stdout);

    if (dsp_fd >= 0) close(dsp_fd);
    if (fb_fd >= 0) close(fb_fd);
    fclose(f);
    free(audio_buf);
    free(packed_frame);
    if (line_buf) free(line_buf);

    return 0;
}
