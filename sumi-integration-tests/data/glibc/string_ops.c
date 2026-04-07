/* String/memory routines that exercise glibc's IFUNC dispatch and SSE2 fast paths. */
#include <stdio.h>
#include <string.h>

int main(void) {
    char buf[128];
    memset(buf, 0, sizeof(buf));

    strcpy(buf, "hello");
    strcat(buf, ", ");
    strcat(buf, "world");
    if (strlen(buf) != 12) return 1;
    if (strcmp(buf, "hello, world") != 0) return 2;

    if (strncmp(buf, "hello", 5) != 0) return 3;
    if (strchr(buf, 'w') == NULL) return 4;
    if (memchr(buf, ',', 12) == NULL) return 5;

    char dst[16];
    memcpy(dst, "abcdefghij", 11);
    if (memcmp(dst, "abcdefghij", 11) != 0) return 6;

    /* memmove with overlapping ranges. */
    char src[16] = "0123456789";
    memmove(src + 2, src, 8);
    if (memcmp(src, "0101234567", 10) != 0) return 7;

    printf("string ops ok: %s\n", buf);
    return 0;
}
