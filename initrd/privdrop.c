// privdrop: does a lower-privilege process actually get less privilege?
//
// The kernel stores every file's mode and owner and reports them in stat,
// but (before the access-control work) it never compared them against the
// caller. This program is the test for that comparison: it starts as root,
// drops to uid 1000, and asserts that the drop actually cost it something.
//
// Every fixture is created by the program itself, so the test does not depend
// on the image's contents or on what the filesystem's default modes happen to
// be. The mode is set with chmod after the create, because a create that
// ignores the requested mode is a separate defect.
//
// Output: one PRIVDROP_* line per assertion, then PRIVDROP_DONE. A failing
// assertion prints PRIVDROP_FAIL <what> and the program exits non-zero.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

// setgroups(2) lives outside POSIX, so glibc declares it only with _DEFAULT_SOURCE.
#include <grp.h>

static int failures = 0;

static void pass(const char *what) { printf("PRIVDROP_%s\n", what); }

static void fail(const char *what, const char *why) {
    printf("PRIVDROP_FAIL %s: %s\n", what, why);
    failures++;
}

/// Create a file with the given contents and mode, and return its path.
/// The mode is applied with chmod rather than at create time.
static const char *make_file(const char *path, const char *contents, mode_t mode) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) {
        printf("PRIVDROP_FAIL cannot create %s: %s\n", path, strerror(errno));
        failures++;
        return path;
    }
    ssize_t n = write(fd, contents, strlen(contents));
    (void)n;
    close(fd);
    if (chmod(path, mode) != 0) {
        printf("PRIVDROP_FAIL cannot chmod %s: %s\n", path, strerror(errno));
        failures++;
    }
    return path;
}

/// The setuid child: execve'd with the setuid bit set, so it should run as
/// uid 0 and be able to open a root-only file. Exits 0 on success, 1 if the
/// open fails (setuid did not take effect), 2 if the fixture is missing.
static int suid_child(void) {
    int fd = open("/tmp/privdrop_rootok", O_RDONLY);
    if (fd < 0) {
        return 2;
    }
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    // The setuid child entry point: execve'd from a setuid-root copy of this
    // binary. If setuid worked we are uid 0 and can open a root-only file.
    if (argc > 1 && strcmp(argv[1], "suidchild") == 0) {
        _exit(suid_child());
    }

    setvbuf(stdout, NULL, _IONBF, 0);

    // A root-only file, and one anybody may read.
    const char *secret = "/tmp/privdrop_secret";
    const char *public_ = "/tmp/privdrop_public";
    make_file(secret, "secret\n", 0600);
    make_file(public_, "public\n", 0644);

    // Two group-readable files: one owned by a group the caller belongs to
    // only through the supplementary list, one by a group it does not belong
    // to at all.
    const char *shared = "/tmp/privdrop_shared";
    const char *stranger = "/tmp/privdrop_stranger";
    make_file(shared, "shared\n", 0640);
    make_file(stranger, "stranger\n", 0640);
    if (chown(shared, 0, 1000) != 0) {
        fail("SETUP_CHOWN_SHARED", strerror(errno));
    }
    if (chown(stranger, 0, 3000) != 0) {
        fail("SETUP_CHOWN_STRANGER", strerror(errno));
    }

    // A tree the caller may not traverse: 0700 root-owned, with a
    // world-readable file underneath it.
    mkdir("/rootonly", 0755);
    mkdir("/rootonly/sub", 0755);
    make_file("/rootonly/sub/file", "hidden\n", 0644);
    if (chmod("/rootonly", 0700) != 0) {
        fail("SETUP_CHMOD_ROOTONLY", strerror(errno));
    }

    // Root-owned fixtures used by the mutation tests after the drop.
    mkdir("/rootonly/writetest", 0755);
    chmod("/rootonly/writetest", 0700);
    make_file("/rootonly/rootfile", "root\n", 0644);
    mkdir("/stickydir", 0755);
    chmod("/stickydir", 01777);
    open("/stickydir/theirs", O_WRONLY|O_CREAT|O_TRUNC, 0644);
    mkdir("/nostickydir", 0755);
    chmod("/nostickydir", 0777);
    open("/nostickydir/others", O_WRONLY|O_CREAT|O_TRUNC, 0644);
    mkdir("/shareddir", 0755);
    chmod("/shareddir", 0770);
    chown("/shareddir", 0, 1000);
    mkdir("/writabledir", 0755);
    chmod("/writabledir", 0777);
    // A directory that is searchable but not writable by the caller: create
    // here must fail on the parent's w bit, not on traversal.
    mkdir("/nowritedir", 0755);
    make_file("/writabledir/rootfile", "root\n", 0644);

    // Fixtures for the exec tests: a non-executable file, a root-only file
    // that a setuid-root binary should be able to open, and this binary made
    // setuid-root so execve'ing it raises the process to uid 0.
    make_file("/tmp/privdrop_noexec", "noexec\n", 0644);
    make_file("/tmp/privdrop_rootok", "root\n", 0600);
    // /privdrop is root-owned; make it setuid-root so an unprivileged execve
    // of it raises the new process to uid 0.
    chmod("/privdrop", 04755);

    // The supplementary list is set while still root, because setgroups is
    // root-only -- as it must be, or any process could join any group.
    gid_t supp[] = {1000};
    if (setgroups(1, supp) != 0) {
        fail("SETUP_SETGROUPS", strerror(errno));
    }

    // (1) As root, the root-only file is readable. Root bypasses the
    // read/write checks, so this must keep working after the checks land.
    int fd = open(secret, O_RDONLY);
    if (fd >= 0) {
        close(fd);
        pass("ROOT_OPEN_OK");
    } else {
        fail("ROOT_OPEN_OK", strerror(errno));
    }

    // Drop to the unprivileged user. The kernel's setuid sets uid, euid and
    // suid together, so this is a full drop, and the capabilities go with it.
    // The primary gid is 2000 deliberately: it matches none of the fixture
    // files, so only the supplementary list can put the caller in a file's
    // group.
    if (setgid(2000) != 0) {
        fail("SETGID", strerror(errno));
        return 1;
    }
    if (setuid(1000) != 0) {
        fail("SETUID", strerror(errno));
        return 1;
    }
    if (getuid() != 1000 || geteuid() != 1000 || getgid() != 2000) {
        fail("DROPPED", "the uid or gid did not change");
        return 1;
    }
    // getgroups must report the supplementary list, which is what the group
    // class consults.
    gid_t got[8];
    int ngroups = getgroups(0, NULL);
    if (ngroups != 1 || getgroups(1, got) != 1 || got[0] != 1000) {
        fail("GETGROUPS", "the supplementary list did not survive the drop");
    } else {
        pass("GETGROUPS_OK");
    }

    // The GPU/render device node is group 1000 (mode 0666), so a uid-1000
    // process in group 1000 can open it through the group class.
    errno = 0;
    fd = open("/dev/dri/renderD128", O_RDWR);
    if (fd >= 0) {
        close(fd);
        pass("USER_OPEN_RENDERNODE_OK");
    } else {
        fail("USER_OPEN_RENDERNODE_OK", strerror(errno));
    }
    struct stat st;
    if (stat("/dev/dri/renderD128", &st) == 0 && st.st_gid == 1000) {
        pass("USER_RENDERNODE_GID_1000");
    } else {
        fail("USER_RENDERNODE_GID_1000", "device node is not group 1000");
    }

    // ── exec checks ────────────────────────────────────────────────────────
    // (14) execve of a non-executable file is refused with EACCES. Fork so the
    // parent can observe the child's exit code without being replaced.
    {
        pid_t pid = fork();
        if (pid == 0) {
            execve("/tmp/privdrop_noexec", (char *const[]){"/tmp/privdrop_noexec", NULL},
                   (char *const[]){NULL});
            _exit(77); // execve itself returned an error
        }
        int status = 0;
        if (waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 77) {
            pass("USER_EXEC_NOEXEC_EACCES");
        } else {
            fail("USER_EXEC_NOEXEC_EACCES", "execve of a non-executable file succeeded");
        }
    }

    // (15) A setuid-root executable makes the new process uid 0, so it can
    // open a root-only file. /tmp/privdrop_suid is a setuid-root copy of this
    // binary; run with "suidchild" it opens the root-only fixture and reports
    // success via its exit code.
    {
        pid_t pid = fork();
        if (pid == 0) {
            execve("/privdrop",
                   (char *const[]){"/privdrop", "suidchild", NULL},
                   (char *const[]){NULL});
            _exit(77);
        }
        int status = 0;
        if (waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0) {
            pass("USER_SETUID_WORKS");
        } else {
            printf("DEBUG setuid: waitpid status=%d exited=%d code=%d\n", status, WIFEXITED(status), WIFEXITED(status) ? WEXITSTATUS(status) : -1);
            fail("USER_SETUID_WORKS", "setuid bit did not take effect on exec");
        }
    }

    // (2) The root-only file is not readable any more.
    errno = 0;
    fd = open(secret, O_RDONLY);
    if (fd < 0 && errno == EACCES) {
        pass("USER_OPEN_EACCES");
    } else {
        if (fd >= 0) close(fd);
        fail("USER_OPEN_EACCES", fd >= 0 ? "the open succeeded" : strerror(errno));
    }

    // (3) A file anybody may read still is.
    fd = open(public_, O_RDONLY);
    if (fd >= 0) {
        close(fd);
        pass("USER_OPEN_OTHER_OK");
    } else {
        fail("USER_OPEN_OTHER_OK", strerror(errno));
    }

    // (4) The group class is matched through the *supplementary* list, not
    // just the primary gid. The caller's primary gid is 2000 throughout; the
    // shared file is group 1000 and the stranger's is group 3000.
    errno = 0;
    fd = open(shared, O_RDONLY);
    if (fd >= 0) {
        close(fd);
        pass("USER_GROUP_VIA_SUPPLEMENTARY_OK");
    } else {
        fail("USER_GROUP_VIA_SUPPLEMENTARY_OK", strerror(errno));
    }
    errno = 0;
    fd = open(stranger, O_RDONLY);
    if (fd < 0 && errno == EACCES) {
        pass("USER_GROUP_NOT_MEMBER_EACCES");
    } else {
        if (fd >= 0) close(fd);
        fail("USER_GROUP_NOT_MEMBER_EACCES", fd >= 0 ? "the open succeeded" : strerror(errno));
    }

    // (5) The check uses the *requested* access, not the file's own mode: the
    // file is readable, so a read succeeds, but it is not writable.
    errno = 0;
    fd = open(public_, O_WRONLY);
    if (fd < 0 && errno == EACCES) {
        pass("USER_OPEN_WRONLY_EACCES");
    } else {
        if (fd >= 0) close(fd);
        fail("USER_OPEN_WRONLY_EACCES", fd >= 0 ? "the open succeeded" : strerror(errno));
    }

    // (6) Traversal: the file itself is world-readable, but it sits under a
    // directory this caller may not search. That is EACCES -- "you may not
    // look here" -- not ENOENT, "there is nothing here".
    errno = 0;
    fd = open("/rootonly/sub/file", O_RDONLY);
    if (fd < 0 && errno == EACCES) {
        pass("USER_TRAVERSE_EACCES");
    } else {
        if (fd >= 0) close(fd);
        fail("USER_TRAVERSE_EACCES", fd >= 0 ? "the open succeeded" : strerror(errno));
    }

    // ── Mutation checks (the rest of the access-control story) ─────────────
    // (7) Creating an entry needs w+x on the *parent*: a 0700 root-owned
    // directory refuses the caller (the directory was made root-owned above).
    errno = 0;
    if (open("/rootonly/writetest/afile", O_WRONLY|O_CREAT|O_TRUNC, 0644) < 0 && errno == EACCES) {
        pass("USER_CREATE_IN_0700_DIR_EACCES");
    } else {
        fail("USER_CREATE_IN_0700_DIR_EACCES", errno == 0 ? "the create succeeded" : strerror(errno));
    }

    // (7b) A directory that is searchable but not writable by the caller:
    // create is refused on the parent's w bit, not on traversal.
    errno = 0;
    if (open("/nowritedir/afile", O_WRONLY|O_CREAT|O_TRUNC, 0644) < 0 && errno == EACCES) {
        pass("USER_CREATE_IN_NOWRITEDIR_EACCES");
    } else {
        fail("USER_CREATE_IN_NOWRITEDIR_EACCES", errno == 0 ? "the create succeeded" : strerror(errno));
    }

    // (8) A world-writable directory lets the caller create, and the new file
    // is owned by the caller with the caller's egid (the caller is not in the
    // root-owned directory's group).
    errno = 0;
    fd = open("/writabledir/afile", O_WRONLY|O_CREAT|O_TRUNC, 0644);
    if (fd < 0) {
        fail("USER_CREATE_IN_0777_DIR", strerror(errno));
    } else {
        close(fd);
        pass("USER_CREATE_IN_0777_DIR");
        struct stat st;
        if (stat("/writabledir/afile", &st) == 0 && st.st_uid == 1000 && st.st_gid == 2000) {
            pass("USER_CREATE_OWNER_1000_2000");
        } else {
            fail("USER_CREATE_OWNER_1000_2000", "unexpected owner/group");
        }
    }

    // (9) A directory whose group the caller is in (via the supplementary
    // list) gives new files that group, not the caller's egid.
    errno = 0;
    fd = open("/shareddir/afile", O_WRONLY|O_CREAT|O_TRUNC, 0644);
    if (fd < 0) {
        fail("USER_CREATE_SETGID_DIR", strerror(errno));
    } else {
        close(fd);
        pass("USER_CREATE_SETGID_DIR");
        struct stat st;
        if (stat("/shareddir/afile", &st) == 0 && st.st_gid == 1000) {
            pass("USER_CREATE_SETGID_DIR_GID_1000");
        } else {
            fail("USER_CREATE_SETGID_DIR_GID_1000", "group not inherited");
        }
    }

    // Root-owned files for the unlink/chmod/chown tests: /rootonly/rootfile
    // lives in an unsearchable directory, /writabledir/rootfile in a
    // searchable one (so the ownership check, not traversal, is what fails).

    // (10) Unlink needs w on the parent; /rootonly is 0700 root-owned so the
    // caller cannot reach it → EACCES.
    errno = 0;
    if (unlink("/rootonly/rootfile") != 0 && errno == EACCES) {
        pass("USER_UNLINK_ROOTFILE_EACCES");
    } else {
        fail("USER_UNLINK_ROOTFILE_EACCES", errno == 0 ? "the unlink succeeded" : strerror(errno));
    }

    // (11) A sticky (1777) directory: the caller may not remove another's
    // file → EPERM (/stickydir/theirs is root-owned, created above).
    errno = 0;
    if (unlink("/stickydir/theirs") != 0 && errno == EPERM) {
        pass("USER_UNLINK_STICKY_EPERM");
    } else {
        fail("USER_UNLINK_STICKY_EPERM", errno == 0 ? "the unlink succeeded" : strerror(errno));
    }

    // (12) A non-sticky (0777) directory: the caller may remove another's
    // file → success (that is precisely what /tmp allows).
    errno = 0;
    if (unlink("/nostickydir/others") == 0) {
        pass("USER_UNLINK_NONSTICKY_OK");
    } else {
        fail("USER_UNLINK_NONSTICKY_OK", strerror(errno));
    }

    // (13) chmod/chown on a root-owned file are refused → EPERM. The file is
    // in a world-writable directory so traversal is fine; the denial is the
    // ownership rule, not traversal.
    errno = 0;
    if (chmod("/writabledir/rootfile", 0777) != 0 && errno == EPERM) {
        pass("USER_CHMOD_ROOTFILE_EPERM");
    } else {
        fail("USER_CHMOD_ROOTFILE_EPERM", errno == 0 ? "the chmod succeeded" : strerror(errno));
    }
    errno = 0;
    if (chown("/writabledir/rootfile", 1000, 1000) != 0 && errno == EPERM) {
        pass("USER_CHOWN_ROOTFILE_EPERM");
    } else {
        fail("USER_CHOWN_ROOTFILE_EPERM", errno == 0 ? "the chown succeeded" : strerror(errno));
    }

    if (failures) {
        printf("PRIVDROP_FAILURES %d\n", failures);
        return 1;
    }
    printf("PRIVDROP_DONE\n");
    return 0;
}
