/* Verifies basic printf and stdout flushing through glibc.
   Exits 0 on success — sumi grep's [exit] code=0 from the kernel. */
#include <stdio.h>
#include <stdlib.h>

int main(void) {
    printf("hello from printf\n");
    printf("number = %d, hex = 0x%x, str = %s\n", 42, 0xCAFE, "ok");

    /* fflush ensures stdout buffer drains through the write() syscall path. */
    if (fflush(stdout) != 0) {
        return 1;
    }
    return 0;
}
