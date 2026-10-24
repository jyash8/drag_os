// drmspy: LD_PRELOAD shim logging fopen/fscanf/sscanf/stat around libdrm.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdarg.h>
#include <unistd.h>
#include <dlfcn.h>
#include <string.h>
#include <sys/stat.h>
static void logline(const char *s) {
    int fd = 2;
    (void)!write(fd, s, strlen(s));
    (void)!write(fd, "\n", 1);
}

int sscanf_disabled(const char *str, const char *fmt, ...) {
    static int (*real_vsscanf)(const char *, const char *, va_list) = NULL;
    if (!real_vsscanf) real_vsscanf = dlsym(RTLD_NEXT, "vsscanf");
    char b[300];
    snprintf(b, sizeof(b), "SPY sscanf fmt='%s' in='%.40s'", fmt, str);
    logline(b);
    va_list ap;
    va_start(ap, fmt);
    int r = real_vsscanf(str, fmt, ap);
    va_end(ap);
    snprintf(b, sizeof(b), "SPY sscanf -> %d", r);
    logline(b);
    return r;
}

/* GCC16/C23 redirects scanf-family to __isoc23_* — interpose those too. */
int __isoc23_sscanf_disabled(const char *str, const char *fmt, ...) {
    static int (*real_vsscanf)(const char *, const char *, va_list) = NULL;
    if (!real_vsscanf) real_vsscanf = dlsym(RTLD_NEXT, "vsscanf");
    char b[300];
    snprintf(b, sizeof(b), "SPY sscanf fmt='%s' in='%.40s'", fmt, str);
    logline(b);
    va_list ap;
    va_start(ap, fmt);
    int r = real_vsscanf(str, fmt, ap);
    va_end(ap);
    snprintf(b, sizeof(b), "SPY sscanf -> %d", r);
    logline(b);
    return r;
}

int fscanf_disabled(FILE *f, const char *fmt, ...) {
    static int (*real_vfscanf)(FILE *, const char *, va_list) = NULL;
    if (!real_vfscanf) real_vfscanf = dlsym(RTLD_NEXT, "vfscanf");
    int fd = fileno(f);
    char p[64] = "?";
    if (fd >= 0) {
        char l[64];
        snprintf(l, sizeof(l), "/proc/self/fd/%d", fd);
        ssize_t n = readlink(l, p, sizeof(p) - 1);
        if (n > 0) p[n] = 0;
    }
    char b[300];
    snprintf(b, sizeof(b), "SPY fscanf fmt='%s' fdpath='%s'", fmt, p);
    logline(b);
    va_list ap;
    va_start(ap, fmt);
    int r = real_vfscanf(f, fmt, ap);
    va_end(ap);
    snprintf(b, sizeof(b), "SPY fscanf -> %d", r);
    logline(b);
    return r;
}

int __isoc23_fscanf_disabled(FILE *f, const char *fmt, ...) {
    static int (*real_vfscanf)(FILE *, const char *, va_list) = NULL;
    if (!real_vfscanf) real_vfscanf = dlsym(RTLD_NEXT, "vfscanf");
    int fd = fileno(f);
    char p[64] = "?";
    if (fd >= 0) {
        char l[64];
        snprintf(l, sizeof(l), "/proc/self/fd/%d", fd);
        ssize_t n = readlink(l, p, sizeof(p) - 1);
        if (n > 0) p[n] = 0;
    }
    char b[300];
    snprintf(b, sizeof(b), "SPY fscanf fmt='%s' fdpath='%s'", fmt, p);
    logline(b);
    va_list ap;
    va_start(ap, fmt);
    int r = real_vfscanf(f, fmt, ap);
    va_end(ap);
    snprintf(b, sizeof(b), "SPY fscanf -> %d", r);
    logline(b);
    return r;
}

int stat(const char *path, struct stat *buf) {    static int (*real_stat)(const char *, struct stat *) = NULL;
    if (!real_stat) real_stat = dlsym(RTLD_NEXT, "stat");
    int r = real_stat(path, buf);
    char b[300];
    snprintf(b, sizeof(b), "SPY stat '%s' -> %d mode=%o rdev=%llx", path, r,
             r == 0 ? (unsigned)buf->st_mode : 0,
             r == 0 ? (unsigned long long)buf->st_rdev : 0ULL);
    logline(b);
    return r;
}

/* TEMP-DIAG: trace reads on drm/sysfs paths */
ssize_t read(int fd, void *buf, size_t count) {
    static ssize_t (*real_read)(int, void *, size_t) = NULL;
    if (!real_read) real_read = dlsym(RTLD_NEXT, "read");
    ssize_t r = real_read(fd, buf, count);
    char l[64], p[128] = "?";
    snprintf(l, sizeof(l), "/proc/self/fd/%d", fd);
    ssize_t n = readlink(l, p, sizeof(p) - 1);
    /* use raw syscall to avoid recursion */
    extern ssize_t __libc_read(int, void *, size_t);
    if (n > 0) {
        p[n] = 0;
        if (strstr(p, "vendor") || strstr(p, "drm/") || strstr(p, "uevent")) {
            char b[220];
            int m = snprintf(b, sizeof(b), "SPY read fd=%d path='%s' count=%zu -> %zd first=%.12s", fd, p, count, r, r > 0 ? (char *)buf : "");
            (void)!write(2, b, m);
            (void)!write(2, "\n", 1);
        }
    }
    return r;
}

/* TEMP-DIAG: va_list level hooks (catch all scanf variants) */
int vsscanf(const char *str, const char *fmt, va_list ap) {
    static int (*real_vsscanf)(const char *, const char *, va_list) = NULL;
    if (!real_vsscanf) real_vsscanf = dlsym(RTLD_NEXT, "vsscanf");
    char b[300];
    snprintf(b, sizeof(b), "SPY vsscanf fmt='%s' in='%.40s'", fmt, str);
    logline(b);
    va_list aq;
    va_copy(aq, ap);
    int r = real_vsscanf(str, fmt, aq);
    va_end(aq);
    snprintf(b, sizeof(b), "SPY vsscanf -> %d", r);
    logline(b);
    return r;
}
int vfscanf(FILE *f, const char *fmt, va_list ap) {
    static int (*real_vfscanf)(FILE *, const char *, va_list) = NULL;
    if (!real_vfscanf) real_vfscanf = dlsym(RTLD_NEXT, "vfscanf");
    int fd = fileno(f);
    char p[64] = "?";
    if (fd >= 0) {
        char l[64];
        snprintf(l, sizeof(l), "/proc/self/fd/%d", fd);
        ssize_t n = readlink(l, p, sizeof(p) - 1);
        if (n > 0) p[n] = 0;
    }
    char b[300];
    snprintf(b, sizeof(b), "SPY vfscanf fmt='%s' fdpath='%s'", fmt, p);
    logline(b);
    va_list aq;
    va_copy(aq, ap);
    int r = real_vfscanf(f, fmt, aq);
    va_end(aq);
    snprintf(b, sizeof(b), "SPY vfscanf -> %d", r);
    logline(b);
    return r;
}

/* TEMP-DIAG: catch abort() with a backtrace */
#include <execinfo.h>
void abort(void) {
    static void (*real_abort)(void) = NULL;
    void *bt[32];
    int n = backtrace(bt, 32);
    backtrace_symbols_fd(bt, n, 2);
    if (!real_abort) real_abort = dlsym(RTLD_NEXT, "abort");
    real_abort();
    _exit(99);
}
