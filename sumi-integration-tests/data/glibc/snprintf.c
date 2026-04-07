/* Tests glibc's vsnprintf-driven formatting machinery without touching stdio. */
#include <stdio.h>
#include <string.h>

int main(void) {
    char buf[64];
    int n = snprintf(buf, sizeof(buf), "%d %s %x %.3f", 42, "hello", 0xFEED, 3.14);
    if (n < 0) {
        return 1;
    }
    if (strcmp(buf, "42 hello feed 3.140") != 0) {
        printf("got: '%s'\n", buf);
        return 2;
    }
    printf("snprintf ok: %s\n", buf);
    return 0;
}
