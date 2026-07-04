// Phase 8 test: 4 threads each increment a shared counter 100000 times
// under a mutex. Final value must be exactly 400000.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

#define THREADS      4
#define INCREMENTS   100000

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static long counter = 0;

static void *worker(void *arg) {
    (void)arg;
    for (int i = 0; i < INCREMENTS; i++) {
        pthread_mutex_lock(&mu);
        counter++;
        pthread_mutex_unlock(&mu);
    }
    return NULL;
}

int main(void) {
    pthread_t threads[THREADS];
    for (int i = 0; i < THREADS; i++) {
        pthread_attr_t attr;
        pthread_attr_init(&attr);
        pthread_attr_setstacksize(&attr, 64 * 1024);
        if (pthread_create(&threads[i], &attr, worker, NULL) != 0) {
            fprintf(stderr, "pthread_create failed\n");
            return 1;
        }
        pthread_attr_destroy(&attr);
    }
    for (int i = 0; i < THREADS; i++) {
        pthread_join(threads[i], NULL);
    }
    if (counter != (long)THREADS * INCREMENTS) {
        fprintf(stderr, "counter=%ld expected=%d\n", counter,
                THREADS * INCREMENTS);
        return 1;
    }
    return 0;
}
