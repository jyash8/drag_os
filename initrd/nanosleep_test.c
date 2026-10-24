#include <stdio.h>
#include <time.h>

int main() {
    struct timespec req = {0, 100000000}; /* 100ms */
    struct timespec rem;
    struct timespec t1, t2;
    clock_gettime(CLOCK_MONOTONIC, &t1);
    int r = nanosleep(&req, &rem);
    clock_gettime(CLOCK_MONOTONIC, &t2);
    long d1 = t1.tv_sec * 1000 + t1.tv_nsec / 1000000;
    long d2 = t2.tv_sec * 1000 + t2.tv_nsec / 1000000;
    printf("nanosleep ret=%d elapsed=%ldms rem=%ld.%09ld\n",
           r, d2 - d1, rem.tv_sec, rem.tv_nsec);
    return 0;
}
