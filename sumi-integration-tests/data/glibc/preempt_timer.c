// Verify timer preemption works.
//
// A busy-loop thread that NEVER voluntarily yields must be preempted
// by the LAPIC timer so that other threads can make progress. Without
// preemption, the worker thread would never run and the test would fail.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <time.h>

static atomic_int stop = 0;
static atomic_long worker_count = 0;

static void *busy_loop(void *arg) {
    (void)arg;
    while (!atomic_load(&stop)) {
        // Pure CPU spin — no syscall, no sched_yield.
        // Without preemption, this thread monopolizes its vCPU forever.
    }
    return NULL;
}

static void *worker(void *arg) {
    (void)arg;
    while (!atomic_load(&stop)) {
        atomic_fetch_add(&worker_count, 1);
        // No sched_yield — relies entirely on timer preemption to get
        // CPU time away from the busy_loop thread.
    }
    return NULL;
}

int main(void) {
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 65536);

    pthread_t t_busy, t_worker;
    if (pthread_create(&t_busy, &attr, busy_loop, NULL) != 0) return 1;
    if (pthread_create(&t_worker, &attr, worker, NULL) != 0) return 1;
    pthread_attr_destroy(&attr);

    // Sleep 200ms. During this time, the timer preempts the busy-loop
    // thread every ~1ms, giving the worker thread a chance to run.
    struct timespec ts = { 0, 200000000 }; // 200ms
    nanosleep(&ts, NULL);

    atomic_store(&stop, 1);
    pthread_join(t_busy, NULL);
    pthread_join(t_worker, NULL);

    // With preemption: worker_count >> 0. Expect at least 10 iterations
    // in 200ms (actually hundreds, since each timer tick gives the worker
    // a 1ms timeslice worth of atomic_fetch_add iterations).
    long count = atomic_load(&worker_count);
    if (count < 10) {
        fprintf(stderr, "worker_count = %ld (too low -- preemption broken?)\n", count);
        return 2;
    }
    return 0;
}
