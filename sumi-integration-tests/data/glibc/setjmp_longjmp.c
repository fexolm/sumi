/* setjmp/longjmp — verifies that glibc can save and restore the call stack. */
#include <setjmp.h>
#include <stdio.h>

static jmp_buf env;

static void inner(void) {
    longjmp(env, 7);
}

int main(void) {
    int v = setjmp(env);
    if (v == 0) {
        inner();
        return 1; /* unreachable */
    }
    if (v != 7) return 2;
    printf("longjmp returned %d\n", v);
    return 0;
}
