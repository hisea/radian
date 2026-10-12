/* Applies a Landlock ruleset and then execs a command.
 *
 *   llsandbox --rw DIR --ro DIR ... [--deny-net] -- CMD [ARGS...]
 *
 * Read/execute are allowed only on the --ro/--rw paths; write is allowed only on --rw.
 * With --deny-net, TCP bind/connect are handled with no allow rule, so both are denied.
 * No kernel headers are required beyond syscall numbers. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/prctl.h>
#include <sys/stat.h>
#include <sys/syscall.h>

#define __NR_landlock_create_ruleset 444
#define __NR_landlock_add_rule 445
#define __NR_landlock_restrict_self 446

#define LANDLOCK_RULE_PATH_BENEATH 1
#define LANDLOCK_CREATE_RULESET_VERSION (1U << 0)

struct landlock_ruleset_attr {
    unsigned long long handled_access_fs;
    unsigned long long handled_access_net;
    unsigned long long scoped;
};
struct landlock_path_beneath_attr {
    unsigned long long allowed_access;
    int parent_fd;
} __attribute__((packed));

#define FS_EXECUTE (1ULL << 0)
#define FS_WRITE_FILE (1ULL << 1)
#define FS_READ_FILE (1ULL << 2)
#define FS_READ_DIR (1ULL << 3)
#define FS_REMOVE_DIR (1ULL << 4)
#define FS_REMOVE_FILE (1ULL << 5)
#define FS_MAKE_CHAR (1ULL << 6)
#define FS_MAKE_DIR (1ULL << 7)
#define FS_MAKE_REG (1ULL << 8)
#define FS_MAKE_SOCK (1ULL << 9)
#define FS_MAKE_FIFO (1ULL << 10)
#define FS_MAKE_BLOCK (1ULL << 11)
#define FS_MAKE_SYM (1ULL << 12)
#define FS_REFER (1ULL << 13)
#define FS_TRUNCATE (1ULL << 14)
#define FS_IOCTL_DEV (1ULL << 15)
#define FS_ALL ((1ULL << 16) - 1)

#define NET_BIND_TCP (1ULL << 0)
#define NET_CONNECT_TCP (1ULL << 1)

static void die(const char *what) {
    fprintf(stderr, "llsandbox: %s: %s\n", what, strerror(errno));
    exit(2);
}

/* Rights that only make sense for a directory rule (Landlock rejects them on a file). */
#define FS_DIR_ONLY (FS_READ_DIR | FS_REMOVE_DIR | FS_REMOVE_FILE | FS_MAKE_CHAR | FS_MAKE_DIR | \
                     FS_MAKE_REG | FS_MAKE_SOCK | FS_MAKE_FIFO | FS_MAKE_BLOCK | FS_MAKE_SYM | FS_REFER)

static void add_path_rule(int ruleset_fd, const char *path, unsigned long long access) {
    int fd = open(path, O_PATH | O_CLOEXEC);
    if (fd < 0) die(path);
    // Directory-only rights are EINVAL on a non-directory, and EXECUTE is EINVAL on a
    // non-regular file (e.g. the character device /dev/null).
    struct stat st;
    if (stat(path, &st) < 0) die(path);
    if (S_ISDIR(st.st_mode)) {
        /* full set */
    } else if (S_ISREG(st.st_mode)) {
        access &= ~FS_DIR_ONLY;
    } else {
        access &= ~(FS_DIR_ONLY | FS_EXECUTE);
    }
    struct landlock_path_beneath_attr attr = {.allowed_access = access, .parent_fd = fd};
    if (syscall(__NR_landlock_add_rule, ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &attr, 0) < 0) {
        fprintf(stderr, "llsandbox: landlock_add_rule(%s, access=0x%llx): %s\n", path, access, strerror(errno));
        exit(2);
    }
    close(fd);
}

int main(int argc, char **argv) {
    unsigned long long fs = FS_ALL;
    unsigned long long net = 0;
    const char *rw[64], *ro[64];
    int n_rw = 0, n_ro = 0;
    int i = 1;
    for (; i < argc; i++) {
        if (strcmp(argv[i], "--") == 0) { i++; break; }
        if (strcmp(argv[i], "--rw") == 0 && i + 1 < argc) rw[n_rw++] = argv[++i];
        else if (strcmp(argv[i], "--ro") == 0 && i + 1 < argc) ro[n_ro++] = argv[++i];
        else if (strcmp(argv[i], "--deny-net") == 0) net = NET_BIND_TCP | NET_CONNECT_TCP;
        else { fprintf(stderr, "llsandbox: bad argument %s\n", argv[i]); return 2; }
    }
    if (i >= argc) { fprintf(stderr, "llsandbox: no command\n"); return 2; }

    struct landlock_ruleset_attr attr = {.handled_access_fs = fs, .handled_access_net = net, .scoped = 0};
    int ruleset_fd = syscall(__NR_landlock_create_ruleset, &attr, sizeof(attr), 0);
    if (ruleset_fd < 0) die("landlock_create_ruleset");

    for (int k = 0; k < n_ro; k++) add_path_rule(ruleset_fd, ro[k], FS_EXECUTE | FS_READ_FILE | FS_READ_DIR);
    for (int k = 0; k < n_rw; k++) add_path_rule(ruleset_fd, rw[k], FS_ALL);

    if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0) die("prctl(NO_NEW_PRIVS)");
    if (syscall(__NR_landlock_restrict_self, ruleset_fd, 0) < 0) die("landlock_restrict_self");
    close(ruleset_fd);

    execvp(argv[i], &argv[i]);
    die("execvp");
}
