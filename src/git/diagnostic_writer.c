#define _POSIX_C_SOURCE 200809L
#define _FILE_OFFSET_BITS 64

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <unistd.h>

struct diagnostic_writer {
    int output;
    int result;
    int setup_failed;
    const unsigned char *bytes;
    uint64_t length;
    void *context;
    int (*begin)(void *);
    void (*complete)(void *, int, int);
};

static int write_bytes(const struct diagnostic_writer *writer, const unsigned char *bytes, uint64_t remaining)
{
    while (remaining != 0) {
        pthread_testcancel();
        size_t chunk = remaining < 65536 ? (size_t)remaining : 65536;
        ssize_t count = write(writer->output, bytes, chunk);
        int error = count < 0 ? errno : 0;
        if (error == EINTR)
            continue;
        if (error == EAGAIN || error == EWOULDBLOCK) {
            struct pollfd ready = { .fd = writer->output, .events = POLLOUT };
            if (poll(&ready, 1, -1) < 0 && errno != EINTR)
                return errno;
            continue;
        }
        if (count <= 0)
            return count < 0 ? error : EIO;
        bytes += count;
        remaining -= (uint64_t)count;
    }
    return 0;
}

static void release_diagnostic(void *argument)
{
    struct diagnostic_writer *writer = argument;
    pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, NULL);
    sigset_t mask;
    sigfillset(&mask);
    pthread_sigmask(SIG_BLOCK, &mask, NULL);
    writer->complete(writer->context, writer->result, writer->setup_failed);
}

static void *run_writer(void *argument)
{
    struct diagnostic_writer *writer = argument;
    pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, NULL);
    pthread_cleanup_push(release_diagnostic, writer);
    sigset_t mask;
    sigfillset(&mask);
    /* Preserve the caller's SIGTTOU behavior for background terminal output. */
    sigdelset(&mask, SIGTTOU);
    int error = pthread_sigmask(SIG_BLOCK, &mask, NULL);
    if (error != 0)
        error = pthread_sigmask(SIG_BLOCK, &mask, NULL);
    if (error != 0) {
        writer->result = error;
        writer->setup_failed = 1;
    } else if (writer->begin(writer->context)) {
        pthread_setcancelstate(PTHREAD_CANCEL_ENABLE, NULL);
        pthread_testcancel();
        writer->result = write_bytes(writer, writer->bytes, writer->length);
    }
    pthread_cleanup_pop(1);
    return NULL;
}

/* Cancellation can unwind on Linux. Only C frames are cancellable. */
int git_on_diagnostic_start(pthread_t *thread, struct diagnostic_writer *writer)
{
    return pthread_create(thread, NULL, run_writer, writer);
}
