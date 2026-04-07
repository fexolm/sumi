/* qsort + bsearch — verifies function-pointer dispatch and stack usage. */
#include <stdio.h>
#include <stdlib.h>

static int cmp_int(const void *a, const void *b) {
    int x = *(const int *)a;
    int y = *(const int *)b;
    return (x > y) - (x < y);
}

int main(void) {
    int data[] = { 9, 3, 7, 1, 8, 2, 6, 4, 5, 0 };
    size_t n = sizeof(data) / sizeof(data[0]);

    qsort(data, n, sizeof(int), cmp_int);
    for (size_t i = 0; i < n; i++) {
        if (data[i] != (int)i) return 1;
    }

    int needle = 7;
    int *found = bsearch(&needle, data, n, sizeof(int), cmp_int);
    if (!found || *found != 7) return 2;

    printf("qsort/bsearch ok\n");
    return 0;
}
