// dnstest: real UDP DNS via smoltcp. Sends an A query for example.com to
// the nameserver in argv[1] (default 10.0.2.3, QEMU user-net resolver),
// parses the first A answer, prints DNSTEST_PASS with the IP.
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <netinet/in.h>
#include <arpa/inet.h>

static int fail(const char *msg) { printf("DNSTEST_FAIL: %s\n", msg); return 1; }

int main(int argc, char **argv) {
    const char *ns = (argc > 1) ? argv[1] : "10.0.2.3";
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) return fail("socket");

    struct timeval tv = { .tv_sec = 8, .tv_usec = 0 };
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons(53);
    if (inet_pton(AF_INET, ns, &sa.sin_addr) != 1) return fail("pton");

    // A query for example.com, txid 0x1234, RD=1
    static const unsigned char q[] = {
        0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x07, 'e', 'x', 'a', 'm', 'p', 'l', 'e', 0x03, 'c', 'o', 'm', 0x00,
        0x00, 0x01, 0x00, 0x01,
    };
    if (sendto(fd, q, sizeof(q), 0, (struct sockaddr *)&sa, sizeof(sa)) != (ssize_t)sizeof(q))
        return fail("sendto");
    printf("DNSTEST_SENT\n");

    unsigned char resp[512];
    struct sockaddr_in from;
    socklen_t fromlen = sizeof(from);
    ssize_t n = recvfrom(fd, resp, sizeof(resp), 0, (struct sockaddr *)&from, &fromlen);
    if (n < 12) return fail("recvfrom");
    if (resp[0] != 0x12 || resp[1] != 0x34) return fail("txid mismatch");
    if ((resp[3] & 0x0F) != 0) return fail("rcode nonzero");
    unsigned ancount = ((unsigned)resp[6] << 8) | resp[7];
    if (ancount == 0) return fail("no answers");
    // scan for first A record (type 1, class 1, rdlen 4)
    int found = 0;
    unsigned char ip[4] = {0, 0, 0, 0};
    for (unsigned i = 0; i + 10 < (unsigned)n; i++) {
        if (resp[i] == 0x00 && resp[i+1] == 0x01 && resp[i+2] == 0x00 && resp[i+3] == 0x01
            && resp[i+8] == 0x00 && resp[i+9] == 0x04) {
            ip[0] = resp[i+10]; ip[1] = resp[i+11]; ip[2] = resp[i+12]; ip[3] = resp[i+13];
            found = 1;
            break;
        }
    }
    if (!found) return fail("no A record");
    printf("DNSTEST_PASS %u.%u.%u.%u\n", ip[0], ip[1], ip[2], ip[3]);
    close(fd);
    return 0;
}
