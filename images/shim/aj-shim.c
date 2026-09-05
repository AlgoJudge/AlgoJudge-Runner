/* aj-shim -- runs one submission and measures that submission alone.
 *
 * **What it is for.** A time limit is processor time, and the Runner reads it
 * from the cgroup of the container the submission ran in. That cgroup also
 * holds everything the container spent getting there: measured 33 to 74 ms
 * across the four language images, against limits of 100 to 600 ms. So a
 * participant is charged for the container's own start. This becomes PID 1
 * instead, forks the submission, and reports `wait4`'s accounting for that one
 * child -- which is the program and nothing else, at microsecond resolution.
 *
 * **It is the precise instrument, and it is what a participant is charged.**
 * The report travels out on stderr, through a container the submission also
 * lives in, so three things are what make a forged one hard: the line carries a
 * nonce the submission cannot read, this process kills every other process in
 * the namespace before writing, and it writes last. The Runner's cgroup reading
 * is kept beside it to *disbelieve* a report the reading cannot account for --
 * a container costs a program nothing it can be charged for, and measurement
 * showed that charging the larger of the two failed correct submissions.
 *
 *   aj-shim <input-file> <output-file> <program> [args...]
 *
 * The input file becomes the child's stdin. `sh -c "exec prog < input"` did
 * that before and is what this replaces; a shell would otherwise be a second
 * process in the accounting, and its own start is part of what is being removed.
 *
 * **The output file becomes the child's stdout, and that is a disk measurement
 * rather than a taste.** Left on the container's own stdout, everything a
 * submission prints is written by the daemon to its `*-json.log` -- JSON-escaped
 * and stamped per line -- and read back through a socket. Measured 2026-09-05 on
 * a twelve-Runner burst: one flooding submission produced a **76 MB** log for a
 * 64 MiB cap, and the daemon wrote at 72 MB/s throughout. Here the bytes go
 * straight to the file the Runner already has open a directory to.
 *
 * **The submission never gets a writable path, only the descriptor.** This runs
 * as root and opens the file before forking; the directory it sits in is root's,
 * so the submission -- which is uid 65534 by the time it runs -- cannot create,
 * rename or read anything there. It is the same trick already used for the
 * input, in the other direction.
 *
 * `AJ_SHIM_MAX_OUTPUT` is the cap, in bytes, applied as `RLIMIT_FSIZE` before
 * privileges are dropped. **The kernel stops the program at the byte**, where
 * the Runner used to count a stream and notice afterwards; a submission over it
 * dies of `SIGXFSZ`, which the report carries out like any other signal.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

/* Who the submission runs as. The same `nobody` the images declare, so nothing
 * about the submission's own privileges changes -- what changes is that this
 * process keeps root long enough to be unreachable from it. */
#define RUN_AS_UID 65534
#define RUN_AS_GID 65534

/* Ours, and outside anything a submission produces by exiting: 125 is the
 * highest code `env` and friends reserve, and a program returning it is
 * indistinguishable from us failing -- which is why the report line carries the
 * reason as well, and the Runner reads that rather than guessing from a code. */
#define SHIM_FAILED 125
#define EXEC_FAILED 126
#define NOT_FOUND 127

extern char **environ;

static char nonce[128];

/* One `write`, because two could interleave with whatever else holds this fd. */
static void say(const char *text) {
    size_t left = strlen(text);
    const char *at = text;
    while (left > 0) {
        ssize_t wrote = write(STDERR_FILENO, at, left);
        if (wrote <= 0) {
            if (errno == EINTR) continue;
            return;
        }
        at += wrote;
        left -= (size_t)wrote;
    }
}

static void fatal(const char *what) {
    char line[512];
    snprintf(line, sizeof line, "%s aj-shim1 failed %s: %s\n",
             nonce, what, strerror(errno));
    say(line);
    _exit(SHIM_FAILED);
}

/* **Overwriting the bytes, not just calling `unsetenv`.** `/proc/<pid>/environ`
 * reads the block the kernel recorded when the process was executed, so an
 * unset leaves the value there for anyone allowed to read it. Scrubbing in
 * place is what actually removes it, and it is what makes the nonce safe even
 * where this runs as the same user as the submission. */
static void take_nonce(void) {
    static const char key[] = "AJ_SHIM_NONCE=";
    for (char **entry = environ; *entry != NULL; entry++) {
        if (strncmp(*entry, key, sizeof key - 1) != 0) continue;

        char *value = *entry + sizeof key - 1;
        snprintf(nonce, sizeof nonce, "%s", value);
        memset(value, 'x', strlen(value));
        unsetenv("AJ_SHIM_NONCE");
        return;
    }
    errno = ENOENT;
    fatal("no AJ_SHIM_NONCE in the environment");
}

/* The output cap, taken from the environment and then removed from it.
 *
 * **Scrubbed like the nonce, and for a weaker reason.** Knowing the cap tells a
 * submission nothing it could not learn by writing until it dies, but the
 * environment it is handed should carry only what it needs, and leaving one of
 * ours in there invites the next one to be left too.
 *
 * Absent means no cap, which is what every unmeasured step wants. */
static rlim_t take_output_cap(void) {
    static const char key[] = "AJ_SHIM_MAX_OUTPUT=";
    for (char **entry = environ; *entry != NULL; entry++) {
        if (strncmp(*entry, key, sizeof key - 1) != 0) continue;

        char *value = *entry + sizeof key - 1;
        errno = 0;
        char *end = NULL;
        unsigned long long bytes = strtoull(value, &end, 10);
        if (errno != 0 || end == value || *end != '\0') {
            errno = EINVAL;
            fatal("AJ_SHIM_MAX_OUTPUT is not a number of bytes");
        }
        unsetenv("AJ_SHIM_MAX_OUTPUT");
        return (rlim_t)bytes;
    }
    return 0;
}

/* **Verified, and fatal if it did not take.** A drop that silently fails would
 * run a submission as root, so every failure here has to stop the run rather
 * than continue into `execve`. Where this already runs unprivileged the calls
 * are no-ops that succeed, which is correct: the submission ends up as the same
 * user either way. */
static void become_the_submission(void) {
    if (getuid() == 0) {
        if (setgroups(0, NULL) != 0) fatal("setgroups");
        if (setgid(RUN_AS_GID) != 0) fatal("setgid");
        if (setuid(RUN_AS_UID) != 0) fatal("setuid");
    }

    if (getuid() != RUN_AS_UID || geteuid() != RUN_AS_UID) {
        errno = EPERM;
        fatal("the drop did not take");
    }
    if (setuid(0) == 0) {
        errno = EPERM;
        fatal("root is still reachable after the drop");
    }
}

/* Everything this run started, before anything is reported.
 *
 * **`kill(-1)` rather than the process group, and only as PID 1.** A child that
 * called `setsid` has left both the group and the session, so only `kill(-1)`
 * still reaches it -- and PID 1 of a namespace is exempt from its own, so it
 * cannot reach us. Which is the whole of the reason this is guarded: run
 * anywhere but as init, `kill(-1)` means *every process this user owns*, and
 * that is the machine rather than the run. The guard is what makes this
 * testable outside a container at all.
 *
 * Outside one, the child's own process group is the most that can be killed
 * safely. It misses a `setsid` escapee, which is why that case is proved in the
 * containerised suite and not here. */
static void kill_the_rest(pid_t group) {
    if (getpid() == 1) {
        kill(-1, SIGKILL);
    } else if (group > 0) {
        kill(-group, SIGKILL);
    }
    for (int i = 0; i < 1024; i++) {
        int status;
        pid_t gone = waitpid(-1, &status, WNOHANG);
        if (gone <= 0) break;
    }
}

int main(int argc, char **argv) {
    if (argc < 4) {
        errno = EINVAL;
        fatal("usage: aj-shim <input-file> <output-file> <program> [args...]");
    }

    take_nonce();
    rlim_t cap = take_output_cap();

    int input = open(argv[1], O_RDONLY | O_CLOEXEC);
    if (input < 0) fatal("cannot open the input file");

    /* **Opened here, as root, and never named to the submission.** 0600 so that
     * nothing else in the container can read back what was written -- a checker
     * runs in a container of its own and gets the file from the Runner. */
    int output = open(argv[2], O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC, 0600);
    if (output < 0) fatal("cannot open the output file");

    struct timeval began;
    gettimeofday(&began, NULL);

    pid_t child = fork();
    if (child < 0) fatal("fork");

    if (child == 0) {
        /* A group of its own, so a shim that is not PID 1 has something it can
         * kill without reaching past this run. */
        setpgid(0, 0);
        if (dup2(input, STDIN_FILENO) < 0) fatal("dup2 the input onto stdin");
        if (dup2(output, STDOUT_FILENO) < 0) fatal("dup2 the output onto stdout");

        /* **Before the drop, so it cannot be raised again.** A limit set as root
         * and then inherited across `setuid` is a limit the submission has no
         * way back over: `RLIMIT_FSIZE` may only be raised towards the hard
         * limit, and this sets both. `SIGXFSZ` then arrives on the write that
         * would have crossed it. */
        if (cap > 0) {
            struct rlimit fsize = {.rlim_cur = cap, .rlim_max = cap};
            if (setrlimit(RLIMIT_FSIZE, &fsize) != 0) fatal("setrlimit RLIMIT_FSIZE");
        }

        become_the_submission();
        /* **`execvp`, because the catalogue names `python3` and not a path.**
         * The shell this replaces searched `PATH`; `execve` does not, and every
         * interpreted language in the catalogue would come back as a program
         * that is not there. The environment is the one already scrubbed of the
         * nonce, which `execvp` passes on as it stands. */
        execvp(argv[3], &argv[3]);
        /* Only reachable when `execve` did not happen. */
        char line[512];
        snprintf(line, sizeof line, "%s aj-shim1 failed exec %s: %s\n",
                 nonce, argv[3], strerror(errno));
        say(line);
        /* 127 and 126 are the shell's own split -- not found, against found and
         * not runnable -- so a submission naming a program that is not there
         * comes back as the code it always did. */
        _exit(errno == ENOENT ? NOT_FOUND : EXEC_FAILED);
    }

    int status = 0;
    struct rusage used;
    while (wait4(child, &status, 0, &used) < 0) {
        if (errno != EINTR) fatal("wait4");
    }

    kill_the_rest(child);

    struct timeval ended;
    gettimeofday(&ended, NULL);
    long long wall_us = (long long)(ended.tv_sec - began.tv_sec) * 1000000
                      + (ended.tv_usec - began.tv_usec);

    long long user_us = (long long)used.ru_utime.tv_sec * 1000000 + used.ru_utime.tv_usec;
    long long system_us = (long long)used.ru_stime.tv_sec * 1000000 + used.ru_stime.tv_usec;
    long long cpu_us = user_us + system_us;

    /* `ru_maxrss` is the child's own here, and only because this process is
     * small: the mark survives `fork` and `exec`, so a child forked from a
     * large parent reports the parent's. That is the defect this replaces --
     * measuring from a PyPy driver returned its 64 MiB for every program. */
    char line[512];
    /* **User and system apart, as well as together.** `wait4` has both and
     * this summed them away. They answer a question the total cannot: work a
     * program did, against work the kernel did on its behalf -- faulting its
     * pages in, reading its input. Under load the second can be the larger,
     * and a participant is charged for both. **The line's shape is fixed and
     * `aj-shim1` names it**: a reader finding fewer fields than that version
     * promises has been handed a report this Runner did not write. */
    snprintf(line, sizeof line, "%s aj-shim1 ok %d %d %lld %lld %lld %lld %lld\n",
             nonce,
             WIFEXITED(status) ? WEXITSTATUS(status) : 0,
             WIFSIGNALED(status) ? WTERMSIG(status) : 0,
             cpu_us, (long long)used.ru_maxrss * 1024, wall_us,
             user_us, system_us);
    say(line);

    /* **The child's fate, worn as our own.** The runtime reports PID 1's exit
     * code, and until now PID 1 *was* the submission -- so a segmentation fault
     * arrived as 139 and the pipeline read the signal out of it. Returning our
     * own success instead would turn every killed program into a clean exit. */
    if (WIFSIGNALED(status)) _exit(128 + WTERMSIG(status));
    _exit(WIFEXITED(status) ? WEXITSTATUS(status) : SHIM_FAILED);
}
