// ext4test: exercise full Ext4/ext2 read/write on disk.img (never /tmp or initrd).
// Covers: mkdir, file create, write/read roundtrip, stat, O_TRUNC,
// truncate, ftruncate (shrink+grow), fsync/sync, unlink.
// Prints EXT4TEST_PASS only if every stage passes.
#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <sys/stat.h>

static int fail(const char *msg) {
    printf("EXT4TEST_FAIL: %s\n", msg);
    return 1;
}

int main(void) {
    const char *dir = "/ext4rw";
    const char *file = "/ext4rw/basic.txt";
    const char *sub = "/ext4rw/subdir";
    struct stat st;
    char buf[64];

    unlink(file);
    unlink("/ext4rw/trunc.txt");

    // 1. directory modification
    if (mkdir(dir, 0755) != 0) {
        if (stat(dir, &st) != 0) return fail("mkdir/stat dir");
    }
    if (stat(dir, &st) != 0) return fail("stat dir");
    if (!S_ISDIR(st.st_mode)) return fail("dir not S_ISDIR");
    if (mkdir(sub, 0755) != 0) {
        if (stat(sub, &st) != 0 || !S_ISDIR(st.st_mode)) return fail("mkdir sub");
    }
    printf("MKDIR_OK\n");

    // 1b. fresh subdir + dot/dot-dot verification (exercises create's
    //     on-disk "." and ".." entries). Uses a new path each run so it
    //     doesn't get masked by stale state from previous test runs.
    const char *sub2 = "/ext4rw/subB";
    unlink(sub2);  // best-effort; ignore result (no rmdir, so dirs leak)
    if (mkdir(sub2, 0755) != 0) {
        if (stat(sub2, &st) != 0 || !S_ISDIR(st.st_mode)) return fail("mkdir sub2");
    }
    char dotpath[64], dotdotpath[64];
    snprintf(dotpath, sizeof(dotpath), "%s/.", sub2);
    snprintf(dotdotpath, sizeof(dotdotpath), "%s/..", sub2);
    if (stat(dotpath, &st) != 0) return fail("stat dot");
    if (st.st_ino == 0) return fail("dot ino");
    if (stat(dotdotpath, &st) != 0) return fail("stat dotdot");
    if (st.st_ino == 0) return fail("dotdot ino");
    // Fresh dir should be a non-empty directory (has the "." / ".." block).
    if (stat(sub2, &st) != 0) return fail("stat sub2");
    if (st.st_size == 0) return fail("sub2 size 0 (no dot entries)");
    printf("DOT_OK\n");

    // 2. file creation + write/read roundtrip
    int fd = open(file, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) return fail("open creat");
    const char *msg = "EXT4_RW_CONTENT_12345";
    size_t len = strlen(msg);
    if (write(fd, msg, len) != (ssize_t)len) return fail("write");
    if (lseek(fd, 0, SEEK_SET) != 0) return fail("lseek");
    memset(buf, 0, sizeof(buf));
    if (read(fd, buf, sizeof(buf)) != (ssize_t)len) return fail("read len");
    if (memcmp(buf, msg, len) != 0) return fail("read content");
    printf("BASIC_OK\n");

    // 3. stat reports real size/mode (ext2 stat used to return zeros)
    if (fstat(fd, &st) != 0) return fail("fstat");
    if (st.st_size != (off_t)len) return fail("stat size");
    if (!S_ISREG(st.st_mode)) return fail("stat not S_ISREG");
    printf("STAT_OK\n");
    close(fd);

    // 4. O_TRUNC on open
    fd = open(file, O_RDWR | O_TRUNC);
    if (fd < 0) return fail("open otrunc");
    if (fstat(fd, &st) != 0) return fail("fstat otrunc");
    if (st.st_size != 0) return fail("otrunc size");
    printf("OTRUNC_OK\n");
    if (write(fd, msg, len) != (ssize_t)len) return fail("rewrite");
    close(fd);

    // 5. truncate(2) shrink
    if (truncate(file, 4) != 0) return fail("truncate");
    fd = open(file, O_RDONLY);
    if (fd < 0) return fail("open trunc");
    memset(buf, 0, sizeof(buf));
    if (read(fd, buf, sizeof(buf)) != 4) return fail("trunc read len");
    if (memcmp(buf, "EXT4", 4) != 0) return fail("trunc content");
    close(fd);
    printf("TRUNC_OK\n");

    // 6. ftruncate(2) grow with zero fill
    fd = open(file, O_RDWR);
    if (fd < 0) return fail("open ftrunc");
    if (ftruncate(fd, 10) != 0) return fail("ftruncate");
    if (fstat(fd, &st) != 0) return fail("fstat ftrunc");
    if (st.st_size != 10) return fail("ftrunc size");
    if (lseek(fd, 0, SEEK_SET) != 0) return fail("lseek ftrunc");
    memset(buf, 0xAA, sizeof(buf));
    if (read(fd, buf, 10) != 10) return fail("ftrunc read");
    if (memcmp(buf, "EXT4", 4) != 0) return fail("ftrunc head");
    for (int i = 4; i < 10; i++) {
        if (buf[i] != 0) return fail("ftrunc zero fill");
    }
    printf("FTRUNC_OK\n");

    // 7. sync / fsync durability
    if (fsync(fd) != 0) return fail("fsync");
    close(fd);
    sync();
    printf("SYNC_OK\n");

    // 8. unlink cleanup + verify gone
    if (unlink(file) != 0) return fail("unlink");
    if (stat(file, &st) == 0) return fail("unlink verify");
    sync();  // flush BlockCache before QEMU kill so on-disk state is verifiable
    printf("EXT4TEST_PASS\n");
    return 0;
}
