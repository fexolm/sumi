/* getenv must not crash even when no env strings are present.
   sumi currently passes an empty environment, so getenv() returns NULL. */
#include <stdio.h>
#include <stdlib.h>

int main(void) {
    const char *p = getenv("PATH");
    /* No env passed → NULL is the expected outcome. We just verify it
       returns and we can use the result safely. */
    printf("PATH=%s\n", p ? p : "(null)");
    return 0;
}
