#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <sys/stat.h>

int main(void) {
    // tmpfs: create, write, read back
    int fd = open("/tmp/wl-test.txt", O_RDWR | O_CREAT, 0666);
    if (fd < 0) { perror("open /tmp"); return 1; }
    const char *msg = "hello tmpfs\n";
    write(fd, msg, strlen(msg));
    lseek(fd, 0, SEEK_SET);
    char buf[64] = {0};
    read(fd, buf, sizeof(buf));
    printf("tmpfs roundtrip: %s", buf);
    close(fd);

    // mkdir
    if (mkdir("/tmp/xdg-run", 0777) == 0) printf("mkdir ok\n");
    else perror("mkdir");

    // sysfs: uevent for event0
    fd = open("/sys/dev/char/13:64/uevent", O_RDONLY);
    if (fd < 0) { perror("open uevent"); return 1; }
    char ue[256] = {0};
    read(fd, ue, sizeof(ue) - 1);
    printf("uevent:\n%s---\n", ue);
    close(fd);

    // stat rdev of the input device
    struct stat st;
    if (stat("/dev/input/event0", &st) == 0) {
        printf("event0 rdev=%lu mode=%o\n", (unsigned long)st.st_rdev, st.st_mode);
    } else {
        perror("stat event0");
    }
    return 0;
}
