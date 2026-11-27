#define _GNU_SOURCE
#include <dirent.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int is_final_read(int file, const char *target) {
    char descriptor[48];
    char path[4096];
    snprintf(descriptor, sizeof(descriptor), "/proc/self/fd/%d", file);
    ssize_t length = readlink(descriptor, path, sizeof(path) - 1);
    if (length < 0) {
        return 0;
    }
    path[length] = '\0';
    if (strcmp(path, target) != 0) {
        return 0;
    }
    *strrchr(path, '/') = '\0';
    DIR *parent = opendir(path);
    if (parent == NULL) {
        return 0;
    }
    int staged = 0;
    struct dirent *entry;
    while ((entry = readdir(parent)) != NULL) {
        if (strncmp(entry->d_name, ".tmp", 4) == 0) {
            staged = 1;
            break;
        }
    }
    closedir(parent);
    return staged;
}

/* When the staged save exists, complete a second process's write after EOF. */
ssize_t read(int file, void *buffer, size_t size) {
    ssize_t (*read_file)(int, void *, size_t) = dlsym(RTLD_NEXT, "read");
    ssize_t count = read_file(file, buffer, size);
    const char *target = getenv("STROLL_AUDIT_SAVE_TARGET");
    static atomic_flag competed = ATOMIC_FLAG_INIT;
    if (count == 0 && size != 0 && target != NULL && is_final_read(file, target)
        && !atomic_flag_test_and_set(&competed)) {
        static const char external[] = "alpha line\nexternal beta line\ngamma line\n";
        pid_t writer = fork();
        if (writer == 0) {
            int output = open(target, O_WRONLY | O_TRUNC);
            if (output < 0 || write(output, external, sizeof(external) - 1) != sizeof(external) - 1
                || fsync(output) != 0 || close(output) != 0) {
                _exit(1);
            }
            _exit(0);
        }
        int status;
        if (writer > 0 && waitpid(writer, &status, 0) == writer && WIFEXITED(status) && WEXITSTATUS(status) == 0) {
            fputs("Audit writer finished before rename\n", stderr);
        }
    }
    return count;
}
