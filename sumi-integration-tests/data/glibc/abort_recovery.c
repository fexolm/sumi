/* Verifies that errno propagation works for a known-failing syscall.
   Calls open() on a path that doesn't exist and inspects errno. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    int fd = open("/no/such/file/sumi_glibc_errno", O_RDONLY);
    if (fd >= 0) {
        return 1;
    }
    if (errno != ENOENT) {
        printf("unexpected errno = %d (%s)\n", errno, strerror(errno));
        return 2;
    }
    printf("errno=ENOENT as expected\n");
    return 0;
}
