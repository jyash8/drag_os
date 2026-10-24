#include <stdio.h>
#include <time.h>

int main() {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    unsigned long start = t.tv_sec * 1000 + t.tv_nsec / 1000000;
    unsigned long max_ms = start;
    for (int i = 0; i < 1000000; i++) {
        clock_gettime(CLOCK_MONOTONIC, &t);
        unsigned long ms = t.tv_sec * 1000 + t.tv_nsec / 1000000;
        if (ms > max_ms) max_ms = ms;
    }
    printf("start=%lu max=%lu advanced=%lu\n", start, max_ms, max_ms - start);
    return 0;
}
