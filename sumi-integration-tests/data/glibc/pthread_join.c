// pthread_join returns cleanly.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>

static void *thread_fn(void *arg) {
    (void)arg;
    return (void *)42;
}

int main(void) {
    pthread_t t;
    if (pthread_create(&t, NULL, thread_fn, NULL) != 0) {
        fprintf(stderr, "pthread_create failed\n");
        return 1;
    }
    void *ret = NULL;
    if (pthread_join(t, &ret) != 0) {
        fprintf(stderr, "pthread_join failed\n");
        return 2;
    }
    if (ret != (void *)42) {
        fprintf(stderr, "wrong return value: %p\n", ret);
        return 3;
    }
    return 0;
}
