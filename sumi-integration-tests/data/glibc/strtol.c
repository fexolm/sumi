/* strtol/atoi conversion path through glibc. */
#include <stdio.h>
#include <stdlib.h>

int main(void) {
    if (atoi("12345") != 12345) return 1;
    if (atoi("-42") != -42) return 2;

    char *end;
    long n = strtol("0xCAFE", &end, 16);
    if (n != 0xCAFE) return 3;
    if (*end != 0) return 4;

    long n2 = strtol("  -987 trail", &end, 10);
    if (n2 != -987) return 5;
    if (end == NULL || *end != ' ') return 6;

    printf("strtol ok\n");
    return 0;
}
