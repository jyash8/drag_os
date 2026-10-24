#include <stdio.h>
#include <time.h>

int main() {
    for (int i = 0; i < 10; i++) {
        struct timespec t;
        clock_gettime(CLOCK_MONOTONIC, &t);
        printf("i=%d mono=%ld.%09ld\n", i, t.tv_sec, t.tv_nsec);
        for (volatile int j = 0; j < 1000000; j++);
    }
    return 0;
}
