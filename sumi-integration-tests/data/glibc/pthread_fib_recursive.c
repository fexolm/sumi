// Stress test: recursive Fibonacci via thread creation.
//
// fib(n) spawns two child threads for fib(n-1) and fib(n-2), joins both,
// and returns the sum. Below a threshold, computes iteratively to avoid
// an exponential blowup in thread count. With threshold=10 and n=20,
// this creates ~180 threads total (fib(20) tree with leaves at depth 10).
// Exercises:
//   - Deeply nested pthread_create/join (thread creates threads)
//   - Concurrent thread creation on multiple threads simultaneously
//   - Stack of join operations (parent blocked in futex_wait until
//     child finishes, child itself blocked on grandchildren)
//   - Reaper handling a burst of zombie threads
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>

#define FIB_N         15
#define THRESHOLD     12     // switch to iterative below this

typedef struct {
    int n;
    int64_t result;
} fib_arg_t;

static int64_t fib_iterative(int n) {
    if (n <= 1) return n;
    int64_t a = 0, b = 1;
    for (int i = 2; i <= n; i++) {
        int64_t t = a + b;
        a = b;
        b = t;
    }
    return b;
}

// Shared pthread_attr with a small stack size (64 KB). Threads that
// only call fib_iterative need minimal stack; the recursive ones need
// space for the thread-local variables and two join calls.
static pthread_attr_t fib_attr;

static void *fib_thread(void *arg) {
    fib_arg_t *fa = (fib_arg_t *)arg;
    int n = fa->n;

    if (n <= THRESHOLD) {
        fa->result = fib_iterative(n);
        return NULL;
    }

    // Spawn two child threads for fib(n-1) and fib(n-2).
    fib_arg_t a1 = { .n = n - 1, .result = 0 };
    fib_arg_t a2 = { .n = n - 2, .result = 0 };
    pthread_t t1, t2;

    if (pthread_create(&t1, &fib_attr, fib_thread, &a1) != 0) {
        fprintf(stderr, "pthread_create fib(%d-1) failed\n", n);
        fa->result = -1;
        return NULL;
    }
    if (pthread_create(&t2, &fib_attr, fib_thread, &a2) != 0) {
        fprintf(stderr, "pthread_create fib(%d-2) failed\n", n);
        pthread_join(t1, NULL);
        fa->result = -1;
        return NULL;
    }

    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    fa->result = a1.result + a2.result;
    return NULL;
}

int main(void) {
    pthread_attr_init(&fib_attr);
    pthread_attr_setstacksize(&fib_attr, 65536);

    fib_arg_t root = { .n = FIB_N, .result = 0 };
    pthread_t t;

    if (pthread_create(&t, &fib_attr, fib_thread, &root) != 0) {
        fprintf(stderr, "pthread_create fib(%d) failed\n", FIB_N);
        return 1;
    }
    pthread_join(t, NULL);

    int64_t expected = fib_iterative(FIB_N);
    if (root.result != expected) {
        fprintf(stderr, "fib(%d) = %ld, expected %ld\n",
                FIB_N, (long)root.result, (long)expected);
        return 2;
    }

    pthread_attr_destroy(&fib_attr);
    printf("fib(%d) = %ld OK\n", FIB_N, (long)root.result);
    return 0;
}
