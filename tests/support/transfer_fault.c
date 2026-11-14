/* Deterministic same-UID mutation and OS errors at source ownership boundaries. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int armed;
static int mode_is(const char *value) {
    const char *mode = getenv("GIT_ON_TEST_TRANSFER_FAULT");
    return mode && strcmp(mode, value) == 0;
}
static void tamper(int fd, off64_t offset) {
    ssize_t (*real)(int, const void *, size_t, off64_t) = dlsym(RTLD_NEXT, "pwrite64");
    const char *marker = getenv("GIT_ON_TEST_TRANSFER_MARKER");
    if (!marker || real(fd, "!", 1, offset) != 1)
        _exit(96);
    int observed = open(marker, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0600);
    if (observed < 0)
        _exit(97);
    close(observed);
}
int renameat(int from, const char *old, int to, const char *name) {
    int (*real)(int, const char *, int, const char *) = dlsym(RTLD_NEXT, "renameat");
    int transfer = strlen(old) == 40 && strlen(name) &&
        strspn(name, "0123456789") == strlen(name);
    if (transfer && mode_is("rename")) {
        int fd = openat(from, old, O_WRONLY | O_CLOEXEC | O_NOFOLLOW);
        tamper(fd, 2 * 65536 + 7);
        close(fd);
    }
    int result = real(from, old, to, name);
    if (!result && transfer)
        armed = 1;
    return result;
}
ssize_t pwrite64(int fd, const void *bytes, size_t size, off64_t offset) {
    ssize_t (*real)(int, const void *, size_t, off64_t) = dlsym(RTLD_NEXT, "pwrite64");
    if (armed && mode_is("write")) {
        armed = 0;
        tamper(fd, 7);
    }
    return real(fd, bytes, size, offset);
}
int ftruncate64(int fd, off64_t size) {
    int (*real)(int, off64_t) = dlsym(RTLD_NEXT, "ftruncate64");
    if (armed && mode_is("truncate")) {
        armed = 0;
        tamper(fd, 7);
    }
    return real(fd, size);
}
int unlink(const char *name) {
    int (*real)(const char *) = dlsym(RTLD_NEXT, "unlink");
    if (armed && mode_is("unlink") && strstr(name, "/backing/")) {
        armed = 0;
        errno = EACCES;
        return -1;
    }
    return real(name);
}
