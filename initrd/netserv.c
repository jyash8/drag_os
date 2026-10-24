// netserv: guest TCP server for RX-path isolation. Listens on 0.0.0.0:80,
// accepts one connection, reads a request, replies with a fixed body.
// Host reaches it via QEMU hostfwd (host :8080 -> guest :80).
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>

int main(void) {
    int ls = socket(AF_INET, SOCK_STREAM, 0);
    if (ls < 0) { printf("NETSERV_FAIL socket\n"); return 1; }
    int one = 1;
    setsockopt(ls, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons(80);
    sa.sin_addr.s_addr = htonl(0);
    if (bind(ls, (struct sockaddr *)&sa, sizeof(sa)) != 0) { printf("NETSERV_FAIL bind\n"); return 1; }
    if (listen(ls, 4) != 0) { printf("NETSERV_FAIL listen\n"); return 1; }
    printf("NETSERV_LISTENING\n");
    int c = accept(ls, 0, 0);
    if (c < 0) { printf("NETSERV_FAIL accept\n"); return 1; }
    printf("NETSERV_ACCEPTED\n");
    char req[256];
    ssize_t n = read(c, req, sizeof(req) - 1);
    if (n <= 0) { printf("NETSERV_FAIL read\n"); return 1; }
    req[n] = 0;
    printf("NETSERV_GOT %d bytes\n", (int)n);
    const char *resp = "HTTP/1.0 200 OK\r\nContent-Length: 9\r\n\r\nSERVTOKEN";
    size_t len = strlen(resp);
    if (write(c, resp, len) != (ssize_t)len) { printf("NETSERV_FAIL write\n"); return 1; }
    printf("NETSERV_SERVED\n");
    close(c);
    close(ls);
    return 0;
}
