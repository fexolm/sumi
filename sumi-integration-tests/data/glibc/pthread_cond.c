// Producer pushes 10000 items into a bounded ring buffer
// (capacity 64). 4 consumers pop and sum. Final sum must equal
// sum(0..9999) = 49995000.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdatomic.h>

#define CONSUMERS    4
#define ITEMS        10000
#define RING_CAP     64

static pthread_mutex_t mu     = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t  not_empty = PTHREAD_COND_INITIALIZER;
static pthread_cond_t  not_full  = PTHREAD_COND_INITIALIZER;

static long ring[RING_CAP];
static int  rhead = 0;
static int  rtail = 0;
static int  rcount = 0;
static int  done = 0;   /* producer sets this to 1 under the lock */

static atomic_long total_sum = 0;

static void ring_push(long val) {
    pthread_mutex_lock(&mu);
    while (rcount == RING_CAP)
        pthread_cond_wait(&not_full, &mu);
    ring[rtail] = val;
    rtail = (rtail + 1) % RING_CAP;
    rcount++;
    pthread_cond_signal(&not_empty);
    pthread_mutex_unlock(&mu);
}

static void *producer(void *arg) {
    (void)arg;
    for (int i = 0; i < ITEMS; i++)
        ring_push((long)i);
    pthread_mutex_lock(&mu);
    done = 1;
    pthread_cond_broadcast(&not_empty);
    pthread_mutex_unlock(&mu);
    return NULL;
}

static void *consumer(void *arg) {
    (void)arg;
    long local = 0;
    for (;;) {
        pthread_mutex_lock(&mu);
        while (rcount == 0 && !done)
            pthread_cond_wait(&not_empty, &mu);
        if (rcount == 0 && done) {
            pthread_mutex_unlock(&mu);
            break;
        }
        long val = ring[rhead];
        rhead = (rhead + 1) % RING_CAP;
        rcount--;
        pthread_cond_signal(&not_full);
        pthread_mutex_unlock(&mu);
        local += val;
    }
    atomic_fetch_add(&total_sum, local);
    return NULL;
}

int main(void) {
    pthread_t prod;
    pthread_t cons[CONSUMERS];

    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 64 * 1024);

    for (int i = 0; i < CONSUMERS; i++) {
        if (pthread_create(&cons[i], &attr, consumer, NULL) != 0) {
            fprintf(stderr, "pthread_create consumer failed\n");
            return 1;
        }
    }
    if (pthread_create(&prod, &attr, producer, NULL) != 0) {
        fprintf(stderr, "pthread_create producer failed\n");
        return 1;
    }
    pthread_attr_destroy(&attr);

    pthread_join(prod, NULL);
    for (int i = 0; i < CONSUMERS; i++)
        pthread_join(cons[i], NULL);

    long expected = (long)ITEMS * (ITEMS - 1) / 2; /* 49995000 */
    long got = atomic_load(&total_sum);
    if (got != expected) {
        fprintf(stderr, "sum=%ld expected=%ld\n", got, expected);
        return 1;
    }
    return 0;
}
