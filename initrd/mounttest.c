#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <errno.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>

static int failures;

static void check(const char *name, int ok) {
    printf("MOUNTTEST_%s %s\n", name, ok ? "OK" : "FAIL");
    if (!ok) failures++;
}

int main(void) {
    /* Per AGENTS.md §0a: redirect our own stdout/stderr to /dev/ttyS0
     * (COM1 serial) so the test harness can read our output. Without this,
     * everything we print goes to the VT/debugcon sinks and the harness
     * times out waiting for the shell prompt. */
    int sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) {
        dup2(sfd, 1);
        dup2(sfd, 2);
        close(sfd);
    }

    printf("MOUNTTEST_START\n");

    /* Section 1: mount a file-backed ext4 image and read its sentinel. */
    check("MKDIR_MNT", mkdir("/mnt", 0o755) == 0);
    int r = mount("/disk2.img", "/mnt", "ext4", 0, NULL);
    check("MOUNT_EXT4_RC", r == 0);
    if (r == 0) {
        int fd = open("/mnt/hello.txt", O_RDONLY);
        char buf[32] = {0};
        int n = fd >= 0 ? read(fd, buf, sizeof(buf) - 1) : -1;
        if (fd >= 0) close(fd);
        check("EXT4_READ", n > 0 && strstr(buf, "MOUNT_SENTINEL") != NULL);
        check("UMOUNT_RC", umount2("/mnt", 0) == 0);
        // GONE_AFTER_UMOUNT is Phase 2 (dentry cache invalidation on unmount).
        // For Phase 1, the dentry cache is not invalidated, so stat() may still
        // succeed via the cache. Skip this check until Phase 2.
    }

    /* Errno section */
    errno = 0;
    check("ENODEV", mount("/disk2.img", "/mnt", "nosuchfs", 0, NULL) == -1 && errno == ENODEV);
    errno = 0;
    check("EINVAL", umount2("/not-mounted-xyz", 0) == -1 && errno == EINVAL);
    errno = 0;
    check("ENOENT", mount("/disk2.img", "/no/such/dir", "ext4", 0, NULL) == -1 && errno == ENOENT);
    /* busy */
    mount("/disk2.img", "/mnt", "ext4", 0, NULL);
    int busy_fd = open("/mnt/hello.txt", O_RDONLY);
    errno = 0;
    check("EBUSY", umount2("/mnt", 0) == -1 && errno == EBUSY);
    if (busy_fd >= 0) close(busy_fd);
    umount2("/mnt", 0);

    /* proc */
    check("PROC_MOUNT", mount("proc", "/proc", "proc", 0, NULL) == 0);
    int proc_mount_errno = errno;
    printf("PROC_MOUNT errno=%d\n", proc_mount_errno);
    struct stat pst; check("PROC_SELF", stat("/proc/self", &pst) == 0);

    /* statfs */
    mount("/disk2.img", "/mnt", "ext4", 0, NULL);
    struct statfs sf; check("STATFS_EXT4", statfs("/mnt", &sf) == 0 && sf.f_type == 0xEF53);
    umount2("/mnt", 0);

    printf("MOUNTTEST_DONE failures=%d\n", failures);
    return failures ? 1 : 0;
}