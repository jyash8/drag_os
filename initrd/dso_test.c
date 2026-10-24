#include <stdio.h>
#include <errno.h>
#include <unistd.h>
#include <sys/wait.h>

int main(int argc, char **argv) {
    printf("dso_test pid=%d argc=%d libc_data=%p\n", getpid(), argc, (void *)&stdout);
    if (argc > 1) {
        _exit(0); /* exec'd copy: libc.so freshly mapped */
    }
    for (int i = 0; i < 2; i++) {
        printf("parent fork child %d\n", i + 1);
        pid_t p = fork();
        if (p == 0) {
            printf("child %d: pre-exec getpid=%d libc_data=%p\n",
                   i + 1, getpid(), (void *)&stdout);
            char *args[] = {"dso_test", "child", 0};
            int r = execv("/dso_test", args);
            printf("child %d: execv FAILED r=%d errno=%d\n", i + 1, r, errno);
            _exit(1);
        }
        printf("parent waitpid %d\n", i + 1);
        waitpid(p, 0, 0);
    }
    printf("parent done\n");
    return 0;
}
