/* opendir/readdir/closedir — exercises glibc's directory enumeration on top
   of getdents64. */
#include <dirent.h>
#include <stdio.h>
#include <string.h>

int main(void) {
    DIR *d = opendir("/tmp");
    if (!d) return 1;

    int count = 0;
    struct dirent *e;
    while ((e = readdir(d)) != NULL) {
        /* Every entry must have a non-empty name. */
        if (e->d_name[0] == 0) return 2;
        count++;
        if (count > 1000) break; /* Safety bound. */
    }

    if (closedir(d) != 0) return 3;
    printf("dirent ok: count=%d\n", count);
    return 0;
}
