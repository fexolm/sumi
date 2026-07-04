// Verify glibc pthread_create correctly allocates a TCB and
// the kernel restores fs_base on context switch so the child reads its own
// __thread variable. Calling pthread_join is covered by pthread_join.c.
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>

static atomic_int child_ok = 0;
static __thread volatile int tls_canary = 0xCAFEBABE;

static void *thread_fn(void *arg) {
    (void)arg;
    // Reads via fs:<offset>. Wrong fs_base → garbage → fail.
    if (tls_canary == 0xCAFEBABE) {
        tls_canary = 0xDEADBEEF;              // per-thread copy
        atomic_store(&child_ok, 1);
    } else {
        atomic_store(&child_ok, 2);
    }
    // Spin forever so main's exit_group kills the VM.
    for (;;) sched_yield();
}

int main(void) {
    pthread_t t;
    if (pthread_create(&t, NULL, thread_fn, NULL) != 0) {
        fprintf(stderr, "pthread_create failed\n");
        return 1;
    }
    while (atomic_load(&child_ok) == 0) sched_yield();
    if (atomic_load(&child_ok) != 1) return 2;
    // Main thread observes its own copy of tls_canary is unchanged.
    if (tls_canary != 0xCAFEBABE) return 3;
    return 0;
}
