// Stress test (design doc §14.3, scaled down from 100x1000 to fit the
// harness's 30s timeout): WORKERS threads take a strict round-robin
// ticket protected by a mutex+condvar, forcing every thread to block on
// pthread_cond_wait most rounds and get woken by another thread's
// broadcast. Exercises M:N scheduling saturation (WORKERS > vCPUs),
// work-stealing, and futex wait/wake under load.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>

#define WORKERS 32
#define ROUNDS  200

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t  cv = PTHREAD_COND_INITIALIZER;
static long turn = 0;
static long counter = 0;

static void *worker(void *arg) {
    long idx = (long)arg;
    for (long r = 0; r < ROUNDS; r++) {
        long want = r * WORKERS + idx;
        pthread_mutex_lock(&mu);
        while (turn != want) {
            pthread_cond_wait(&cv, &mu);
        }
        counter++;
        turn++;
        pthread_cond_broadcast(&cv);
        pthread_mutex_unlock(&mu);
    }
    return NULL;
}

int main(void) {
    pthread_t t[WORKERS];
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 64 * 1024);

    for (long i = 0; i < WORKERS; i++) {
        if (pthread_create(&t[i], &attr, worker, (void *)i) != 0) {
            fprintf(stderr, "pthread_create failed at %ld\n", i);
            return 1;
        }
    }
    pthread_attr_destroy(&attr);

    for (int i = 0; i < WORKERS; i++) {
        pthread_join(t[i], NULL);
    }

    long expected = (long)WORKERS * ROUNDS;
    if (counter != expected) {
        fprintf(stderr, "counter=%ld expected=%ld\n", counter, expected);
        return 2;
    }
    if (turn != expected) {
        fprintf(stderr, "turn=%ld expected=%ld\n", turn, expected);
        return 3;
    }

    return 0;
}
