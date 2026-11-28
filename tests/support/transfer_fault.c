/* Inject a single unlink error after linking a spill into the writable layer. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>

static atomic_int fault;
static atomic_int rollback_fault;

static int backing_name(const char *name) {
    return *name && strspn(name, "0123456789") == strlen(name);
}

static int truncate_rollback(void) {
    const char *mode = getenv("GIT_ON_TEST_COPY_UP_ROLLBACK");
    return mode && strcmp(mode, "truncate") == 0;
}

int linkat(int from, const char *old, int to, const char *name, int flags) {
    int (*real)(int, const char *, int, const char *, int) = dlsym(RTLD_NEXT, "linkat");
    int result = real(from, old, to, name, flags);
    if (!result && strlen(old) == 40 && backing_name(name)) {
        int idle = 0;
        atomic_int *selected = getenv("GIT_ON_TEST_COPY_UP_ROLLBACK") ? &rollback_fault : &fault;
        atomic_compare_exchange_strong(selected, &idle, 1);
    }
    return result;
}

static int fail_first_write(void) {
    int armed = 1;
    if (!atomic_compare_exchange_strong(&rollback_fault, &armed, 2))
        return 0;
    errno = EIO;
    return 1;
}

ssize_t pwrite(int fd, const void *data, size_t size, off_t offset) {
    ssize_t (*real)(int, const void *, size_t, off_t) = dlsym(RTLD_NEXT, "pwrite");
    return fail_first_write() ? -1 : real(fd, data, size, offset);
}

ssize_t pwrite64(int fd, const void *data, size_t size, off64_t offset) {
    ssize_t (*real)(int, const void *, size_t, off64_t) = dlsym(RTLD_NEXT, "pwrite64");
    return fail_first_write() ? -1 : real(fd, data, size, offset);
}

int unlinkat(int directory, const char *name, int flags) {
    int (*real)(int, const char *, int) = dlsym(RTLD_NEXT, "unlinkat");
    int idle = 0;
    if (strlen(name) == 40 && truncate_rollback() &&
        atomic_compare_exchange_strong(&rollback_fault, &idle, 2)) {
        errno = EIO;
        return -1;
    }
    int failed_write = 2;
    if (backing_name(name) && atomic_compare_exchange_strong(&rollback_fault, &failed_write, 3)) {
        errno = EACCES;
        return -1;
    }
    return real(directory, name, flags);
}

int unlink(const char *name) {
    int (*real)(const char *) = dlsym(RTLD_NEXT, "unlink");
    int failed_write = 2;
    if (strstr(name, "/backing/") && truncate_rollback() &&
        atomic_compare_exchange_strong(&rollback_fault, &failed_write, 3)) {
        errno = EACCES;
        return -1;
    }
    int armed = 1;
    if (strstr(name, "/backing/") &&
        atomic_compare_exchange_strong(&fault, &armed, 2)) {
        errno = EACCES;
        return -1;
    }
    return real(name);
}
