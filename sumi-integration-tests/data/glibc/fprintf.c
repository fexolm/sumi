/* Tests fprintf to both stdout and stderr through glibc's stream layer. */
#include <stdio.h>
#include <stdlib.h>

int main(void) {
    fprintf(stdout, "fprintf-stdout %d\n", 1);
    fprintf(stderr, "fprintf-stderr %d\n", 2);
    fflush(NULL);
    return 0;
}
