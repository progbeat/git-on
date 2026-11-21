/* Inject one unlink failure in private copy-up backing. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int unlink(const char *name) {
    static int failed;
    int (*real)(const char *) = dlsym(RTLD_NEXT, "unlink");
    struct stat info;
    if (!failed && getenv("GIT_ON_TEST_UNLINK_FAULT") && strstr(name, "/backing/") &&
        lstat(name, &info) == 0 && S_ISREG(info.st_mode)) {
        failed = 1;
        errno = EACCES;
        return -1;
    }
    return real(name);
}

int unlinkat(int directory, const char *name, int flags) {
    static int failed;
    int (*real)(int, const char *, int) = dlsym(RTLD_NEXT, "unlinkat");
    if (!failed && getenv("GIT_ON_TEST_COPY_ROLLBACK_FAULT")) {
        char fd[64], path[PATH_MAX];
        static const char suffix[] = "/backing";
        snprintf(fd, sizeof(fd), "/proc/self/fd/%d", directory);
        ssize_t length = readlink(fd, path, sizeof(path));
        if (length >= (ssize_t)(sizeof(suffix) - 1) &&
            memcmp(path + length - (sizeof(suffix) - 1), suffix, sizeof(suffix) - 1) == 0) {
            failed = 1;
            errno = EACCES;
            return -1;
        }
    }
    return real(directory, name, flags);
}
