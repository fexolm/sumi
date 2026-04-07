/* time(), clock_gettime(), and difftime() — exercise glibc's time wrappers. */
#include <stdio.h>
#include <time.h>

int main(void) {
    time_t now = time(NULL);
    if (now <= 0) return 1;

    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) return 2;
    if (ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1000000000) return 3;

    /* The realtime clock should be close to time(NULL). */
    struct timespec real;
    if (clock_gettime(CLOCK_REALTIME, &real) != 0) return 4;
    double diff = difftime(real.tv_sec, now);
    if (diff < -2 || diff > 2) return 5;

    printf("time ok: now=%ld\n", (long)now);
    return 0;
}
