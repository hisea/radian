/* Probes the Landlock ABI version and, one bit at a time, which access rights this
 * kernel accepts. Evidence for the capability table; no kernel headers needed. */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/syscall.h>

#define __NR_landlock_create_ruleset 444
#define LANDLOCK_CREATE_RULESET_VERSION (1U << 0)

struct landlock_ruleset_attr {
    unsigned long long handled_access_fs;
    unsigned long long handled_access_net;
    unsigned long long scoped;
};

static long ll_create_ruleset(const struct landlock_ruleset_attr *attr, size_t size, unsigned int flags) {
    return syscall(__NR_landlock_create_ruleset, attr, size, flags);
}

static const char *fs_names[] = {
    "EXECUTE", "WRITE_FILE", "READ_FILE", "READ_DIR", "REMOVE_DIR", "REMOVE_FILE",
    "MAKE_CHAR", "MAKE_DIR", "MAKE_REG", "MAKE_SOCK", "MAKE_FIFO", "MAKE_BLOCK",
    "MAKE_SYM", "REFER", "TRUNCATE", "IOCTL_DEV",
};
static const char *net_names[] = {"BIND_TCP", "CONNECT_TCP"};
static const char *scoped_names[] = {"ABSTRACT_UNIX_SOCKET", "SIGNAL"};

int main(void) {
    long abi = ll_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION);
    if (abi < 0) {
        printf("landlock_abi: unavailable (%s)\n", strerror(errno));
        return 1;
    }
    printf("landlock_abi: %ld\n", abi);

    struct landlock_ruleset_attr attr;
    for (unsigned i = 0; i < sizeof(fs_names) / sizeof(*fs_names); i++) {
        memset(&attr, 0, sizeof(attr));
        attr.handled_access_fs = 1ULL << i;
        long fd = ll_create_ruleset(&attr, sizeof(attr), 0);
        printf("fs %-20s %s\n", fs_names[i], fd < 0 ? "REJECTED" : "accepted");
        if (fd >= 0) close((int)fd);
    }
    for (unsigned i = 0; i < sizeof(net_names) / sizeof(*net_names); i++) {
        memset(&attr, 0, sizeof(attr));
        attr.handled_access_net = 1ULL << i;
        long fd = ll_create_ruleset(&attr, sizeof(attr), 0);
        printf("net %-19s %s\n", net_names[i], fd < 0 ? "REJECTED" : "accepted");
        if (fd >= 0) close((int)fd);
    }
    for (unsigned i = 0; i < sizeof(scoped_names) / sizeof(*scoped_names); i++) {
        memset(&attr, 0, sizeof(attr));
        attr.scoped = 1ULL << i;
        long fd = ll_create_ruleset(&attr, sizeof(attr), 0);
        printf("scoped %-16s %s\n", scoped_names[i], fd < 0 ? "REJECTED" : "accepted");
        if (fd >= 0) close((int)fd);
    }
    return 0;
}
