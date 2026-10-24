/* Does a write to a /tmp file become visible to a subsequent read?
 * zellij's log file opens, is written, and reads back empty. */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>

int main(void)
{
    const char *p = "/tmp/wtest.txt";
    const char *msg = "hello-from-wtest\n";
    int fd = open(p, O_CREAT | O_RDWR | O_TRUNC, 0644);
    printf("open(%s) = %d errno=%d\n", p, fd, errno);
    if (fd < 0) return 1;

    ssize_t w = write(fd, msg, strlen(msg));
    printf("write(%zu) = %zd errno=%d\n", strlen(msg), w, errno);

    char buf[64] = {0};
    ssize_t r = pread(fd, buf, sizeof(buf) - 1, 0);
    printf("pread(0) = %zd '%s'\n", r, r > 0 ? buf : "");

    /* Reopen by path: does a fresh handle see the data? */
    int fd2 = open(p, O_RDONLY);
    if (fd2 >= 0) {
        char b2[64] = {0};
        ssize_t r2 = read(fd2, b2, sizeof(b2) - 1);
        printf("reopen+read = %zd '%s'\n", r2, r2 > 0 ? b2 : "");
        close(fd2);
    } else {
        printf("reopen failed errno=%d\n", errno);
    }
    printf("TMPFSW_DONE\n");
    return 0;
}
