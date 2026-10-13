#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Model the master unlinking its real socket after ownership was checked. */
int unlink(const char *path) {
    int (*real_unlink)(const char *) = dlsym(RTLD_NEXT, "unlink");
    const char *target = getenv("GIT_ON_REVIEW_UNLINK_PATH");
    const char *marker = getenv("GIT_ON_REVIEW_UNLINK_MARKER");
    const char *mode = getenv("GIT_ON_REVIEW_UNLINK_MODE");
    if (target && marker && mode && strcmp(path, target) == 0) {
        int fd = open(marker, O_WRONLY | O_CREAT | O_EXCL, 0600);
        if (fd >= 0) {
            close(fd);
            if (strcmp(mode, "removed") == 0) {
                if (real_unlink(path) != 0) return -1;
            } else if (strcmp(mode, "denied") == 0) {
                errno = EACCES;
                return -1;
            }
        }
    }
    return real_unlink(path);
}
