#include <stdint.h>
#include <stddef.h>

#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR   2
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#define PROT_READ  0x1
#define PROT_WRITE 0x2
#define MAP_PRIVATE 0x02
#define MAP_DEVICE  0x40

#define SYS_READ         0
#define SYS_WRITE        1
#define SYS_OPEN         2
#define SYS_CLOSE        3
#define SYS_LSEEK        8
#define SYS_MMAP         9
#define SYS_IOCTL       16
#define SYS_NANOSLEEP   35
#define SYS_EXIT        60
#define SYS_GET_FB_INFO 333

static long syscall1(long n, long a1) {
    long ret;
    __asm__ volatile ("syscall" : "=a"(ret) : "a"(n), "D"(a1) : "rcx", "r11", "memory");
    return ret;
}
static long syscall2(long n, long a1, long a2) {
    long ret;
    __asm__ volatile ("syscall" : "=a"(ret) : "a"(n), "D"(a1), "S"(a2) : "rcx", "r11", "memory");
    return ret;
}
static long syscall3(long n, long a1, long a2, long a3) {
    long ret;
    __asm__ volatile ("syscall" : "=a"(ret) : "a"(n), "D"(a1), "S"(a2), "d"(a3) : "rcx", "r11", "memory");
    return ret;
}
static long syscall6(long n, long a1, long a2, long a3, long a4, long a5, long a6) {
    long ret;
    register long r10 __asm__("r10") = a4;
    register long r8  __asm__("r8")  = a5;
    register long r9  __asm__("r9")  = a6;
    __asm__ volatile ("syscall" : "=a"(ret) : "a"(n), "D"(a1), "S"(a2), "d"(a3), "r"(r10), "r"(r8), "r"(r9) : "rcx", "r11", "memory");
    return ret;
}

static void print_str(const char *s);

size_t strlen(const char *s) {
    size_t len = 0;
    while (s[len]) len++;
    return len;
}

void *memset(void *s, int c, size_t n) {
    uint8_t *p = (uint8_t *)s;
    for (size_t i = 0; i < n; i++) p[i] = (uint8_t)c;
    return s;
}

static void print_str(const char *s) {
    syscall3(SYS_WRITE, 1, (long)s, strlen(s));
}

static void print_num(uint32_t n) {
    char buf[16];
    int i = 0;
    if (n == 0) { print_str("0"); return; }
    while (n > 0) {
        buf[i++] = '0' + (n % 10);
        n /= 10;
    }
    for (int j = 0; j < i / 2; j++) {
        char tmp = buf[j];
        buf[j] = buf[i - 1 - j];
        buf[i - 1 - j] = tmp;
    }
    buf[i] = '\0';
    print_str(buf);
}

static int sys_open(const char *path, int flags) {
    return (int)syscall3(SYS_OPEN, (long)path, flags, 0);
}
static int sys_read(int fd, void *buf, size_t count) {
    return (int)syscall3(SYS_READ, fd, (long)buf, count);
}
static int sys_write(int fd, const void *buf, size_t count) {
    return (int)syscall3(SYS_WRITE, fd, (long)buf, count);
}
static int sys_close(int fd) {
    return (int)syscall1(SYS_CLOSE, fd);
}
static int sys_ioctl(int fd, unsigned long cmd, void *arg) {
    return (int)syscall3(SYS_IOCTL, fd, cmd, (long)arg);
}
static void sys_usleep(uint32_t usec) {
    uint64_t req[2];
    req[0] = usec / 1000000;
    req[1] = (usec % 1000000) * 1000;
    syscall2(SYS_NANOSLEEP, (long)req, 0);
}

struct FbInfoSys {
    uint64_t phys_addr;
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint8_t bpp;
};

static uint8_t heap_mem[128 * 1024]; // 128KB static heap
static size_t heap_off = 0;
static void *sys_malloc(size_t sz) {
    sz = (sz + 15) & ~(size_t)15; // align 16
    if (heap_off + sz > sizeof(heap_mem)) return NULL;
    void *p = &heap_mem[heap_off];
    heap_off += sz;
    return p;
}

static int sys_memcmp(const void *s1, const void *s2, size_t n) {
    const uint8_t *p1 = s1, *p2 = s2;
    for (size_t i = 0; i < n; i++) {
        if (p1[i] != p2[i]) return p1[i] - p2[i];
    }
    return 0;
}

int main(int argc, char **argv) {
    print_str("\n==================================================\n");
    print_str("     Drag OS Video Player (MPlayer Engine)        \n");
    print_str("==================================================\n");

    const char *file = (argc > 1 && argv[1]) ? argv[1] : "/video.vid";
    int vid_fd = sys_open(file, O_RDONLY);
    if (vid_fd < 0) {
        file = "/video.vid";
        vid_fd = sys_open(file, O_RDONLY);
    }

    if (vid_fd < 0) {
        print_str("Video Player: open /video.vid failed\n");
        return 1;
    }

    uint8_t hdr[14];
    if (sys_read(vid_fd, hdr, 14) != 14 || sys_memcmp(hdr, "DRGV", 4) != 0) {
        print_str("Video Player: invalid format\n");
        sys_close(vid_fd);
        return 1;
    }

    uint16_t v_width = hdr[4] | (hdr[5] << 8);
    uint16_t v_height = hdr[6] | (hdr[7] << 8);
    uint16_t fps = hdr[8] | (hdr[9] << 8);
    uint32_t total_frames = hdr[10] | (hdr[11] << 8) | (hdr[12] << 16) | (hdr[13] << 24);

    print_str("Video File: ");
    print_str(file);
    print_str(" (");
    print_num(v_width);
    print_str("x");
    print_num(v_height);
    print_str(" @ ");
    print_num(fps);
    print_str(" FPS, Total Frames: ");
    print_num(total_frames);
    print_str(")\n");

    struct FbInfoSys fb_sys;
    uint32_t *fb_map = NULL;
    uint32_t screen_w = 1280, screen_h = 800, pitch = 1280;

    if (syscall1(SYS_GET_FB_INFO, (long)&fb_sys) == 0) {
        screen_w = fb_sys.width;
        screen_h = fb_sys.height;
        pitch = fb_sys.pitch;
        fb_map = (uint32_t *)fb_sys.phys_addr;
        print_str("Video Player: Framebuffer mapped directly\n");
    }

    int dsp_fd = sys_open("/dev/dsp", O_WRONLY);
    if (dsp_fd >= 0) {
        uint32_t speed = 44100;
        uint32_t channels = 1;
        uint32_t fmt = 16;
        sys_ioctl(dsp_fd, 0x5002, &speed);
        sys_ioctl(dsp_fd, 0x5006, &channels);
        sys_ioctl(dsp_fd, 0x5005, &fmt);
        print_str("Video Player: AC97 PCM Audio Engine Initialized (/dev/dsp)\n");
    }

    int samples_per_frame = 44100 / (fps ? fps : 24);
    int audio_chunk_bytes = samples_per_frame * 2;
    uint8_t *audio_buf = (uint8_t *)sys_malloc(audio_chunk_bytes);

    int video_frame_bits_bytes = (v_width * v_height) / 8;
    uint8_t *packed_frame = (uint8_t *)sys_malloc(video_frame_bits_bytes);

    int scale_x = screen_w / (v_width ? v_width : 320);
    if (scale_x < 1) scale_x = 1;
    int scale_y = screen_h / (v_height ? v_height : 200);
    if (scale_y < 1) scale_y = 1;

    int render_w = v_width * scale_x;
    int render_h = v_height * scale_y;
    int off_x = (screen_w > render_w) ? (screen_w - render_w) / 2 : 0;
    int off_y = (screen_h > render_h) ? (screen_h - render_h) / 2 : 0;

    if (fb_map) {
        memset(fb_map, 0, (size_t)pitch * screen_h * 4);
    }

    print_str("Decoding and Playing Synchronized Video + Audio...\n");
    uint32_t target_delay_us = 1000000 / (fps ? fps : 24);

    for (uint32_t f = 0; f < total_frames; f++) {
        // 1. Read & Play Audio Chunk
        if (sys_read(vid_fd, audio_buf, audio_chunk_bytes) != audio_chunk_bytes) break;
        if (dsp_fd >= 0) {
            sys_write(dsp_fd, audio_buf, audio_chunk_bytes);
        }

        // 2. Read & Render Video Frame
        if (sys_read(vid_fd, packed_frame, video_frame_bits_bytes) != video_frame_bits_bytes) break;

        if (fb_map) {
            for (int y = 0; y < v_height; y++) {
                int start_sy = off_y + y * scale_y;
                if (start_sy >= screen_h) break;

                for (int x = 0; x < v_width; x++) {
                    int pixel_idx = y * v_width + x;
                    int byte_idx = pixel_idx / 8;
                    int bit_idx = 7 - (pixel_idx % 8);
                    uint32_t color = ((packed_frame[byte_idx] >> bit_idx) & 1) ? 0x00FFFFFF : 0x00000000;

                    int px_base = off_x + x * scale_x;
                    for (int sy = 0; sy < scale_y; sy++) {
                        int cur_y = start_sy + sy;
                        if (cur_y >= screen_h) break;

                        uint32_t *row = &fb_map[cur_y * pitch];
                        for (int sx = 0; sx < scale_x; sx++) {
                            if (px_base + sx < screen_w) {
                                row[px_base + sx] = color;
                            }
                        }
                    }
                }
            }
        }

        sys_usleep(target_delay_us);
    }

    print_str("\nPlayback Finished!\n");
    if (dsp_fd >= 0) sys_close(dsp_fd);
    sys_close(vid_fd);
    return 0;
}

__asm__(
    ".global _start\n"
    "_start:\n"
    "   andq $-16, %rsp\n"
    "   mov $1, %rdi\n"
    "   call main\n"
    "   mov %rax, %rdi\n"
    "   mov $60, %rax\n"
    "   syscall\n"
);
