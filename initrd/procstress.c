// Process-scale stress test.
//
// Ramps the number of *concurrently live* processes and checks that the kernel
// keeps accounting for all of them. Each round forks N children that do real
// work (allocate, write, read back, exit with a status encoding their index),
// then the parent reaps them all and verifies every status.
//
// Design notes, because they matter for what the numbers mean:
//
//   * Each child exits with code `base + i`, so the parent can prove it reaped
//     the *right* child rather than merely "some child". A scheduler that
//     mixed up waitpid results would still exit 0 here if we only counted.
//   * Children are forked and reaped in waves, so the peak live-process count
//     is what grows -- that is the number that stresses the process table, the
//     scheduler queue, and pid allocation rather than just wall time.
//   * Results print as ONE summary line at the end. The harness's serial reader
//     is flaky on multi-line output (see docs/deferred-work.md §6), so a test
//     that streams per-round lines is measuring the harness as much as the
//     kernel. A single terminal line is the least flake-exposed oracle.
//   * Everything is on stdout via the shell's serial redirect, matching the
//     other probes.
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

// Exit codes must stay in 1..125 to be distinguishable from shell noise.
#define EXIT_BASE 10
#define MAX_INDEX 100

static int failures;

// Child body: touch memory so the fork is not optimised into a no-op, then
// exit with a status that identifies us.
static void child_body(int idx) {
    // A modest allocation exercises the slab/COW paths under concurrency.
    // 64 KiB lands in a size class rather than the large-allocation fallback.
    size_t n = 64 * 1024;
    char *buf = malloc(n);
    if (buf) {
        memset(buf, idx & 0xff, n);
        // Read it back so the pages are genuinely faulted in.
        volatile unsigned char sink = 0;
        for (size_t i = 0; i < n; i += 4096) sink += buf[i];
        (void)sink;
        free(buf);
    }
    _exit(EXIT_BASE + idx);
}

// Fork `count` children, reap them all, verify each exited with the expected
// code. Returns the number of mismatches.
static int run_wave(int count) {
    pid_t *pids = calloc((size_t)count, sizeof(pid_t));
    if (!pids) {
        fprintf(stderr, "calloc failed\n");
        return count;
    }

    int forked = 0;
    for (int i = 0; i < count; i++) {
        pid_t p = fork();
        if (p == 0) {
            child_body(i);
            // unreachable
        }
        if (p < 0) {
            // Fork failed at scale. That is itself a finding: record how far
            // we got rather than silently shrinking the test.
            break;
        }
        pids[forked++] = p;
    }

    int bad = count - forked; // un-forked children count as failures
    for (int i = 0; i < forked; i++) {
        int st = 0;
        pid_t r;
        do {
            r = waitpid(pids[i], &st, 0);
        } while (r < 0 && errno == EINTR);

        if (r != pids[i]) {
            bad++;
            continue;
        }
        // WIFEXITED + exact code proves the right child was reaped.
        if (!WIFEXITED(st) || WEXITSTATUS(st) != EXIT_BASE + i) {
            bad++;
        }
    }
    free(pids);
    return bad;
}

int main(void) {
    // Wave sizes chosen to straddle the interesting transitions: a handful of
    // processes, then enough to make the scheduler queue and pid space work,
    // then a wave wide enough to expose per-process bookkeeping limits.
    const int waves[] = {4, 16, 32, 64};
    const int nwaves = (int)(sizeof(waves) / sizeof(waves[0]));

    failures = 0;
    for (int w = 0; w < nwaves; w++) {
        failures += run_wave(waves[w]);
    }

    // Terminal summary: single line, easy for the harness to match, and it
    // carries the real quantities rather than just a verdict.
    printf("STRESS_DONE waves=%d failures=%d\n", nwaves, failures);
    fflush(stdout);
    return failures ? 1 : 0;
}