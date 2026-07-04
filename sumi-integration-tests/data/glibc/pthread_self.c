// pthread_self() must be unique per thread and must match the pthread_t
// handle pthread_create() handed back to the parent.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>

#define THREADS 8

static pthread_t reported[THREADS];

static void *worker(void *arg) {
    long idx = (long)arg;
    reported[idx] = pthread_self();
    return NULL;
}

int main(void) {
    pthread_t created[THREADS];

    for (long i = 0; i < THREADS; i++) {
        if (pthread_create(&created[i], NULL, worker, (void *)i) != 0) {
            fprintf(stderr, "pthread_create failed\n");
            return 1;
        }
    }
    for (int i = 0; i < THREADS; i++) {
        pthread_join(created[i], NULL);
    }

    // Each worker's self-observed pthread_self() must equal what
    // pthread_create() returned to the parent for that same thread.
    for (int i = 0; i < THREADS; i++) {
        if (!pthread_equal(reported[i], created[i])) {
            fprintf(stderr, "thread %d: pthread_self() mismatch\n", i);
            return 2;
        }
    }

    // All THREADS ids must be pairwise distinct, and none may equal the
    // main thread's own id.
    pthread_t self = pthread_self();
    for (int i = 0; i < THREADS; i++) {
        if (pthread_equal(reported[i], self)) {
            fprintf(stderr, "thread %d collided with main thread id\n", i);
            return 3;
        }
        for (int j = i + 1; j < THREADS; j++) {
            if (pthread_equal(reported[i], reported[j])) {
                fprintf(stderr, "threads %d and %d share an id\n", i, j);
                return 4;
            }
        }
    }

    return 0;
}
