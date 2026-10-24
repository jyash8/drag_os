#include <stdio.h>
#include <time.h>
#include <unistd.h>

int main() {
    struct timespec t1, t2;
    clock_gettime(CLOCK_MONOTONIC, &t1);
    printf("mono start: %ld.%09ld\n", t1.tv_sec, t1.tv_nsec);
    sleep(1);
    clock_gettime(CLOCK_MONOTONIC, &t2);
    printf("mono after sleep: %ld.%09ld\n", t2.tv_sec, t2.tv_nsec);
    printf("delta ms: %ld\n", (t2.tv_sec - t1.tv_sec) * 1000 + (t2.tv_nsec - t1.tv_nsec) / 1000000);

    struct timespec r1, r2;
    clock_gettime(CLOCK_REALTIME, &r1);
    printf("realtime start: %ld.%09ld\n", r1.tv_sec, r1.tv_nsec);
    sleep(1);
    clock_gettime(CLOCK_REALTIME, &r2);
    printf("realtime after sleep: %ld.%09ld\n", r2.tv_sec, r2.tv_nsec);
    printf("delta ms: %ld\n", (r2.tv_sec - r1.tv_sec) * 1000 + (r2.tv_nsec - r1.tv_nsec) / 1000000);
    return 0;
}
