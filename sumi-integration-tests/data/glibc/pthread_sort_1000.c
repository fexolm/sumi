// Stress test: parallel merge sort with 100 threads.
//
// Spawns a thread per chunk (100 chunks of 10000 elements = 1M ints),
// each thread sorts its chunk with qsort, then the main thread merges.
// Verifies the result is fully sorted. Exercises:
//   - 100 concurrent pthread_create + pthread_join cycles
//   - Thread stack allocation / deallocation pressure (100 × 2 MB kernel
//     stacks + 100 × 64 KB user stacks)
//   - Scheduler under high M:N load
//   - Reaper cleaning up 100 zombie threads
//   - qsort on each thread exercises real computation with TLS
//
// Note: each thread consumes 2 MB of kernel stack from the page allocator.
// 100 threads × 2 MB = 200 MB — fits comfortably in the 2 GB VM. Using
// 1000 threads would require 2 GB of kernel stacks alone, exhausting
// memory. The user-space stack is set to 64 KB via pthread_attr to avoid
// the glibc default of 8 MB.
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define NUM_THREADS 100
#define CHUNK_SIZE  10000
#define TOTAL       (NUM_THREADS * CHUNK_SIZE)

static int data[TOTAL];

typedef struct {
    int *base;
    int count;
} chunk_t;

static int cmp_int(const void *a, const void *b) {
    int x = *(const int *)a;
    int y = *(const int *)b;
    return (x > y) - (x < y);
}

static void *sort_chunk(void *arg) {
    chunk_t *c = (chunk_t *)arg;
    qsort(c->base, c->count, sizeof(int), cmp_int);
    return NULL;
}

// Simple k-way merge of sorted chunks into `out`.
static void merge_sorted_chunks(int *out, int total,
                                 chunk_t *chunks, int nchunks) {
    int *idx = calloc(nchunks, sizeof(int));
    for (int i = 0; i < total; i++) {
        int best = -1;
        for (int c = 0; c < nchunks; c++) {
            if (idx[c] >= chunks[c].count) continue;
            if (best < 0 ||
                chunks[c].base[idx[c]] < chunks[best].base[idx[best]])
                best = c;
        }
        out[i] = chunks[best].base[idx[best]];
        idx[best]++;
    }
    free(idx);
}

int main(void) {
    // Fill with a deterministic pseudo-random sequence.
    unsigned seed = 12345;
    for (int i = 0; i < TOTAL; i++) {
        seed = seed * 1103515245 + 12345;
        data[i] = (int)(seed >> 16) & 0x7FFF;
    }

    // Use small user stacks (64 KB) instead of glibc's default 8 MB.
    // Each thread also gets a 2 MB kernel stack from the page allocator,
    // so total threading overhead is ~200 MB for 100 threads.
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 65536);

    // Set up chunks and spawn one thread per chunk.
    chunk_t chunks[NUM_THREADS];
    pthread_t threads[NUM_THREADS];
    for (int i = 0; i < NUM_THREADS; i++) {
        chunks[i].base = &data[i * CHUNK_SIZE];
        chunks[i].count = CHUNK_SIZE;
        if (pthread_create(&threads[i], &attr, sort_chunk, &chunks[i]) != 0) {
            fprintf(stderr, "pthread_create %d failed\n", i);
            return 1;
        }
    }
    pthread_attr_destroy(&attr);

    // Join all threads.
    for (int i = 0; i < NUM_THREADS; i++) {
        if (pthread_join(threads[i], NULL) != 0) {
            fprintf(stderr, "pthread_join %d failed\n", i);
            return 2;
        }
    }

    // Merge sorted chunks.
    int *sorted = malloc(TOTAL * sizeof(int));
    if (!sorted) {
        fprintf(stderr, "malloc failed\n");
        return 3;
    }
    merge_sorted_chunks(sorted, TOTAL, chunks, NUM_THREADS);

    // Verify the result is fully sorted.
    for (int i = 1; i < TOTAL; i++) {
        if (sorted[i] < sorted[i - 1]) {
            fprintf(stderr, "not sorted at index %d: %d > %d\n",
                    i, sorted[i - 1], sorted[i]);
            free(sorted);
            return 4;
        }
    }

    free(sorted);
    return 0;
}
