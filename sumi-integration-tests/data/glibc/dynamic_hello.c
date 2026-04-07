/* Smallest possible glibc-linked program. Verifies that the dynamic linker
   path (PT_INTERP → ld-linux.so.2 → libc.so.6) works inside sumi. */
#include <stdio.h>

int main(void) {
    puts("Hello from glibc!");
    return 0;
}
