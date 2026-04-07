/* Exercises glibc's malloc/calloc/realloc/free path: brk, mmap, and freelist. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void) {
    /* Small allocs go through the fast bins (brk-backed). */
    char *a = malloc(64);
    if (!a) return 1;
    memset(a, 'A', 64);
    if (a[0] != 'A' || a[63] != 'A') return 2;

    /* Realloc grows. */
    a = realloc(a, 4096);
    if (!a) return 3;
    a[4095] = 'Z';
    if (a[0] != 'A' || a[4095] != 'Z') return 4;

    /* calloc must zero. */
    int *zeros = calloc(256, sizeof(int));
    if (!zeros) return 5;
    for (int i = 0; i < 256; i++) {
        if (zeros[i] != 0) return 6;
    }

    /* Large allocation forces mmap (above MMAP_THRESHOLD). */
    char *big = malloc(2 * 1024 * 1024);
    if (!big) return 7;
    big[0] = 'B';
    big[2 * 1024 * 1024 - 1] = 'E';
    if (big[0] != 'B' || big[2 * 1024 * 1024 - 1] != 'E') return 8;

    free(a);
    free(zeros);
    free(big);

    printf("malloc/free ok\n");
    return 0;
}
