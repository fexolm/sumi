/* fopen/fwrite/fread/fseek/fclose round-trip through glibc's stdio buffering. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void) {
    const char *path = "/tmp/sumi_int_glibc_io.txt";
    const char payload[] = "glibc stdio works\n";

    FILE *f = fopen(path, "w+");
    if (!f) return 1;

    if (fwrite(payload, 1, sizeof(payload) - 1, f) != sizeof(payload) - 1) return 2;
    if (fflush(f) != 0) return 3;

    if (fseek(f, 0, SEEK_SET) != 0) return 4;

    char buf[64] = {0};
    size_t n = fread(buf, 1, sizeof(buf) - 1, f);
    if (n != sizeof(payload) - 1) return 5;
    if (strcmp(buf, payload) != 0) return 6;

    if (fclose(f) != 0) return 7;

    printf("file io ok\n");
    return 0;
}
