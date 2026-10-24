// sleep: tiny static delay helper for init.sh ordering waits (musl).
// Usage: sleep SECONDS (fractions ok, e.g. sleep 0.5)
#define _POSIX_C_SOURCE 199309L
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

int main(int argc, char **argv) {
    if (argc != 2) {
        dprintf(2, "usage: sleep SECONDS\n");
        return 2;
    }
    double s = strtod(argv[1], 0);
    if (!(s > 0))
        return 0;
    struct timespec ts;
    ts.tv_sec = (time_t)s;
    ts.tv_nsec = (long)((s - (double)(time_t)s) * 1000000000.0);
    nanosleep(&ts, 0);
    return 0;
}
