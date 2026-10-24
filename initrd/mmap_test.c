#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include <string.h>

int main(void) {
    const char *test_path = "/mmap_test.txt";
    int fd_create = open(test_path, O_RDWR | O_CREAT | O_TRUNC, 0666);
    if (fd_create < 0) {
        printf("mmap_test: create failed\n");
        return 1;
    }
    const char *initial = "Hello, DragOS mmap world!\n";
    if (write(fd_create, initial, strlen(initial)) != (ssize_t)strlen(initial)) {
        printf("mmap_test: write failed\n");
        close(fd_create);
        return 1;
    }
    close(fd_create);

    int fd = open(test_path, O_RDONLY);
    if (fd < 0) { printf("mmap_test: open failed\n"); return 1; }
    off_t sz = lseek(fd, 0, SEEK_END);
    lseek(fd, 0, SEEK_SET);
    char *p = mmap(0, sz, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    if (p == MAP_FAILED) { printf("mmap_test: mmap failed\n"); return 1; }
    printf("mmap_test: first bytes: %.20s\n", p);  /* demand-paged read */

    /* MAP_PRIVATE COW write must not touch the file on disk. */
    char save = p[0];
    p[0] = 'Z';
    if (p[0] != 'Z') { printf("mmap_test: COW write failed\n"); return 1; }
    int fd2 = open(test_path, O_RDONLY);
    char orig;
    if (fd2 < 0 || read(fd2, &orig, 1) != 1 || orig != save) {
        printf("mmap_test: file mutated!\n"); return 1;
    }
    close(fd);
    close(fd2);
    munmap(p, sz);
    unlink(test_path);
    printf("mmap_test: COW ok\n");
    return 0;
}