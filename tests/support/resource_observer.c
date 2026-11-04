#define _DEFAULT_SOURCE
#include <sys/stat.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <signal.h>
#include <unistd.h>
#include <errno.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--stat-size")) {
        printf("%zu", sizeof(struct stat));
        return 0;
    }
    if (argc < 5 || strcmp(argv[1], "--rss-fd") || strcmp(argv[3], "--"))
        return 125;
    char *end;
    errno = 0;
    long value = strtol(argv[2], &end, 10);
    if (errno || *end || end == argv[2] || value < 3 || value > INT_MAX)
        return 125;
    int output = (int)value;

    /* The child owns graceful cleanup of any separately owned process groups. */
    struct sigaction action = {0};
    action.sa_handler = SIG_IGN;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGTERM, &action, NULL))
        return 125;
    pid_t pid = fork();
    if (pid < 0)
        return 125;
    if (pid == 0) {
        close(output);
        action.sa_handler = SIG_DFL;
        if (sigaction(SIGTERM, &action, NULL))
            _exit(125);
        execvp(argv[4], argv + 4);
        _exit(127);
    }
    struct rusage usage;
    int status;
    while (wait4(pid, &status, 0, &usage) < 0)
        if (errno != EINTR)
            return 125;
    long resident = usage.ru_maxrss;
#ifdef __APPLE__
    resident = (resident + 1023) / 1024;
#endif
    if (dprintf(output, "%ld\n", resident) < 0 || close(output))
        return 125;
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
}
