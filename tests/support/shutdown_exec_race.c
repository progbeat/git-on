/* Linux/glibc fixture: change only the armed client's bytes at spawn, after
 * the runner's file checks. The replacement is a harmless sentinel script. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdlib.h>
#include <spawn.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void rewrite_client(const char *file) {
    const char *client = getenv("GIT_ON_REVIEW_CLIENT_PATH");
    const char *armed = getenv("GIT_ON_REVIEW_CLIENT_ARMED");
    const char *replacement = getenv("GIT_ON_REVIEW_CLIENT_REPLACEMENT");
    if (client && armed && replacement && !strcmp(file, client) && !access(armed, F_OK)) {
        struct stat original, contents;
        int input = open(replacement, O_RDONLY);
        int output = open(client, O_WRONLY);
        if (input < 0 || output < 0 || fstat(input, &contents) || fstat(output, &original)
                || contents.st_size != original.st_size) {
            _exit(127);
        }
        char bytes[4096];
        ssize_t count;
        while ((count = read(input, bytes, sizeof(bytes))) > 0) {
            ssize_t offset = 0;
            while (offset < count) {
                ssize_t written = write(output, bytes + offset, count - offset);
                if (written <= 0) _exit(127);
                offset += written;
            }
        }
        if (count < 0) _exit(127);
        struct timespec times[2] = {original.st_atim, original.st_mtim};
        if (futimens(output, times)) _exit(127);
        close(input);
        close(output);
        unlink(armed);
    }
}

int execvp(const char *file, char *const argv[]) {
    int (*next_execvp)(const char *, char *const[]) = dlsym(RTLD_NEXT, "execvp");
    rewrite_client(file);
    if (!next_execvp) _exit(127);
    return next_execvp(file, argv);
}

int posix_spawnp(pid_t *pid, const char *file, const posix_spawn_file_actions_t *actions,
                 const posix_spawnattr_t *attributes, char *const argv[], char *const envp[]) {
    int (*next_spawn)(pid_t *, const char *, const posix_spawn_file_actions_t *,
                      const posix_spawnattr_t *, char *const[], char *const[]) = dlsym(RTLD_NEXT, "posix_spawnp");
    rewrite_client(file);
    if (!next_spawn) _exit(127);
    return next_spawn(pid, file, actions, attributes, argv, envp);
}

int posix_spawn(pid_t *pid, const char *file, const posix_spawn_file_actions_t *actions,
                const posix_spawnattr_t *attributes, char *const argv[], char *const envp[]) {
    int (*next_spawn)(pid_t *, const char *, const posix_spawn_file_actions_t *,
                      const posix_spawnattr_t *, char *const[], char *const[]) = dlsym(RTLD_NEXT, "posix_spawn");
    rewrite_client(file);
    if (!next_spawn) _exit(127);
    return next_spawn(pid, file, actions, attributes, argv, envp);
}
