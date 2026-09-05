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
 * The report goes out on a channel of its own that nothing else in the
 * container can name, and it still carries the nonce, this process still kills
 * every other process in the namespace before writing, and it still writes
 * last. The Runner's cgroup reading is kept beside it to *disbelieve* a report
 * the reading cannot account for -- a container costs a program nothing it can
 * be charged for, and measurement showed that charging the larger of the two
 * failed correct submissions.
 *
 *   aj-shim <input-file> <output-file> <program> [args...]
 *
 * The input file becomes the child's stdin. `sh -c "exec prog < input"` did
 * that before and is what this replaces; a shell would otherwise be a second
 * process in the accounting, and its own start is part of what is being removed.
 *
 * **The second argument becomes the child's stdout, and it is normally a pipe.**
 * Left on the container's own stdout, everything a submission prints is written
 * by the daemon to its `*-json.log` -- JSON-escaped and stamped per line -- and
 * read back through a socket. Measured 2026-09-05 on a twelve-Runner burst: one
 * flooding submission produced a **76 MB** log for a 64 MiB cap, and the daemon
 * wrote at 72 MB/s throughout. Here the bytes go to whoever is reading, while
 * the program is still running, and are never stored by anybody.
 *
 * **The submission never gets a writable path, only the descriptor.** This runs
 * as root and opens both ends before forking; what they sit in is root's, so
 * the submission -- which is uid 65534 by the time it runs -- cannot open, name
 * or create anything there. It is the same trick already used for the input,
 * in the other direction.
 *
 * **The report has a channel of its own**, named by `AJ_SHIM_REPORT`. It used
 * to travel on the container's stderr, mixed in with whatever the submission
 * printed there and picked back out by its nonce. Now the submission's stderr
 * is `/dev/null` and the report has a pipe nobody else can write to, so the
 * three things that made a forged report hard are down to one that makes it
 * impossible. Absent, the report goes to stderr as it always did.
 *
 * **There is no output cap here any more.** `RLIMIT_FSIZE` was it, and it does
 * not apply to a pipe -- keeping it would have gone on capping the scratch
 * while silently not capping the thing it was written for. The Runner counts
 * what it reads.
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

/* Where the report goes.
 *
 * **stderr until told otherwise, and that is the transition and the fallback
 * at once.** A Runner that names no channel gets the old behaviour, which is
 * what keeps an older image and a newer one both judgeable while this moves.
 * Once the child is running its own stderr is `/dev/null`, so this is also the
 * one descriptor by which anything can still be said. */
static int report_fd = STDERR_FILENO;

/* One `write`, because two could interleave with whatever else holds this fd. */
static void say(const char *text) {
    size_t left = strlen(text);
    const char *at = text;
    while (left > 0) {
        ssize_t wrote = write(report_fd, at, left);
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

/* The report's channel, taken from the environment and then removed from it.
 *
 * **Scrubbed like the nonce**, for a weaker reason: a submission that learnt the
 * path could open it and write a line of its own, and although the nonce is
 * what makes such a line ignorable, a channel it cannot name is one it cannot
 * try. Absent means stderr, which is where the report went until 2026-09-05.
 *
 * A path that is given and cannot be opened is fatal. There is no third
 * behaviour: a Runner that named a channel is waiting on it, and a shim that
 * quietly wrote somewhere else would leave that Runner waiting for ever. */
static void take_report_channel(void) {
    static const char key[] = "AJ_SHIM_REPORT=";
    for (char **entry = environ; *entry != NULL; entry++) {
        if (strncmp(*entry, key, sizeof key - 1) != 0) continue;

        char *value = *entry + sizeof key - 1;
        char path[512];
        snprintf(path, sizeof path, "%s", value);
        memset(value, 'x', strlen(value));
        unsetenv("AJ_SHIM_REPORT");

        int fd = open(path, O_WRONLY | O_CLOEXEC);
        if (fd < 0) fatal("cannot open the report channel");
        report_fd = fd;
        return;
    }
}

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

    /* **Output, then the report, then the input, and the order is the point.**
     *
     * Every one of these may be a named pipe, and opening one for writing waits
     * until somebody opens the reading end. So the report channel is opened
     * before the input: a shim that blocked on an input nobody was feeding, and
     * only then found it had nowhere to complain, would fail silently.
     *
     * The output is opened first because it is the one the Runner is certainly
     * waiting on -- it opened that end before the container was started. */
    int output = open(argv[2], O_WRONLY | O_CLOEXEC);
    if (output < 0) fatal("cannot open the output");

    take_report_channel();

    int input = open(argv[1], O_RDONLY | O_CLOEXEC);
    if (input < 0) fatal("cannot open the input file");

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

        /* **The submission's own stderr goes nowhere.**
         *
         * Nothing reads it: a judged run's stderr is surfaced by no screen, no
         * document and no attachment -- only a *build* container's is, and that
         * is a different container. Until now it was collected and thrown away,
         * which cost the daemon a write per byte for something nobody would
         * ever see.
         *
         * The shim's own stderr is untouched, and after this the two are not
         * the same descriptor: `report_fd` was resolved before the fork. */
        /* **Move the report out of the way first, where it is still stderr.**
         * With no channel named, `report_fd` *is* descriptor 2 -- and the very
         * next line puts `/dev/null` there. Without this the child's own
         * "failed exec" line, which is how a program that is not there is
         * reported at all, would be written into nothing.
         *
         * Close-on-exec, because a submission that does start has no business
         * holding it. */
        if (report_fd == STDERR_FILENO) {
            int kept = fcntl(report_fd, F_DUPFD_CLOEXEC, 3);
            if (kept < 0) fatal("cannot keep the report channel");
            report_fd = kept;
        }

        int nowhere = open("/dev/null", O_WRONLY | O_CLOEXEC);
        if (nowhere < 0) fatal("cannot open /dev/null for the submission's stderr");
        if (dup2(nowhere, STDERR_FILENO) < 0) fatal("dup2 /dev/null onto stderr");

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

    /* **The parent lets go of both, and that is what makes an end an end.**
     *
     * These are the child's now. While the shim still holds the writing end of
     * the output, a reader sees no end-of-file however long ago the program
     * finished -- it would arrive when the shim exits, which is after the
     * report, which is after `wait4`. A reader that has to wait for that cannot
     * decide anything early. */
    close(input);
    close(output);

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
