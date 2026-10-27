/* Proof-only unlinkat interposer. The target's identity is pinned at process
 * startup from the exact fixture path, before Barge's private captures rename it.
 * Every other object and operation passes through to libc unchanged. */
#include <sys/stat.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static dev_t target_dev;
static ino_t target_ino;
static int ready;
static int remaining;
static const char *target_path;
static const char *log_path;
static const char *pause_path;
static const char *pause_log;
static dev_t pause_dev;
static ino_t pause_ino;
static int pause_ready;

__attribute__((constructor)) static void configure(void) {
    struct stat st;
    pause_path = getenv("BARGE_PROOF_PAUSE_PATH");
    pause_log = getenv("BARGE_PROOF_PAUSE_LOG");
    if (pause_path && pause_path[0] == '/' && pause_log && pause_log[0] == '/'
        && lstat(pause_path, &st) == 0 && S_ISDIR(st.st_mode)) {
        pause_dev = st.st_dev;
        pause_ino = st.st_ino;
        pause_ready = 1;
    }
    target_path = getenv("BARGE_PROOF_ENOTEMPTY_PATH");
    log_path = getenv("BARGE_PROOF_ENOTEMPTY_LOG");
    const char *count = getenv("BARGE_PROOF_ENOTEMPTY_COUNT");
    if (!target_path || target_path[0] != '/' || !log_path || log_path[0] != '/'
        || !count || strcmp(count, "3") != 0 || lstat(target_path, &st) != 0
        || !S_ISDIR(st.st_mode)) return;
    target_dev = st.st_dev;
    target_ino = st.st_ino;
    remaining = 3;
    ready = 1;
}

static int proof_unlinkat(int dirfd, const char *path, int flags) {
    struct stat st;
    if (pause_ready && fstat(dirfd, &st) == 0 && st.st_dev == pause_dev &&
        st.st_ino == pause_ino && S_ISDIR(st.st_mode)) {
        int fd = open(pause_log, O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW, 0600);
        if (fd >= 0) {
            char line[2048];
            int length = snprintf(line, sizeof(line), "%s\t%llu\t%llu\n", pause_path,
                (unsigned long long)pause_dev, (unsigned long long)pause_ino);
            if (length > 0 && (size_t)length < sizeof(line) && write(fd, line, (size_t)length) == length) {
                (void)close(fd);
                for (;;) (void)pause();
            }
            (void)close(fd);
        }
    }
    if (ready && remaining > 0 && flags == AT_REMOVEDIR &&
        fstatat(dirfd, path, &st, AT_SYMLINK_NOFOLLOW) == 0 &&
        st.st_dev == target_dev && st.st_ino == target_ino && S_ISDIR(st.st_mode)) {
        int fd = open(log_path, O_WRONLY | O_APPEND | O_CREAT | O_NOFOLLOW, 0600);
        if (fd >= 0) {
            char line[2048];
            int length = snprintf(line, sizeof(line), "%s\t%llu\t%llu\n", target_path,
                (unsigned long long)target_dev, (unsigned long long)target_ino);
            if (length > 0 && (size_t)length < sizeof(line)) (void)write(fd, line, (size_t)length);
            (void)close(fd);
        }
        --remaining;
        errno = ENOTEMPTY;
        return -1;
    }
    return unlinkat(dirfd, path, flags);
}

__attribute__((used)) static struct {
    const void *replacement;
    const void *replacee;
} interpose_unlinkat __attribute__((section("__DATA,__interpose"))) = {
    (const void *)proof_unlinkat, (const void *)unlinkat
};
