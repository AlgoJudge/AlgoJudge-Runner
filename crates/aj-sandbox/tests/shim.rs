//! The measuring shim, run for real.
//!
//! **Every measured verdict now rests on this program**, so it is exercised
//! rather than reasoned about: the shim is compiled from the source the images
//! build, run against small programs compiled beside it, and its report read
//! back off standard error exactly as the sandbox reads it.
//!
//! What is *not* here is the one thing that needs a PID namespace: a child that
//! escapes with `setsid` and is caught by `kill(-1)`. Outside a namespace that
//! call means every process the user owns, so the shim refuses to make it
//! unless it is PID 1 — which is what lets the rest of it be tested here at all.
//! `tests/adversarial.rs` proves that case in a container.
//!
//! Skipped where there is no `cc`, rather than failed: this is a Rust workspace
//! and a C compiler is not one of its declared requirements.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NONCE: &str = "0123456789abcdef0123456789abcdef";

/// A program that does one measurable thing, so a test can name the thing.
const HELPER: &str = r#"
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long long now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &t);
    return (long long)t.tv_sec * 1000000 + t.tv_nsec / 1000;
}

int main(int argc, char **argv) {
    const char *what = argc > 1 ? argv[1] : "";

    if (!strcmp(what, "spend")) {
        long long want = atoll(argv[2]) * 1000, began = now_us();
        volatile unsigned long spun = 0;
        while (now_us() - began < want) spun++;
        printf("spun\n");
    } else if (!strcmp(what, "burn")) {
        /* Arithmetic and nothing else -- no clock, no syscall, so what it
         * spends is the program's own. `spend` cannot serve: it asks the time
         * on every pass and the kernel does that work. */
        volatile unsigned long long sum = 0;
        for (long long i = 0; i < atoll(argv[2]); i++) sum += (unsigned long long)i;
        printf("burnt %llu\n", sum);
    } else if (!strcmp(what, "grow")) {
        long long mib = atoll(argv[2]);
        char *held = malloc((size_t)mib * 1024 * 1024);
        for (long long i = 0; i < mib * 1024 * 1024; i += 4096) held[i] = 1;
        printf("grew %lld\n", mib);
    } else if (!strcmp(what, "exit")) {
        return atoi(argv[2]);
    } else if (!strcmp(what, "fault")) {
        volatile int *nowhere = 0;
        *nowhere = 1;
    } else if (!strcmp(what, "who")) {
        printf("uid %d\n", (int)getuid());
    } else if (!strcmp(what, "environ")) {
        /* Whatever this can read of its parent, and of itself. */
        char path[64];
        snprintf(path, sizeof path, "/proc/%d/environ", (int)getppid());
        for (int i = 0; i < 2; i++) {
            const char *from = i ? "/proc/self/environ" : path;
            FILE *f = fopen(from, "rb");
            if (!f) { printf("%s refused\n", from); continue; }
            char buffer[65536];
            size_t got = fread(buffer, 1, sizeof buffer - 1, f);
            buffer[got] = 0;
            int found = 0;
            for (size_t at = 0; at < got; at++)
                if (!strncmp(buffer + at, "NONCE_HERE", 32)) found = 1;
            printf("%s nonce=%d\n", from, found);
            fclose(f);
        }
    } else if (!strcmp(what, "echo")) {
        char line[256];
        if (fgets(line, sizeof line, stdin)) printf("read %s", line);
        else printf("read nothing\n");
    } else if (!strcmp(what, "noisy")) {
        fprintf(stderr, "the program's own complaint\n");
    }
    fflush(stdout);
    return 0;
}
"#;

struct Built {
    shim: PathBuf,
    helper: PathBuf,
    input: PathBuf,
    /// The directory the child's stdout is written into, one file per run.
    ///
    /// **Per run and not per suite**, which cost a debugging round: the tests
    /// share one `Built` and cargo runs them in parallel, so a single path had
    /// them truncating each other's output and four of them failed together
    /// while each passed alone.
    outputs: PathBuf,
}

/// One run of the shim: what the process did, and what the child wrote.
///
/// Derefs to the process output so the assertions that were here before —
/// status, the shim's own stderr — read exactly as they did.
struct Ran {
    process: Output,
    /// What the child printed, read back from the file the shim wrote it to.
    written: Vec<u8>,
}

impl std::ops::Deref for Ran {
    type Target = Output;
    fn deref(&self) -> &Output {
        &self.process
    }
}

impl Ran {
    /// The process alone, for the refusals: a shim that refused wrote nothing,
    /// so there is no file to compare and the arms only need to agree.
    fn process(self) -> Output {
        self.process
    }
}

fn compile(source: &str, into: &Path, name: &str) -> Option<PathBuf> {
    let file = into.join(format!("{name}.c"));
    std::fs::write(&file, source).expect("write the source");
    let out = into.join(name);
    let built = Command::new("cc")
        .args(["-O1", "-Wall", "-Wextra", "-o"])
        .arg(&out)
        .arg(&file)
        .output()
        .ok()?;
    assert!(
        built.status.success(),
        "{name} did not compile: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    Some(out)
}

/// Everything a test needs, or `None` where this host has no C compiler.
fn build() -> Option<Built> {
    let root = std::env::temp_dir().join(format!("aj-shim-tests-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("a place to build in");

    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/shim/aj-shim.c");
    let shim = compile(
        &std::fs::read_to_string(&source).expect("the shim's source"),
        &root,
        "aj-shim",
    )?;
    let helper = compile(&HELPER.replace("NONCE_HERE", NONCE), &root, "helper")?;

    let input = root.join("input");
    std::fs::write(&input, "42 the input line\n").expect("an input file");

    Some(Built {
        shim,
        helper,
        input,
        outputs: root.join("outputs"),
    })
}

fn run(built: &Built, args: &[&str], nonce: Option<&str>) -> Ran {
    let at = built.output_for(&args.join("-"));
    let mut command = Command::new(&built.shim);
    command
        .arg(&built.input)
        .arg(&at)
        .arg(&built.helper)
        .args(args);
    match nonce {
        Some(nonce) => command.env("AJ_SHIM_NONCE", nonce),
        None => command.env_remove("AJ_SHIM_NONCE"),
    };
    let process = command.output().expect("the shim runs");
    Ran {
        written: std::fs::read(&at).unwrap_or_default(),
        process,
    }
}

impl Built {
    /// A path of this run's own, made unique by the arguments and a counter:
    /// two tests may run the same helper mode at the same moment.
    fn output_for(&self, what: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        std::fs::create_dir_all(&self.outputs).expect("a place for the output");
        // **Writable by anybody, and only here.** One test runs the shim as
        // 65534 to prove the nonce scrub does not depend on privilege, and a
        // root-owned directory would stop it opening the file at all -- which
        // is the production arrangement and the point of it: there the shim is
        // root, the directory is root's, and the submission gets the descriptor
        // and never a path. `container_user` and the mount are what hold that,
        // not this harness.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.outputs, std::fs::Permissions::from_mode(0o777));
        }
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let at = self.outputs.join(format!("{what}-{n}"));
        // **Made here, because the shim no longer makes it.** In the product
        // the far end is a named pipe the Runner created before the container
        // was started; the shim opens what it is given and does not create,
        // so a destination that is not there is a Runner that did not do its
        // half rather than something to paper over.
        std::fs::File::create(&at).expect("a destination for the output");
        // **Writable by anybody, and only here.** One test runs the shim as
        // 65534 to prove the nonce scrub does not depend on privilege, and a
        // root-owned destination would stop it writing at all. In the product
        // the far end is a 0600 pipe the Runner made and the shim is root when
        // it opens it, which is what keeps the submission away from it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o666));
        }
        at
    }
}

/// The line the sandbox looks for, parsed the way it parses it.
struct Report {
    exit: i32,
    signal: i32,
    cpu_us: u64,
    peak_bytes: u64,
    /// The total split into the program's own work and the kernel's work on its
    /// behalf. The wall clock sits between them on the line and is skipped: the
    /// sandbox has its own and does not read the shim's.
    user_us: u64,
    system_us: u64,
    rest: String,
}

fn report_in(output: &Output) -> Option<Report> {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let marker = format!("{NONCE} aj-shim1 ");
    let mut rest = String::new();
    let mut last: Option<String> = None;
    for line in stderr.split_inclusive('\n') {
        match line.trim_end().strip_prefix(marker.as_str()) {
            None => rest.push_str(line),
            Some(said) => last = Some(said.to_owned()),
        }
    }
    let said = last?;
    let mut fields = said.strip_prefix("ok ")?.split_whitespace();
    Some(Report {
        exit: fields.next()?.parse().ok()?,
        signal: fields.next()?.parse().ok()?,
        cpu_us: fields.next()?.parse().ok()?,
        peak_bytes: fields.next()?.parse().ok()?,
        user_us: fields.nth(1)?.parse().ok()?,
        system_us: fields.next()?.parse().ok()?,
        rest,
    })
}

/// **Compiled once for the whole file.** Tests run in parallel, and building
/// per test had them writing the same binaries over each other while another
/// test was executing one -- which came back as a program that appeared to do
/// nothing at all.
fn built() -> Option<&'static Built> {
    static ONCE: std::sync::OnceLock<Option<Built>> = std::sync::OnceLock::new();
    ONCE.get_or_init(build).as_ref()
}

macro_rules! built {
    () => {
        match built() {
            Some(built) => built,
            None => {
                eprintln!("no cc on this host, so the shim was not exercised");
                return;
            }
        }
    };
}

#[test]
fn it_reports_the_processor_time_the_child_spent() {
    let built = built!();
    let output = run(built, &["spend", "200"], Some(NONCE));
    let said = report_in(&output).expect("a report");

    // Generous on both sides: this is a shared machine. What it is really
    // asserting is that the figure is the child's work rather than a constant,
    // a zero, or the whole process tree.
    assert!(
        (150_000..400_000).contains(&said.cpu_us),
        "200 ms of work reported as {} us",
        said.cpu_us
    );
    assert_eq!(said.exit, 0);
    assert_eq!(said.signal, 0);
}

/// **The defect this program exists to remove.** `ru_maxrss` survives `fork`
/// and `exec`, so a child forked from a large parent reports the parent's
/// high-water mark — measuring six Python solutions from a PyPy driver returned
/// its own 64 MiB for every one of them. The shim is small, and this is the
/// assertion that keeps it that way: a child that touches 64 MiB has to come
/// back as roughly 64 MiB, whatever the parent is.
#[test]
fn a_childs_peak_memory_is_the_childs_own() {
    let built = built!();
    let small = report_in(&run(built, &["exit", "0"], Some(NONCE))).expect("a report");
    let large = report_in(&run(built, &["grow", "64"], Some(NONCE))).expect("a report");

    assert!(
        large.peak_bytes > 60 * 1024 * 1024,
        "64 MiB touched, {} bytes reported",
        large.peak_bytes
    );
    assert!(
        small.peak_bytes < 16 * 1024 * 1024,
        "a program that allocates nothing reported {} bytes",
        small.peak_bytes
    );
}

/// The runtime reports PID 1's exit code, and PID 1 is the shim now. Returning
/// its own success would turn every killed program into a clean exit.
#[test]
fn the_childs_fate_is_worn_as_the_shims_own() {
    let built = built!();

    let faulted = run(built, &["fault"], Some(NONCE));
    assert_eq!(
        faulted.status.code(),
        Some(128 + 11),
        "a fault is 128 + SIGSEGV"
    );
    assert_eq!(report_in(&faulted).expect("a report").signal, 11);

    let refused = run(built, &["exit", "42"], Some(NONCE));
    assert_eq!(refused.status.code(), Some(42));
    assert_eq!(report_in(&refused).expect("a report").exit, 42);
}

#[test]
fn the_input_file_arrives_as_standard_input() {
    let built = built!();
    let output = run(built, &["echo"], Some(NONCE));

    // **Read from the file, because that is where stdout now goes.** The shim
    // opens it and `dup2`s it onto the child's stdout; a test still reading the
    // shim's own stdout would see nothing, and could then be made to pass by a
    // shim that redirected into a void.
    assert_eq!(
        String::from_utf8_lossy(&output.written),
        "read 42 the input line\n"
    );
}

/// **The report goes where it is told, and stderr carries nothing.**
///
/// `AJ_SHIM_REPORT` names a channel of the shim's own. In the product it is a
/// pipe the Runner is reading; here it is a file, because what is being tested
/// is that the shim writes there and not that a pipe works.
///
/// Both halves matter and neither implies the other: a shim that wrote to the
/// channel *and* to stderr would pass a test that only looked at the channel,
/// and would go on costing the daemon a write per byte of it.
#[test]
fn the_report_travels_on_the_channel_it_was_given() {
    let built = built!();
    let at = built.output_for("report-channel");
    let out = built.output_for("report-channel-stdout");

    let output = Command::new(&built.shim)
        .arg(&built.input)
        .arg(&out)
        .arg(&built.helper)
        .arg("noisy")
        .env("AJ_SHIM_NONCE", NONCE)
        .env("AJ_SHIM_REPORT", &at)
        .output()
        .expect("the shim runs");

    let channel = std::fs::read(&at).unwrap_or_default();
    let said = String::from_utf8_lossy(&channel);
    assert!(
        said.contains(NONCE) && said.contains("aj-shim1 ok "),
        "the report went to the channel it was given: {said}",
    );
    assert!(
        output.stderr.is_empty(),
        "and nowhere else: {:?}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// **A channel that was named and cannot be opened is fatal.**
///
/// There is no third behaviour, and the reason is what the Runner does with it:
/// having named a channel, it is reading that channel. A shim that quietly fell
/// back to stderr would leave it reading an empty pipe until something else
/// gave up — an infrastructure failure reported as a timeout, which is a
/// verdict against somebody's program.
#[test]
fn a_report_channel_that_cannot_be_opened_is_fatal() {
    let built = built!();
    let out = built.output_for("report-missing-stdout");

    let output = Command::new(&built.shim)
        .arg(&built.input)
        .arg(&out)
        .arg(&built.helper)
        .arg("exit")
        .arg("0")
        .env("AJ_SHIM_NONCE", NONCE)
        .env(
            "AJ_SHIM_REPORT",
            built.shim.parent().unwrap().join("not-a-channel"),
        )
        .output()
        .expect("the shim runs");

    assert_eq!(
        output.status.code(),
        Some(125),
        "the shim failed, and said so as itself rather than as the submission",
    );
}

/// **The submission cannot reach the report's channel**, and this is what
/// replaced "the report is written last".
///
/// It used to share stderr with whatever the program printed there, so the
/// ordering was the defence: the shim killed everything else in the namespace
/// and then wrote, last. The submission's stderr is `/dev/null` now, so the two
/// do not share a channel at all -- a stronger thing to be able to say, and a
/// cheaper one to keep true.
#[test]
fn nothing_the_program_writes_reaches_the_report() {
    let built = built!();
    let output = run(built, &["noisy"], Some(NONCE));
    let said = report_in(&output).expect("a report");

    assert_eq!(
        said.rest, "",
        "the report's channel carried the report and nothing else",
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("complaint"),
        "the submission's own stderr went nowhere: {stderr}",
    );
}

/// A failure of the shim is infrastructure, and has to be distinguishable from
/// a submission that exited with the same code — which is what the report line
/// is for, since 125 alone cannot say it.
#[test]
fn every_way_the_shim_can_fail_says_so_and_measures_nothing() {
    let built = built!();

    for (args, expected, because) in [
        (vec!["exit", "0"], 125, "no nonce"),
        (vec!["exit", "0"], 127, "no such program"),
        (vec!["exit", "0"], 125, "no input file"),
    ] {
        let output = match because {
            "no nonce" => run(built, &args, None).process(),
            "no such program" => Command::new(&built.shim)
                .arg(&built.input)
                .arg(built.output_for("no-such-program"))
                .arg(built.shim.parent().unwrap().join("not-a-program"))
                .env("AJ_SHIM_NONCE", NONCE)
                .output()
                .expect("the shim runs"),
            _ => Command::new(&built.shim)
                .arg(built.shim.parent().unwrap().join("not-an-input"))
                .arg(built.output_for("no-such-input"))
                .arg(&built.helper)
                .env("AJ_SHIM_NONCE", NONCE)
                .output()
                .expect("the shim runs"),
        };

        assert_eq!(output.status.code(), Some(expected), "{because}");
        let stderr = String::from_utf8_lossy(&output.stderr);

        if because == "no such program" {
            // **The shim did not fail here, the submission did.** It forked, the
            // child could not exec, and the child's exit is reported faithfully
            // -- 127, which is the code a shell has always given for this. What
            // says what went wrong is the child's own line beside it.
            assert!(
                stderr.contains("aj-shim1 failed exec"),
                "{because}: {stderr}"
            );
            assert_eq!(report_in(&output).expect("a report").exit, 127);
        } else {
            assert!(report_in(&output).is_none(), "{because} measured something");
            assert!(stderr.contains("aj-shim1 failed"), "{because}: {stderr}");
        }
    }
}

/// The nonce is what makes a forged report hard to write. It is passed in the
/// environment and never in the arguments, because `/proc/1/cmdline` is
/// world-readable and `/proc/1/environ` is not — and it is scrubbed in place
/// besides, because `unsetenv` does not touch what `/proc` reports.
#[test]
fn the_nonce_reaches_neither_the_child_nor_its_view_of_the_shim() {
    let built = built!();
    let output = run(built, &["environ"], Some(NONCE));
    let seen = String::from_utf8_lossy(&output.written);

    for line in seen.lines() {
        assert!(
            line.ends_with("nonce=0") || line.ends_with("refused"),
            "the child could read the nonce: {line}"
        );
    }
    assert_eq!(seen.lines().count(), 2, "both were checked: {seen}");
}

/// Where the shim is root the submission must not be, and the drop is verified
/// rather than assumed: a drop that silently failed would run a submission with
/// privileges. Where it is already unprivileged there is nothing to drop and the
/// submission runs as whoever the sandbox said, which is the same answer.
#[test]
fn the_submission_never_runs_as_root() {
    let built = built!();
    let output = run(built, &["who"], Some(NONCE));
    let said = String::from_utf8_lossy(&output.written);

    assert!(said.starts_with("uid "), "got {said}");
    assert_ne!(said.trim(), "uid 0", "the submission was left as root");

    // Whoever owns this process, without reaching for a crate to ask.
    let root = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").unwrap()) == 0;
    if root {
        assert_eq!(said.trim(), "uid 65534", "root drops to nobody");
    }
}

/// **What proves the scrub, which the test above cannot.** Where the shim is
/// root the child is refused `/proc/1/environ` whatever the shim did with its
/// own memory, so that test would pass with the scrub deleted. Run unprivileged
/// -- which is a real deployment, an image the sandbox found no shim in having
/// been started as `nobody` -- the child shares the user and may read it, and
/// overwriting the bytes is the only thing standing between it and the nonce.
#[test]
fn unprivileged_the_scrub_is_what_hides_the_nonce() {
    use std::os::unix::process::CommandExt as _;

    let built = built!();
    let root = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata("/proc/self").unwrap()) == 0;
    if !root {
        return; // Already unprivileged: the case above is this one.
    }

    let at = built.output_for("unprivileged-environ");
    let _ = Command::new(&built.shim)
        .arg(&built.input)
        .arg(&at)
        .arg(&built.helper)
        .arg("environ")
        .env("AJ_SHIM_NONCE", NONCE)
        .uid(65534)
        .gid(65534)
        .output()
        .expect("the shim runs");

    let written = std::fs::read(&at).unwrap_or_default();
    let seen = String::from_utf8_lossy(&written);
    assert!(
        seen.contains("/proc/") && seen.contains("nonce=0"),
        "the parent's environ was readable and had to be empty of it: {seen}"
    );
    assert!(
        !seen.contains("nonce=1"),
        "the nonce survived where the child could read it: {seen}"
    );
}

/// **The catalogue names `python3`, not a path to it.** The shell this replaced
/// searched `PATH`, so a shim that did not would turn every interpreted
/// language in the catalogue into a program that is not there -- which is what
/// it did, and what this pins.
#[test]
fn a_program_named_without_a_path_is_found() {
    let built = built!();
    let output = Command::new(&built.shim)
        .arg(&built.input)
        .arg(built.output_for("env-on-path"))
        .arg("env")
        .env("AJ_SHIM_NONCE", NONCE)
        .output()
        .expect("the shim runs");

    assert_eq!(output.status.code(), Some(0), "`env` was not found on PATH");
    assert_eq!(report_in(&output).expect("a report").exit, 0);
}

/// **The halves are the whole, and they are the halves.** A total that grew
/// under load says nothing about which of the two grew, and they have different
/// causes: one is the program, the other is the kernel faulting its pages in and
/// reading its input for it. This pins that the shim reports both and that they
/// still add up to what a participant is judged on -- an arithmetic slip here
/// would be invisible in every other test, because the total is what they read.
#[test]
fn the_report_splits_the_total_into_the_program_and_the_kernel() {
    let built = built!();

    // Arithmetic is the program's own work and nothing else.
    let spun = report_in(&run(built, &["burn", "200000000"], Some(NONCE))).expect("a report");
    assert_eq!(
        spun.user_us + spun.system_us,
        spun.cpu_us,
        "the halves must be the total: {} user, {} system, {} together",
        spun.user_us,
        spun.system_us,
        spun.cpu_us,
    );
    assert!(
        spun.user_us > 0,
        "two hundred million additions are the program's own time, and it read {} user",
        spun.user_us,
    );

    // And touching thirty-two megabytes is the kernel's: every page is a fault
    // it has to serve.
    let grown = report_in(&run(built, &["grow", "32"], Some(NONCE))).expect("a report");
    assert_eq!(grown.user_us + grown.system_us, grown.cpu_us);
    assert!(
        grown.system_us > 0,
        "faulting in thirty-two megabytes is work done for it, and it read {} system",
        grown.system_us,
    );
}
