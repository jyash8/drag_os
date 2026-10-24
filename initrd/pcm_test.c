#include <stdio.h>
#include <unistd.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>

#define SAMPLE_RATE 44100
#define DURATION_SEC 2
#define FREQ 440.0

int main() {
    printf("Playing AC97 Digital PCM Audio Test (/dev/dsp)...\n");

    int fd = open("/dev/dsp", O_WRONLY);
    if (fd < 0) {
        printf("Failed to open /dev/dsp!\n");
        return 1;
    }

    int total_samples = SAMPLE_RATE * DURATION_SEC;
    int16_t buffer[2048];
    int buf_idx = 0;

    for (int i = 0; i < total_samples; i++) {
        double t = (double)i / SAMPLE_RATE;
        // Generate 440Hz sine wave (scaled to 16-bit PCM)
        int16_t val = (int16_t)(sin(2.0 * 3.141592653589793 * FREQ * t) * 16384.0);
        buffer[buf_idx++] = val;

        if (buf_idx >= 2048) {
            write(fd, buffer, buf_idx * sizeof(int16_t));
            buf_idx = 0;
        }
    }

    if (buf_idx > 0) {
        write(fd, buffer, buf_idx * sizeof(int16_t));
    }

    close(fd);
    printf("PCM Audio Playback Finished!\n");
    return 0;
}
