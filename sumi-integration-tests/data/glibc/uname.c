/* uname() must populate the utsname struct with non-empty fields. */
#include <stdio.h>
#include <string.h>
#include <sys/utsname.h>

int main(void) {
    struct utsname uts;
    if (uname(&uts) != 0) return 1;
    if (strlen(uts.sysname) == 0) return 2;
    if (strlen(uts.release) == 0) return 3;
    if (strlen(uts.machine) == 0) return 4;
    printf("uname ok: %s %s %s\n", uts.sysname, uts.release, uts.machine);
    return 0;
}
