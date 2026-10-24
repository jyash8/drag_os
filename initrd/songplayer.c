#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <time.h>

#define SPF 44100
#define CHUNK_SAMPLES 11025   // 0.25s per chunk
#define LEAD_SAMPLES  (SPF * 6 / 5) // keep 1.2s buffered ahead of the DMA
                                   // (absorbs occasional disk/fs stalls)
#define CHUNK_BYTES (CHUNK_SAMPLES * 2)

static long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long)ts.tv_sec * 1000000000L + ts.tv_nsec;
}

int main(int argc, char **argv) {
    const char *file = (argc > 1) ? argv[1] : "/jandro.wav";
    int wav = open(file, O_RDONLY);
    if (wav < 0) { printf("songplayer: open %s failed\n", file); return 1; }

    uint8_t hdr[44];
    if (read(wav, hdr, sizeof(hdr)) != sizeof(hdr)) { printf("songplayer: short wav\n"); return 1; }

    // Read the whole song into RAM first so disk stalls never touch playback.
    int16_t *song = malloc(1);
    size_t cap = 0, len = 0;
    int16_t tmp[CHUNK_SAMPLES];
    ssize_t n;
    while ((n = read(wav, tmp, sizeof(tmp))) > 0) {
        if (len + n > cap) {
            cap = cap ? cap * 2 : 65536;
            if (cap < len + n) cap = len + n;
            song = realloc(song, cap);
            if (!song) { printf("songplayer: oom\n"); return 1; }
        }
        memcpy((char *)song + len, tmp, n);
        len += n;
    }
    close(wav);
    if (len < 4) { printf("songplayer: empty\n"); return 1; }
    printf("songplayer: buffered %zd bytes of PCM from %s\n", len, file);

    int dsp = open("/dev/dsp", O_WRONLY);
    if (dsp < 0) { printf("songplayer: no /dev/dsp\n"); return 1; }

    size_t off = 0, total = 0;
    while (off < len) {
        size_t n = (len - off) < sizeof(tmp) ? (len - off) : sizeof(tmp);
        if (write(dsp, (char *)song + off, n) != (ssize_t)n) break;
        off += n;
        total += n;
    }
    printf("songplayer: played %ld bytes from %s\n", total, file);

    close(dsp);
    free(song);
    return 0;
}