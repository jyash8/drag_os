/* io_uring_test.c — minimal io_uring exercise for DragOS.
 *
 * Creates a ring, submits a NOP, a WRITE to a temp file and a READ back,
 * and prints the completions. Exercises io_uring_setup / io_uring_enter.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <signal.h>
#include <linux/io_uring.h>

static int sfd = -1;
static void say(const char *s) {
    if (sfd < 0) sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) write(sfd, s, strlen(s));
}

#define IOURING_ENTER_GETEVENTS 1U

/* Direct syscall wrappers: the guest may not have liburing. */
static int io_uring_setup(unsigned entries, struct io_uring_params *p) {
    return (int)syscall(425, entries, p);
}
static int io_uring_enter(int fd, unsigned to_submit, unsigned min_complete,
                          unsigned flags, sigset_t *sig) {
    return (int)syscall(426, fd, to_submit, min_complete, flags, sig);
}
static int io_uring_register(int fd, unsigned opcode, void *arg, unsigned nr_args) {
    return (int)syscall(427, fd, opcode, arg, nr_args);
}

int main(void) {
    char b[256];
    sfd = open("/dev/ttyS0", O_WRONLY);
    if (sfd >= 0) { dup2(sfd, 1); dup2(sfd, 2); close(sfd); sfd = 1; }

    say("[iouring] starting\n");

    struct io_uring_params p;
    memset(&p, 0, sizeof(p));

    int fd = io_uring_setup(8, &p);
    snprintf(b, sizeof b, "[iouring] io_uring_setup -> %d\n", fd);
    say(b);
    if (fd < 0) {
        say("[iouring] FAIL: setup\n");
        return 1;
    }

    snprintf(b, sizeof b,
             "[iouring] sq_entries=%u cq_entries=%u features=%#x\n",
             p.sq_entries, p.cq_entries, p.features);
    say(b);
    snprintf(b, sizeof b,
             "[iouring] sq_off: head=%u tail=%u mask=%u entries=%u array=%u\n",
             p.sq_off.head, p.sq_off.tail, p.sq_off.ring_mask,
             p.sq_off.ring_entries, p.sq_off.array);
    say(b);
    snprintf(b, sizeof b,
             "[iouring] cq_off: head=%u tail=%u mask=%u entries=%u cqes=%u\n",
             p.cq_off.head, p.cq_off.tail, p.cq_off.ring_mask,
             p.cq_off.ring_entries, p.cq_off.cqes);
    say(b);

    /* Map the rings the way liburing does: three mmaps on the ring fd at
     * distinct file offsets. */
    unsigned char *sq_ring = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                  MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_SQ_RING);
    unsigned char *cq_ring = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                                  MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_CQ_RING);
    snprintf(b, sizeof b, "[iouring] mmap sq=%p cq=%p\n", sq_ring, cq_ring);
    say(b);
    if (sq_ring == MAP_FAILED || cq_ring == MAP_FAILED) {
        say("[iouring] FAIL: mmap rings\n");
        return 1;
    }

    /* The SQE array is a third mmap at IORING_OFF_SQES. */
    size_t sqes_sz = (size_t)p.sq_entries * sizeof(struct io_uring_sqe);
    struct io_uring_sqe *sqes = mmap(NULL, sqes_sz, PROT_READ | PROT_WRITE,
                                     MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_SQES);
    snprintf(b, sizeof b, "[iouring] mmap sqes=%p\n", (void *)sqes);
    say(b);
    if (sqes == MAP_FAILED) {
        say("[iouring] FAIL: mmap sqes\n");
        return 1;
    }

    /* Ring offsets are byte offsets within the mapped regions (Linux ABI). */
    unsigned *sring_sq_head = (unsigned *)(sq_ring + p.sq_off.head);
    unsigned *sring_sq_tail = (unsigned *)(sq_ring + p.sq_off.tail);
    unsigned *sring_sq_array = (unsigned *)(sq_ring + p.sq_off.array);
    unsigned *sring_cq_head = (unsigned *)(cq_ring + p.cq_off.head);
    unsigned *sring_cq_tail = (unsigned *)(cq_ring + p.cq_off.tail);
    struct io_uring_cqe *cqes = (struct io_uring_cqe *)(cq_ring + p.cq_off.cqes);
    snprintf(b, sizeof b, "[iouring] sq_head=%p sq_tail=%p sq_array=%p cq_head=%p cq_tail=%p cqes=%p\n",
             (void *)sring_sq_head, (void *)sring_sq_tail, (void *)sring_sq_array,
             (void *)sring_cq_head, (void *)sring_cq_tail, (void *)cqes);
    say(b);

    /* Prepare a file for read/write. */
    int filefd = open("/tmp/iouring_test.txt", O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (filefd < 0) {
        snprintf(b, sizeof b, "[iouring] open failed: %d\n", errno);
        say(b);
        filefd = open("/etc/termcap", O_RDONLY);
        if (filefd < 0) { say("[iouring] FAIL: no file\n"); return 1; }
    }

    char wrbuf[64];
    memset(wrbuf, 0, sizeof(wrbuf));
    strcpy(wrbuf, "io_uring dragos payload\n");

    char rdbuf[64];
    memset(rdbuf, 0, sizeof(rdbuf));

    /* Build three SQEs: NOP, WRITE, READ. */
    unsigned tail = *sring_sq_tail;
    unsigned idx = tail & (p.sq_entries - 1);

    struct io_uring_sqe *sqe = &sqes[idx];
    memset(sqe, 0, sizeof(*sqe));
    sqe->opcode = 0; /* IORING_OP_NOP */
    sqe->user_data = 0x1111;
    sring_sq_array[idx] = idx;
    tail++;

    idx = tail & (p.sq_entries - 1);
    sqe = &sqes[idx];
    memset(sqe, 0, sizeof(*sqe));
    sqe->opcode = 2; /* IORING_OP_WRITEV */
    sqe->fd = filefd;
    sqe->addr = (unsigned long)wrbuf;
    sqe->len = (unsigned)strlen(wrbuf);
    sqe->off = 0;
    sqe->user_data = 0x2222;
    sring_sq_array[idx] = idx;
    tail++;

    idx = tail & (p.sq_entries - 1);
    sqe = &sqes[idx];
    memset(sqe, 0, sizeof(*sqe));
    sqe->opcode = 1; /* IORING_OP_READV */
    sqe->fd = filefd;
    sqe->addr = (unsigned long)rdbuf;
    sqe->len = 32;
    sqe->off = 0;
    sqe->user_data = 0x3333;
    sring_sq_array[idx] = idx;
    tail++;

    *sring_sq_tail = tail;
    snprintf(b, sizeof b, "[iouring] submitted 3 SQEs (tail=%u)\n", tail);
    say(b);

    int r = io_uring_enter(fd, 3, 3, IOURING_ENTER_GETEVENTS, NULL);
    snprintf(b, sizeof b, "[iouring] io_uring_enter -> %d\n", r);
    say(b);

    /* Dump the CQ. */
    unsigned head = *sring_cq_head;
    unsigned ctail = *sring_cq_tail;
    snprintf(b, sizeof b, "[iouring] cq head=%u tail=%u\n", head, ctail);
    say(b);

    for (unsigned i = head; i != ctail; i++) {
        struct io_uring_cqe *cqe = &cqes[i & (p.cq_entries - 1)];
        snprintf(b, sizeof b, "[iouring]   cqe user_data=%#lx res=%d flags=%u\n",
                 (unsigned long)cqe->user_data, cqe->res, cqe->flags);
        say(b);
        if (cqe->user_data == 0x2222 && cqe->res > 0) {
            snprintf(b, sizeof b, "[iouring]   wrote %d bytes\n", cqe->res);
            say(b);
        }
        if (cqe->user_data == 0x3333 && cqe->res > 0) {
            char tmp[128];
            int n = cqe->res < 120 ? cqe->res : 120;
            memcpy(tmp, rdbuf, n);
            tmp[n] = 0;
            snprintf(b, sizeof b, "[iouring]   read back: %s\n", tmp);
            say(b);
        }
    }

    *sring_cq_head = ctail;

    /* io_uring_register: register a fixed file. */
    int regfd = open("/etc/termcap", O_RDONLY);
    if (regfd >= 0) {
        r = io_uring_register(fd, 1 /* IORING_REGISTER_FILES */, &regfd, 1);
        snprintf(b, sizeof b, "[iouring] io_uring_register(FILES) -> %d\n", r);
        say(b);
    }

    close(filefd);
    say("[iouring] done\n");
    return 0;
}
