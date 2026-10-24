// netprobe: validate VirtIO-Net + smoltcp TCP against the QEMU host.
// Connects to 10.0.2.2:18080 (host python http.server), GETs /token.txt,
// expects body "NETTOKEN". Prints NETPROBE_PASS iff the full path works.
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>

static int fail(const char *msg) { printf("NETPROBE_FAIL: %s\n", msg); return 1; }

int main(void) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (fd < 0) return fail("socket");

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons(18080);
    sa.sin_addr.s_addr = htonl(0x0A000202); // 10.0.2.2

    printf("NETPROBE_CONNECTING\n");
    if (connect(fd, (struct sockaddr *)&sa, sizeof(sa)) != 0)
        return fail("connect");

    printf("NETPROBE_CONNECTED\n");
    const char *req = "GET /token.txt HTTP/1.0\r\nHost: x\r\n\r\n";
    size_t reqlen = strlen(req);
    if (write(fd, req, reqlen) != (ssize_t)reqlen) return fail("send");

    char buf[512];
    size_t total = 0;
    ssize_t n;
    while ((n = read(fd, buf + total, sizeof(buf) - 1 - total)) > 0) {
        total += (size_t)n;
        if (total >= sizeof(buf) - 1) break;
    }
    if (n < 0) return fail("recv");
    buf[total] = 0;
    printf("NETPROBE_GOT %u bytes\n", (unsigned)total);

    if (strstr(buf, "NETTOKEN") == NULL) return fail("token not found");
    printf("NETPROBE_PASS\n");
    close(fd);
    return 0;
}
