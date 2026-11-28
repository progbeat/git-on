/* Inject a single unlink error after linking a spill into the writable layer. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <stdatomic.h>
#include <string.h>

static atomic_int fault;

int linkat(int from, const char *old, int to, const char *name, int flags) {
    int (*real)(int, const char *, int, const char *, int) = dlsym(RTLD_NEXT, "linkat");
    int result = real(from, old, to, name, flags);
    if (!result && strlen(old) == 40 && strlen(name) &&
        strspn(name, "0123456789") == strlen(name)) {
        int idle = 0;
        atomic_compare_exchange_strong(&fault, &idle, 1);
    }
    return result;
}

int unlink(const char *name) {
    int (*real)(const char *) = dlsym(RTLD_NEXT, "unlink");
    int armed = 1;
    if (strstr(name, "/backing/") &&
        atomic_compare_exchange_strong(&fault, &armed, 2)) {
        errno = EACCES;
        return -1;
    }
    return real(name);
}
