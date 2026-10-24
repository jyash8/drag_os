#include <stdio.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>

int main() {
    int fl = fcntl(0, F_GETFL, 0);
    printf("F_GETFL = 0x%x\n", fl);
    int r = fcntl(0, F_SETFL, fl | O_NONBLOCK);
    printf("F_SETFL ret=%d errno=%d\n", r, errno);
    fl = fcntl(0, F_GETFL, 0);
    printf("F_GETFL after = 0x%x (O_NONBLOCK=0x%x)\n", fl, O_NONBLOCK);
    unsigned char c;
    errno = 0;
    r = read(0, &c, 1);
    printf("read ret=%d errno=%d\n", r, errno);
    if (r == -1) printf("EAGAIN=%d EWOULDBLOCK=%d\n", EAGAIN, EWOULDBLOCK);
    return 0;
}
