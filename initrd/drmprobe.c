// drmprobe: libdrm device discovery + version check (Venus prerequisites).
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdlib.h>
#include <errno.h>
#include <string.h>
#include <stdarg.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <dirent.h>
#include <libdrm/drm.h>
#include <xf86drm.h>

#ifndef STATX_BASIC_STATS
#define STATX_BASIC_STATS 0x7ffU
#endif
struct my_statx {
    uint32_t stx_mask, stx_blksize; uint64_t stx_attributes;
    uint32_t stx_nlink, stx_uid, stx_gid; uint16_t stx_mode; uint16_t __sp0;
    uint64_t stx_ino, stx_size, stx_blocks, stx_attributes_mask;
    int64_t stx_atime_s; uint32_t stx_atime_ns; int32_t __r0;
    int64_t stx_btime_s; uint32_t stx_btime_ns; int32_t __r1;
    int64_t stx_ctime_s; uint32_t stx_ctime_ns; int32_t __r2;
    int64_t stx_mtime_s; uint32_t stx_mtime_ns; int32_t __r3;
    uint32_t stx_rdev_major, stx_rdev_minor, stx_dev_major, stx_dev_minor;
    uint64_t stx_mnt_id;
};

static void my_store(int *p, ...) {
    va_list ap;
    va_start(ap, p);
    *p = va_arg(ap, int);
    va_end(ap);
}
static unsigned long xhtou(const char *s) {
    unsigned long v = 0;
    if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) s += 2;
    while ((*s >= '0' && *s <= '9') || (*s >= 'a' && *s <= 'f') || (*s >= 'A' && *s <= 'F')) {
        v = v * 16 + (*s <= '9' ? *s - '0' : (*s & 0xDF) - 'A' + 10);
        s++;
    }
    return v;
}

/* Manual replica of libdrm's process_device for renderD128 (no scanf). */
static void manual_walk(void) {    struct stat sb;
    printf("W stat=%d ischr=%d rdev=%llx\n",
           stat("/dev/dri/renderD128", &sb),
           ((sb.st_mode & 0170000) == 0020000),
           (unsigned long long)sb.st_rdev);
    printf("W isdrm=%d\n", stat("/sys/dev/char/226:128/device/drm", &sb));
    char lb[512]; memset(lb, 0, sizeof(lb));
    ssize_t lr = readlink("/sys/dev/char/226:128/device/subsystem", lb, sizeof(lb) - 1);
    printf("W subsystem: ret=%zd tail=%.16s\n", lr, lr > 4 ? lb + lr - 4 : lb);
    char rp[4096];
    printf("W pci=%s\n", realpath("/sys/dev/char/226:128/device", rp) ? rp : "NULL");
    int f = open("/sys/devices/pci0000:00/0000:00:02.0/uevent", O_RDONLY);
    if (f >= 0) {
        char b[512]; memset(b, 0, sizeof(b));
        ssize_t r = read(f, b, sizeof(b) - 1);
        const char *k = strstr(b, "PCI_SLOT_NAME=");
        printf("W uevent_read=%zd slot=%.12s\n", r, k ? k + 14 : "MISSING");
        close(f);
    } else printf("W uevent_open FAIL\n");
    const char *attrs[] = { "vendor", "device", "subsystem_vendor", "subsystem_device" };
    for (int i = 0; i < 4; i++) {
        char p[256]; snprintf(p, sizeof(p), "/sys/devices/pci0000:00/0000:00:02.0/%s", attrs[i]);
        f = open(p, O_RDONLY);
        unsigned long v = 0xBEEF; int ok = 0;
        if (f >= 0) {
            char b[32]; memset(b, 0, sizeof(b));
            if (read(f, b, sizeof(b) - 1) > 0) { v = xhtou(b); ok = 1; }
            close(f);
        }
        printf("W %s ok=%d val=%lx\n", attrs[i], ok, v);
    }
}

int main(void) {
    struct my_statx sx; memset(&sx, 0xAA, sizeof(sx));
    long rc = syscall(SYS_statx, -100, "/dev/dri/renderD128", 0, STATX_BASIC_STATS, &sx);
    printf("RAW_STATX rc=%ld mask=%x mode=%x rdev=%u:%u size=%llu\n", rc,
           sx.stx_mask, sx.stx_mode, sx.stx_rdev_major, sx.stx_rdev_minor,
           (unsigned long long)sx.stx_size);
    struct stat nst;
    memset(&nst, 0xAA, sizeof(nst));
    long sret = (long)stat("/dev/dri/renderD128", &nst);
    printf("FSTAT_RENDER ret=%ld dev=%llx ino=%llu nlink=%llu mode=%o uid=%u gid=%u rdev=%llx size=%lld blksz=%lld\n",
           sret, (unsigned long long)nst.st_dev, (unsigned long long)nst.st_ino,
           (unsigned long long)nst.st_nlink, (unsigned)nst.st_mode,
           nst.st_uid, nst.st_gid, (unsigned long long)nst.st_rdev,
           (long long)nst.st_size, (long long)nst.st_blksize);
    printf("FSTAT_RENDER rdev=%llx mode=%o\n",
           (stat("/dev/dri/renderD128", &nst) == 0) ? (unsigned long long)nst.st_rdev : 999ULL,
           (unsigned)nst.st_mode);
    printf("FSTAT_CARD rdev=%llx\n",
           (stat("/dev/dri/card0", &nst) == 0) ? (unsigned long long)nst.st_rdev : 999ULL);
    printf("STAT_DRM_SUBDIR=%d\n", stat("/sys/dev/char/226:128/device/drm", &nst));
    printf("STAT_DRM_SUBDIR0=%d\n", stat("/sys/dev/char/226:0/device/drm", &nst));
    const char *attrs[] = { "vendor", "device", "subsystem_vendor", "subsystem_device" };
    for (int i = 0; i < 4; i++) {
        char p[256]; snprintf(p, sizeof(p), "/sys/devices/pci0000:00/0000:00:02.0/%s", attrs[i]);
        int f = open(p, O_RDONLY);
        unsigned long v = 0xBEEF; int ok = 0;
        if (f >= 0) {
            char b[32]; memset(b, 0, sizeof(b));
            ssize_t r = read(f, b, sizeof(b) - 1);
            if (r > 0) { v = xhtou(b); ok = 1; }
            close(f);
        }
        printf("ATTR_%s ok=%d val=%lx\n", attrs[i], ok, v);
    }
    int x = -99;
    my_store(&x, 42);
    printf("MY_VARARGS x=%d (want 42)\n", x);
    int y = -99; int *q = &y; *q = 43;
    printf("DIRECT_STORE y=%d (want 43)\n", y);
    unsigned cv = 0xDEAD;
    printf("CONST_SSCANF=%d val=%x\n", sscanf("0x1af4", "%x", &cv), cv);
    cv = 0xDEAD;
    printf("DEC_SSCANF=%d val=%x\n", sscanf("123", "%x", &cv), cv);
    int di = -99, nn = -99; char cc = 0;
    printf("MIX_SSCANF=%d di=%d cc=%c nn=%d\n", sscanf("42 q", "%d %c%n", &di, &cc, &nn), di, cc, nn);
    char ss[16]; memset(ss, 0xAA, sizeof(ss));
    printf("STR_SSCANF=%d s=%s\n", sscanf("hello", "%s", ss), ss);
    cv = 0xDEAD;
    printf("CONST_STRTOUL=%lx\n", strtoul("0x1af4", NULL, 16));
    char rp[4096];
    printf("REALPATH_DEV=%s\n", realpath("/sys/dev/char/226:128/device", rp) ? rp : "NULL");
    printf("REALPATH_CLASS=%s\n", realpath("/sys/class/drm/renderD128", rp) ? rp : "NULL");
    FILE *    f = NULL; // TEMP-DIAG silenced
    printf("OPEN_VENDOR=%s\n", f ? "OK" : strerror(errno));
    if (f) {
        unsigned v = 0;
        char sb[16]; memset(sb, 0xAA, sizeof(sb));
        size_t n = fread(sb, 1, 15, f);
        printf("FREAD=%zu bytes: %02x %02x %02x %02x\n", n,
               (unsigned char)sb[0], (unsigned char)sb[1],
               (unsigned char)sb[2], (unsigned char)sb[3]);
        int sv = -99; unsigned suv = 0;
        printf("SSCANF_BUF=%d val=%x\n", sscanf(sb, "%x", &suv), suv);
        printf("SCANF_VENDOR=%d val=%x\n", fscanf(f, "%x", &v), v);
        fclose(f);
    }
    FILE *w = fopen("/tmp/t", "w");
    if (w) { fputs("0x1af4\n", w); fclose(w); }
    f = fopen("/tmp/t", "r");
    if (f) { char fb[16]; memset(fb,0,sizeof(fb)); size_t fn=fread(fb,1,sizeof(fb)-1,f); printf("FREAD_TMP=%zu val=%lx\n", fn, fn>0?xhtou(fb):0UL); fclose(f); }
    else printf("OPEN_TMP=%s\n", strerror(errno));
    int vfd = open("/sys/devices/pci0000:00/0000:00:02.0/vendor", O_RDONLY);
    printf("OPEN2=%d\n", vfd);
    if (vfd >= 0) {
        unsigned char b[8192]; memset(b, 0xAA, sizeof(b));
        ssize_t r1 = read(vfd, b, 64);
        printf("READ64=%zd bytes:", r1);
        for (ssize_t i = 0; i < r1 && i < 10; i++) printf(" %02x", b[i]);
        printf("\n");
        close(vfd);
    }
    vfd = open("/sys/devices/pci0000:00/0000:00:02.0/vendor", O_RDONLY);
    if (vfd >= 0) {
        unsigned char b[8192]; memset(b, 0xAA, sizeof(b));
        ssize_t r1 = read(vfd, b, 4096);
        printf("READ4096=%zd bytes:", r1);
        for (ssize_t i = 0; i < r1 && i < 10; i++) printf(" %02x", b[i]);
        printf("\n");
        close(vfd);
    }
    vfd = open("/sys/devices/pci0000:00/0000:00:02.0/vendor", O_RDONLY);
    if (vfd >= 0) {
        struct stat st; memset(&st, 0, sizeof(st));
        printf("STAT=%d size=%lld\n", stat("/sys/devices/pci0000:00/0000:00:02.0/vendor", &st), (long long)st.st_size);
        close(vfd);
    }
    char lb[512]; memset(lb, 0, sizeof(lb));
    ssize_t lr = readlink("/sys/dev/char/226:128/device/subsystem", lb, sizeof(lb) - 1);
    printf("READLINK_SUBSYS=%zd target=%.80s err=%d\n", lr, lb, errno);
    memset(lb, 0, sizeof(lb));
    lr = readlink("/sys/dev/char/226:128", lb, sizeof(lb) - 1);
    printf("READLINK_CHAR=%zd target=%.80s err=%d\n", lr, lb, errno);
    FILE *fu = fopen("/sys/dev/char/226:128/device/uevent", "r");
    printf("UEVENT_VIA_LINK=%s\n", fu ? "OK" : strerror(errno));
    if (fu) {
        char ub[512]; memset(ub, 0, sizeof(ub));
        size_t un = fread(ub, 1, sizeof(ub) - 1, fu);
        printf("UEVENT_BYTES=%zu first=%.60s\n", un, ub);
        fclose(fu);
    }
    drmDevicePtr devs[8];
    manual_walk();
    DIR *dd = opendir("/dev/dri");
    printf("OPENDIR=%s\n", dd ? "OK" : "FAIL");
    if (dd) {
        struct dirent *de;
        while ((de = readdir(dd)))
            printf("ENT name='%s' type=%d\n", de->d_name, de->d_type);
        closedir(dd);
    }
    int gfd = open("/dev/dri", O_RDONLY | O_DIRECTORY);
    printf("GOPEN=%d\n", gfd);
    if (gfd >= 0) {
        static unsigned char gb[1024]; memset(gb, 0xAA, sizeof(gb));
        long gr = syscall(SYS_getdents64, gfd, gb, sizeof(gb));
        printf("GETDENTS=%ld first32:", gr);
        for (int i = 0; i < gr && i < 32; i++) printf(" %02x", gb[i]);
        printf("\n");
        if (gr > 0) {
            /* linux_dirent64: ino u64, off i64, reclen u16, type u8, name[] */
            unsigned long long ino = 0; long long off = 0; unsigned short rl = 0; unsigned char ty = 0;
            memcpy(&ino, gb, 8); memcpy(&off, gb + 8, 8); memcpy(&rl, gb + 16, 2); ty = gb[18];
            printf("REC0 ino=%llu off=%lld reclen=%u type=%u name=%.16s\n",
                   ino, off, rl, ty, gb + 19);
        }
        close(gfd);
    }
    /* replicate libdrm sysfs_uevent_get for PCI_SLOT_NAME */
    {
        FILE *fp = fopen("/sys/devices/pci0000:00/0000:00:02.0/uevent", "r");
        printf("GL_OPEN=%s\n", fp ? "OK" : "FAIL");
        if (fp) {
            char *line = NULL; size_t sz = 0; ssize_t nr; int it = 0;
            const char *want = "PCI_SLOT_NAME"; size_t wl = strlen(want);
            while ((nr = getline(&line, &sz, fp)) >= 0 && it < 10) {
                it++;
                printf("GL_LINE%d nr=%zd match=%d %.30s\n", it, nr,
                       (strncmp(line, want, wl) == 0 && line[wl] == '='), line);
                if (strncmp(line, want, wl) == 0 && line[wl] == '=') break;
            }
            printf("GL_DONE iters=%d\n", it);
            free(line);
            fclose(fp);
        }
    }
    int n = drmGetDevices2(0, devs, 8);
    printf("DRM_GETDEVICES2=%d\n", n);
    for (int i = 0; i < n && i < 8; i++) {
        printf("DEV%d bustype=%d nodes=%u primary='%s' render='%s'\n", i,
               devs[i]->bustype, devs[i]->available_nodes,
               (devs[i]->available_nodes & (1 << DRM_NODE_PRIMARY)) ? devs[i]->nodes[DRM_NODE_PRIMARY] : "-",
               (devs[i]->available_nodes & (1 << DRM_NODE_RENDER)) ? devs[i]->nodes[DRM_NODE_RENDER] : "-");
        if (devs[i]->bustype == DRM_BUS_PCI)
            printf("DEV%d pci=%04x:%04x\n", i, devs[i]->deviceinfo.pci->vendor_id,
                   devs[i]->deviceinfo.pci->device_id);
    }
    if (n > 0) drmFreeDevices(devs, n);

    int fd = open("/dev/dri/renderD128", O_RDWR);
    printf("OPEN_RENDER=%d\n", fd);
    if (fd >= 0) {
        drmVersionPtr v = drmGetVersion(fd);
        if (v) {
            printf("DRM_VERSION name='%s' major=%d minor=%d\n",
                   v->name ? v->name : "(null)", v->version_major, v->version_minor);
            drmFreeVersion(v);
        } else {
            printf("DRM_VERSION=NULL\n");
        }
        close(fd);
    }
    printf("DRMPROBE_DONE\n");
    return 0;
}
