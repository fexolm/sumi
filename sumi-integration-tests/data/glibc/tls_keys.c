// pthread_key_create + pthread_setspecific/getspecific: each thread must
// observe only the value it stored under the shared key, never another
// thread's value.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>

#define THREADS 8

static pthread_key_t key;
static atomic_int failures = 0;

static void *worker(void *arg) {
    long idx = (long)arg;
    // Distinguishable per-thread payload — an address, not just an int,
    // so a key/slot mixup would very likely produce garbage rather than
    // a coincidentally-matching small integer.
    long *value = malloc(sizeof(long));
    if (!value) {
        atomic_fetch_add(&failures, 1);
        return NULL;
    }
    *value = idx * 1000 + 7;

    if (pthread_setspecific(key, value) != 0) {
        atomic_fetch_add(&failures, 1);
        return NULL;
    }

    // Yield-heavy read-back loop: give the scheduler room to interleave
    // other threads' setspecific/getspecific calls before we check that
    // we still see our own value.
    for (int i = 0; i < 100; i++) {
        long *got = pthread_getspecific(key);
        if (got != value || *got != idx * 1000 + 7) {
            atomic_fetch_add(&failures, 1);
            return NULL;
        }
        sched_yield();
    }

    free(value);
    return NULL;
}

int main(void) {
    if (pthread_key_create(&key, NULL) != 0) {
        fprintf(stderr, "pthread_key_create failed\n");
        return 1;
    }

    pthread_t t[THREADS];
    for (long i = 0; i < THREADS; i++) {
        if (pthread_create(&t[i], NULL, worker, (void *)i) != 0) {
            fprintf(stderr, "pthread_create failed\n");
            return 2;
        }
    }
    for (int i = 0; i < THREADS; i++) {
        pthread_join(t[i], NULL);
    }

    if (atomic_load(&failures) != 0) {
        fprintf(stderr, "%d thread(s) observed the wrong TLS value\n",
                atomic_load(&failures));
        return 3;
    }

    // The main thread never called setspecific — its slot must read back
    // NULL (the default), not a stale value left by a worker.
    if (pthread_getspecific(key) != NULL) {
        fprintf(stderr, "main thread's key slot is not NULL\n");
        return 4;
    }

    pthread_key_delete(key);
    return 0;
}
