//! Program → expected result.
//!
//! **This is a release gate, not a suite of examples** (D-8, 2026-08-06). Every
//! row is a thing untrusted code will try, and every one must hold against every
//! execution path the Runner supports, unchanged. A suite that is not a gate is
//! not a gate.
//!
//! Each case asserts two things: that the program was stopped correctly, **and
//! that the host is unchanged afterwards** — no container left running. The
//! second is the one that catches a sandbox which contains a program by leaking
//! a process, and five cases were missing it until 2026-08-31.
//!
//! The one exception is `a_sweep_leaves_another_runners_containers_alone`, which
//! leaves a container on purpose — it is the case that proves a sweep spares
//! somebody else's.
//!
//! ```text
//! docker pull alpine:3
//! ./x test -p aj-sandbox --test adversarial -- --include-ignored --test-threads=1
//! ```
//!
//! `alpine` rather than a language image on purpose: these test the isolation,
//! not the toolchain, and a shell is all that is needed to misbehave.

use std::time::Duration;

use aj_sandbox::{Docker, Error, Mount, Profile, Sandbox, Stopped};

const IMAGE: &str = "alpine:3";

/// Whose containers this suite's are. See `Docker::connect`.
const SUITE: &str = "test-adversarial";

/// A sandbox with the image present and nothing left over from before.
///
/// The sweep is not tidiness — it is the production behaviour, run here for the
/// same reason it runs at start: sibling containers outlive whatever made them.
async fn sandbox() -> Docker {
    // A name of this suite's own, and a fixed one. Fixed so a previous run's
    // leftovers are still swept; its own so the sweep below cannot reach a
    // Runner judging on the same host — which, before instance labels existed,
    // it did, killing live evaluations and then failing these tests over the
    // containers it found.
    let docker = Docker::connect(SUITE).expect("a container runtime");

    // The Runner requires cgroup v2 and `preflight` refuses without it. That is
    // the right behaviour and is **not relaxed here** — the escape hatch is in
    // this harness, is opt-in, and says so on every run.
    //
    // It exists because a developer machine is often Docker Desktop, which may
    // still report v1, and the alternative is that the isolation suite is never
    // run outside CI. **Every case here passes on v1**, memory included — what
    // v1 lacks is honest measurement of peak memory and CPU time, which is what
    // the Runner refuses over and which nothing in this file asserts.
    if let Err(e) = docker.preflight().await {
        assert!(
            std::env::var("AJ_SANDBOX_ALLOW_CGROUP_V1").is_ok(),
            "{e}\n\nSet AJ_SANDBOX_ALLOW_CGROUP_V1=1 to run these anyway, and read \
             the results knowing the host is below what the Runner requires.",
        );
        eprintln!("warning: running the isolation suite below specification — {e}");
    }

    docker.ensure_image(IMAGE).await.expect("the test image");
    docker.sweep().await.expect("a clean slate");
    docker
}

fn shell(script: &str) -> Profile {
    Profile::new(IMAGE, vec!["/bin/sh".into(), "-c".into(), script.into()])
        .wall_clock(Duration::from_secs(10))
}

/// How many sandbox containers were left behind. Asked through the runtime
/// API rather than the `docker` command, because these tests run wherever the
/// build does and that is not always somewhere with a CLI.
async fn leftovers(docker: &Docker) -> usize {
    docker.sweep().await.expect("a sweep")
}

// ── The baseline ────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a container runtime"]
async fn an_ordinary_program_runs_and_its_exit_code_survives() {
    let docker = sandbox().await;

    let outcome = docker
        .run(&shell("echo hello; exit 3"))
        .await
        .expect("the run");

    assert_eq!(outcome.exit_code, 3);
    assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "hello");
    assert_eq!(outcome.stopped, Stopped::OnItsOwn);
    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

// ── A1 — it never stops ─────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a container runtime"]
async fn an_infinite_loop_is_killed_at_the_wall_clock() {
    let docker = sandbox().await;

    let outcome = docker
        .run(&shell("while :; do :; done").wall_clock(Duration::from_secs(3)))
        .await
        .expect("the run");

    assert_eq!(outcome.stopped, Stopped::WallClock);
    assert!(
        outcome.wall_time < Duration::from_secs(10),
        "it took {:?}, so the deadline did not apply",
        outcome.wall_time,
    );
    assert_eq!(leftovers(&docker).await, 0);
}

// ── A2 — it multiplies ──────────────────────────────────────────────────────

/// The pids limit, asserted directly rather than through a proxy.
///
/// Counting host processes would be the obvious check and is the wrong one:
/// these tests run wherever the build does, in a different PID namespace from
/// the sibling they started. What can be observed honestly is that the program
/// could not create more processes than it was allowed.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn a_fork_bomb_cannot_outgrow_its_process_limit() {
    let docker = sandbox().await;

    // Counts how many background processes it manages to start before the
    // kernel stops it, and prints the number.
    let outcome = docker
        .run(
            &shell("i=0; while [ $i -lt 500 ]; do sleep 30 & i=$((i+1)); done; echo $i")
                .pids(16)
                .wall_clock(Duration::from_secs(10)),
        )
        .await
        .expect("the run");

    let reached: usize = String::from_utf8_lossy(&outcome.stdout)
        .trim()
        .lines()
        .last()
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);

    assert!(
        reached < 500,
        "it started {reached} processes against a limit of 16, so nothing stopped it",
    );
    assert_eq!(leftovers(&docker).await, 0);
}

// ── What the run cost ───────────────────────────────────────────────────────

/// Peak memory is measured, and it is the program's own.
///
/// Not "a number arrived": asserted against a **known allocation**, because a
/// measurement nobody checked the magnitude of is how a plausible-looking wrong
/// number reaches a participant. A program that touches 64 MiB must report a
/// peak at least that large and not wildly more — roughly 2 MiB of floor is
/// expected and it does not scale, so a generous ceiling still catches an error
/// of kind rather than of degree.
///
/// Skipped rather than failed where the Runner was given nowhere to measure
/// from: `AJ_DOCKER_SOCKET=1` mounts the cgroup hierarchy, and without it the
/// honest answer is `None` by design.
#[tokio::test]
#[ignore = "needs a container runtime and a writable cgroup mount"]
async fn peak_memory_is_measured_and_is_the_programs_own() {
    let docker = sandbox().await;

    // `dd` into the scratch tmpfs: tmpfs pages are charged to the cgroup, so
    // this allocates 64 MiB in a way that is exact rather than approximate.
    let outcome = docker
        .run(
            &shell("dd if=/dev/zero of=/tmp/block bs=1M count=64 2>/dev/null")
                .memory_bytes(512 * 1024 * 1024)
                .tmpfs_bytes(128 * 1024 * 1024),
        )
        .await
        .expect("the run");

    let Some(peak) = outcome.peak_memory_bytes else {
        eprintln!("peak memory was not measured: no writable cgroup root. Skipping.");
        return;
    };

    assert!(
        peak >= 64 * 1024 * 1024,
        "64 MiB was written, so the peak cannot be below it: got {peak} bytes",
    );
    assert!(
        peak < 128 * 1024 * 1024,
        "the floor is about 2 MiB, so twice the allocation means the wrong cgroup: got {peak} bytes",
    );

    assert!(
        outcome.cpu_time.is_some(),
        "cpu.stat sits beside memory.peak in the same cgroup",
    );
    assert_eq!(leftovers(&docker).await, 0);
}

// ── A3 — it eats memory ─────────────────────────────────────────────────────

/// A cgroup OOM, not a timeout. The two are different things to tell a
/// participant, and reporting one as the other sends them optimising the wrong
/// thing.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn exhausting_memory_is_an_out_of_memory_kill() {
    let docker = sandbox().await;

    // Allocation has to be fast and unambiguous, or this test measures the
    // workload rather than the limit.
    //
    // A first version grew a string in `awk`, which in busybox is quadratic:
    // under a 16 MiB limit it never reached the limit inside twenty seconds and
    // was killed by the wall clock instead. That was read as "the host does not
    // enforce memory limits" and it was **wrong** — a direct probe on the same
    // host (cgroup v1) showed `OOMKilled=true`. Writing to a tmpfs charges pages
    // to the cgroup immediately and gets there in a fraction of a second.
    let outcome = docker
        .run(
            &shell("dd if=/dev/zero of=/tmp/big bs=1M count=512")
                .memory_bytes(32 * 1024 * 1024)
                .tmpfs_bytes(1024 * 1024 * 1024)
                .wall_clock(Duration::from_secs(30)),
        )
        .await
        .expect("the run");

    assert_eq!(
        outcome.stopped,
        Stopped::Memory,
        "exit {} after {:?}",
        outcome.exit_code,
        outcome.wall_time,
    );
    assert_eq!(leftovers(&docker).await, 0);
}

// ── A4 — it reaches for the host ────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a container runtime"]
async fn the_root_filesystem_is_read_only() {
    let docker = sandbox().await;

    let outcome = docker
        .run(&shell(
            "touch /oops 2>/dev/null && echo WROTE || echo REFUSED",
        ))
        .await
        .expect("the run");

    assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "REFUSED");
    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

#[tokio::test]
#[ignore = "needs a container runtime"]
async fn scratch_space_is_writable_and_not_executable() {
    let docker = sandbox().await;

    let outcome = docker
        .run(
            &shell(
                "cp /bin/echo /tmp/e 2>/dev/null || { echo NOWRITE; exit 0; }; \
                 /tmp/e ran 2>/dev/null || echo NOEXEC",
            )
            .tmpfs_bytes(16 * 1024 * 1024),
        )
        .await
        .expect("the run");

    // Writable is the point of scratch space; executable is what turns
    // "produced a file" into "ran something nobody compiled".
    assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "NOEXEC");
    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

#[tokio::test]
#[ignore = "needs a container runtime"]
async fn a_read_only_mount_cannot_be_written() {
    let docker = sandbox().await;
    let (here, on_the_host) = fixture("read-only");
    std::fs::write(here.join("1a.in"), "7\n").unwrap();

    let outcome = docker
        .run(
            &shell("cat /in/1a.in; echo x > /in/1a.in 2>/dev/null && echo WROTE || echo REFUSED")
                .mount(Mount::read_only(&on_the_host, "/in")),
        )
        .await
        .expect("the run");

    let said = String::from_utf8_lossy(&outcome.stdout);
    assert!(said.contains('7'), "the input was not readable: {said}");
    assert!(said.contains("REFUSED"), "the input was writable: {said}");
    assert_eq!(std::fs::read_to_string(here.join("1a.in")).unwrap(), "7\n");
    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

/// A scratch directory, as this process sees it **and as the runtime daemon
/// does**.
///
/// The two differ whenever the thing calling the daemon is itself in a
/// container, which is true of these tests and will be true of a Runner
/// deployed by Compose. A bind mount is resolved by the **daemon**, so a path
/// that is real here and meaningless there produces an empty directory rather
/// than an error — which is the quietest possible failure.
///
/// **This is a deployment constraint, not a test artefact.** A Runner in a
/// container must be given its working directory in a form the daemon can
/// resolve: the same path on both sides, or a named volume.
fn fixture(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let relative = format!(".sandbox-fixtures/{name}");

    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(&relative);
    let _ = std::fs::remove_dir_all(&here);
    std::fs::create_dir_all(&here).expect("a fixture directory");

    let on_the_host = match std::env::var("AJ_HOST_WORKDIR") {
        Ok(root) => std::path::PathBuf::from(root).join(relative.replace('/', separator())),
        // Running directly on the host: the two are the same path.
        Err(_) => here.clone(),
    };

    (here, on_the_host)
}

fn separator() -> &'static str {
    if std::env::var("AJ_HOST_WORKDIR").is_ok_and(|w| w.contains('\\')) {
        "\\"
    } else {
        "/"
    }
}

// ── A5 — it reaches for the network ─────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a container runtime"]
async fn there_is_no_network() {
    let docker = sandbox().await;

    let outcome = docker
        .run(&shell(
            "wget -T 2 -q -O- http://1.1.1.1 >/dev/null 2>&1 && echo REACHED || echo BLOCKED",
        ))
        .await
        .expect("the run");

    assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "BLOCKED");
    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

// ── A6 — it floods ──────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "needs a container runtime"]
async fn flooding_output_is_stopped_at_the_cap() {
    let docker = sandbox().await;

    let outcome = docker
        .run(
            &shell("yes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .max_output_bytes(256 * 1024)
                .wall_clock(Duration::from_secs(20)),
        )
        .await
        .expect("the run");

    assert_eq!(outcome.stopped, Stopped::Output);
    assert!(
        outcome.stdout.len() <= 256 * 1024,
        "kept {} bytes against a cap of {}",
        outcome.stdout.len(),
        256 * 1024,
    );
    assert_eq!(leftovers(&docker).await, 0);
}

/// **The other half of A6, and the one that was wrong**: a program that floods
/// and then exits by itself.
///
/// `yes` above never stops, so the collector has long since raised its flag by
/// the time the container is killed. A program that prints past the cap and
/// returns 0 is the opposite order — the wait resolves the moment the process
/// exits, and the collector has still to open its log stream and read what the
/// runtime buffered, so the flag read at that moment is false.
///
/// **A short burst rather than a large one, deliberately.** Megabytes give the
/// collector time to cross the cap while the program is still writing, which is
/// the case the test above already covers; 64 KiB written and exited in a
/// millisecond is the one that loses the race. Measured against the old
/// ordering: `Stopped::OnItsOwn`, exit 0, and stdout silently truncated to the
/// 4 KiB cap. `pipeline.rs` scores that truncated prefix, so the participant is
/// told their answer is wrong and no output-limit verdict is ever produced for
/// a program that exits on its own.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn output_over_the_cap_is_the_verdict_when_the_program_exits_by_itself() {
    let docker = sandbox().await;

    let outcome = docker
        .run(
            &shell("yes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa | head -c 65536")
                .max_output_bytes(4 * 1024)
                .wall_clock(Duration::from_secs(20)),
        )
        .await
        .expect("the run");

    assert_eq!(
        outcome.stopped,
        Stopped::Output,
        "exit {} after {:?}, {} bytes kept",
        outcome.exit_code,
        outcome.wall_time,
        outcome.stdout.len(),
    );
    assert_eq!(leftovers(&docker).await, 0);
}

// ── A7 — it tries to persist ────────────────────────────────────────────────

/// One container per test, never reused. A fresh container is the only answer
/// that does not depend on cleanup having been written correctly.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn nothing_survives_from_one_run_to_the_next() {
    let docker = sandbox().await;

    let first = docker
        .run(&shell("echo remembered > /tmp/state; echo written").tmpfs_bytes(1024 * 1024))
        .await
        .expect("the first run");
    assert_eq!(String::from_utf8_lossy(&first.stdout).trim(), "written");

    let second = docker
        .run(&shell("cat /tmp/state 2>/dev/null || echo GONE").tmpfs_bytes(1024 * 1024))
        .await
        .expect("the second run");

    assert_eq!(String::from_utf8_lossy(&second.stdout).trim(), "GONE");

    // **`/dev/shm` as well**, which no profile here asks for.
    //
    // The container runtime mounts it as a 64 MiB tmpfs in every container and
    // a read-only root filesystem does not cover it — so a program that can
    // write nowhere else can still write there. It is charged to the memory
    // limit like any other tmpfs, and it is new with the container, but neither
    // of those is obvious from reading the profile. Asserted so that a runtime
    // change which started sharing it would fail here rather than in a contest.
    let wrote = docker
        .run(&shell("echo remembered > /dev/shm/state && echo written"))
        .await
        .expect("a run that writes to /dev/shm");
    assert_eq!(
        String::from_utf8_lossy(&wrote.stdout).trim(),
        "written",
        "/dev/shm is expected to be writable; if this fails the note above is stale",
    );

    let looked = docker
        .run(&shell("cat /dev/shm/state 2>/dev/null || echo GONE"))
        .await
        .expect("the run that looks for it");
    assert_eq!(String::from_utf8_lossy(&looked.stdout).trim(), "GONE");
    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

// ── A8 — it writes rather than prints ───────────────────────────────────────

/// A program cannot fill the host by writing a large file.
///
/// The output cap answers a program that *prints* too much; this is the other
/// half, and the two are different limits with different failure modes. `fsize`
/// turns "wrote a hundred gigabytes to scratch" into an immediate signal rather
/// than a slow one, and the tmpfs size is the second bound behind it.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn a_program_cannot_write_a_file_larger_than_it_is_allowed() {
    let docker = sandbox().await;

    let outcome = docker
        .run(
            &shell("dd if=/dev/zero of=/tmp/big bs=1M count=64 2>/dev/null; wc -c < /tmp/big")
                .tmpfs_bytes(256 * 1024 * 1024)
                .max_file_bytes(1024 * 1024)
                .wall_clock(Duration::from_secs(20)),
        )
        .await
        .expect("the run");

    let written: u64 = String::from_utf8_lossy(&outcome.stdout)
        .trim()
        .parse()
        .unwrap_or(u64::MAX);

    assert!(
        written <= 1024 * 1024,
        "it wrote {written} bytes against a limit of {}",
        1024 * 1024,
    );
    assert_eq!(leftovers(&docker).await, 0);
}

// ── Two Runners on one host ─────────────────────────────────────────────────

/// **A sweep is one Runner's, and it removes by force.**
///
/// Several Runners share one host and one daemon — an operator runs two to use
/// the machine, and a developer runs this suite while the development stack's
/// Runner is judging. Until instance labels existed, `sweep()` filtered on the
/// constant `algojudge.sandbox=1`, so the first thing either of them did on
/// starting was force-remove the other's running build, test and checker
/// containers. In the victim, `wait_container` returns, the inspect that
/// follows 404s, and every job it held fails as "a test could not be run".
///
/// The second half is this suite's own: `leftovers` asserts the count is zero,
/// so a co-resident Runner's live container made these tests fail, reporting a
/// leaked process that was somebody else's working one.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn a_sweep_leaves_another_runners_containers_alone() {
    let mine = sandbox().await;
    let theirs = Docker::connect("test-a-second-runner").expect("a container runtime");

    // Long enough to still be running when the sweep below happens, and well
    // inside the profile's own wall clock.
    let running = tokio::spawn(async move {
        let outcome = theirs.run(&shell("sleep 5; echo survived")).await;
        (theirs, outcome)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert_eq!(
        mine.sweep().await.expect("a sweep"),
        0,
        "it removed a container belonging to another Runner",
    );

    let (theirs, outcome) = running.await.expect("the other Runner's run");
    let outcome = outcome.expect("the other Runner's container was destroyed mid-run");
    assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "survived");
    assert_eq!(outcome.stopped, Stopped::OnItsOwn);

    // And it is still able to clear up after itself.
    assert_eq!(leftovers(&mine).await, 0);
    assert_eq!(theirs.sweep().await.expect("a sweep"), 0);
}

// ── A9 — it builds something enormous ────────────────────────────────────────

/// **What comes back from a build is bounded, and refused rather than cut.**
///
/// The artefact is whatever compiling untrusted code produced, and it is read
/// into the *trusted* process. It used to be collected chunk by chunk into a
/// `Vec<Bytes>` and then joined, so the whole of it existed twice at once with
/// nothing capping either copy — and `char pad[240*1024*1024] = {1};` is one
/// line of source.
///
/// The container's own `fsize` is the first bound and the better one, because
/// it makes an oversized artefact the participant's compilation error. This is
/// the second, and the one that holds if a backend ever applies the first
/// differently: the limit **we** apply, in the process that would otherwise
/// hold the bytes.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn an_artefact_over_the_cap_is_refused_rather_than_held() {
    let docker = sandbox().await;

    let outcome = docker
        .run(
            // `/tmp` rather than `/out`: a language image declares `/out` and
            // chowns it precisely because a container running as nobody cannot
            // create a directory at its own root, and `alpine` has no such
            // directory. On the writable layer either way, which is where a
            // real build's artefact goes.
            &shell("dd if=/dev/zero of=/tmp/program bs=1M count=8 2>/dev/null")
                .writable_root()
                .collect("/tmp", 1024 * 1024)
                .wall_clock(Duration::from_secs(20)),
        )
        .await;

    match outcome {
        Err(Error::Refused(said)) => assert!(
            said.contains("larger than"),
            "refused for the wrong reason: {said}",
        ),
        Err(other) => panic!("refused, but not for its size: {other}"),
        Ok(_) => panic!("eight megabytes came back against a cap of one"),
    }

    assert_eq!(leftovers(&docker).await, 0);
}

/// The same profile, under the cap: what a build makes still comes back whole.
///
/// A bound that refused everything would pass the test above and break every
/// submission, which is the failure a one-sided test does not see.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn an_artefact_under_the_cap_comes_back_whole() {
    let docker = sandbox().await;

    let outcome = docker
        .run(
            &shell("dd if=/dev/zero of=/tmp/program bs=1K count=64 2>/dev/null")
                .writable_root()
                .collect("/tmp", 1024 * 1024)
                .wall_clock(Duration::from_secs(20)),
        )
        .await
        .expect("the run");

    let collected = outcome.collected.expect("the artefact");
    assert!(
        collected.len() > 64 * 1024,
        "a tar of 64 KiB cannot be {} bytes",
        collected.len(),
    );
    assert_eq!(leftovers(&docker).await, 0);
}

// ── Whose process it is ─────────────────────────────────────────────────────

/// **Nobody, and nobody's group**: the identity is the sandbox's, never the
/// image's.
///
/// A submission's first move on landing somewhere is to find out who it is,
/// because uid 0 inside changes what everything else is worth — a root process
/// owns what it writes, edits anything mounted writable, and on the day a
/// runtime or kernel bug lets it out is uid 0 on the evaluation host too, since
/// without a user namespace the number is the same number on both sides. So
/// `docker.rs` sets `user` on every container and **does not read the image's
/// own `USER`**: a language image is a third party's build, `FROM` chains are
/// long, and "it runs as nobody" is not a property worth inheriting from one.
///
/// **Asked three ways, because the first two are the container describing
/// itself.** `id` reads `/etc/passwd`, which is a file in an untrusted image.
/// `/proc/self/status` is the kernel's own answer and carries **four** numbers
/// per line — real, effective, saved-set and filesystem — and the saved-set one
/// is the one worth having: a container started as root that drops to nobody
/// keeps 0 there and takes it back with a single `setuid`, which a test reading
/// `id -u` alone would call a pass. The third asks the kernel to *enforce*
/// something rather than to report it: `/etc/shadow` is `0640 root:shadow` in
/// this image, so it is readable by **owner permission alone** — no capability,
/// which is what makes it usable here, because `cap_drop: ALL` leaves a root in
/// this container with none and a probe needing `CAP_DAC_OVERRIDE` would fail
/// for both users and prove nothing.
///
/// The group is asserted apart from the user and that is not decoration:
/// `65534:0` reads `/etc/shadow` no better than `65534:65534` does, so the
/// shadow leg alone would miss a container handed root's *group* — the half
/// that decides what a group-writable mount is worth.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn a_program_runs_as_nobody_and_never_as_root() {
    let docker = sandbox().await;

    let outcome = docker
        .run(&shell(
            "echo \"uid=$(id -u)\"; echo \"gid=$(id -g)\"; echo \"groups=$(id -G)\"; \
             grep -E '^(Uid|Gid):' /proc/self/status; \
             cat /etc/shadow >/dev/null 2>&1 && echo shadow=READ || echo shadow=REFUSED",
        ))
        .await
        .expect("the run");

    let said = String::from_utf8_lossy(&outcome.stdout).into_owned();
    assert_eq!(
        outcome.stopped,
        Stopped::OnItsOwn,
        "the probe did not finish: exit {}, and it said {said:?}",
        outcome.exit_code,
    );

    // One `key=value` per line, and an absent line is a failure rather than a
    // default: a probe that printed nothing must not read as a pass.
    let field = |key: &str| -> String {
        said.lines()
            .find_map(|line| line.trim().strip_prefix(key))
            .unwrap_or_else(|| panic!("{key} was never printed. All of it: {said:?}"))
            .to_owned()
    };

    // `Uid:` and `Gid:` are real, effective, saved-set and filesystem.
    let kernel = |name: &str| -> Vec<String> {
        said.lines()
            .find(|line| line.starts_with(name))
            .unwrap_or_else(|| panic!("/proc/self/status had no {name} line: {said:?}"))
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect()
    };

    let uid = field("uid=");
    let gid = field("gid=");
    let groups = field("groups=");
    let shadow = field("shadow=");
    let kernel_uid = kernel("Uid:");
    let kernel_gid = kernel("Gid:");
    let nobody = vec!["65534".to_owned(); 4];

    assert_eq!(uid, "65534", "it ran as uid {uid}, and 0 is root");
    assert_eq!(gid, "65534", "it ran as gid {gid}, and 0 is root's group");
    assert_eq!(
        groups, "65534",
        "it was given the supplementary groups {groups:?}, which naming a uid should not do",
    );
    assert_eq!(
        kernel_uid, nobody,
        "real, effective, saved-set and filesystem uid: {kernel_uid:?}. A 0 in the third \
         is root one setuid away",
    );
    assert_eq!(
        kernel_gid, nobody,
        "real, effective, saved-set and filesystem gid: {kernel_gid:?}",
    );
    assert_eq!(
        shadow, "REFUSED",
        "/etc/shadow is 0640 root:shadow, so reading it means the process owns it: {shadow}",
    );

    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

// ── A10 — it reaches for a capability ───────────────────────────────────────

/// **No capabilities at all — and the bounding set is the assertion that
/// gates.**
///
/// A capability is what separates "a process running as nobody" from one that
/// can mount a filesystem, make a device node for the host's disk, or load a
/// module. `cap_drop: ALL` empties every set, and nothing in this suite checked
/// it until now.
///
/// **Three of the sets are already empty for a reason that is not ours, and a
/// test asserting only those would be no gate.** Measured on this host with
/// `cap_drop` deleted and everything else unchanged: `CapInh`, `CapPrm`,
/// `CapEff` and `CapAmb` are still all zeros, because the container runs as
/// `65534:65534` and the kernel clears permitted and effective on the drop to a
/// non-root uid. Exactly one set moves — `CapBnd`, to `00000000a80425fb`, the
/// fourteen capabilities the runtime grants unasked. So every set is asserted,
/// but `CapBnd` is the one carrying the gate, and the check below refuses to
/// pass on output that never mentioned it.
///
/// The bounding set is worth holding even while the effective one is empty: it
/// is the ceiling on what may ever be *regained*, so a binary carrying
/// capabilities in its extended attributes grants nothing outside it.
/// `no-new-privileges` answers the same move from the other side, and neither
/// is a reason to skip the other.
///
/// **The `mknod` is a live probe, not a second gate**, and the difference
/// matters. A block device for the host's disk is the first move of the classic
/// escape, and `/dev/shm` is where a read-only root still leaves somewhere to
/// put it. Measured directly: it succeeds with `CAP_MKNOD` and is refused
/// without it, so it is a real capability probe — but under this sandbox's uid
/// it is refused either way, so it corroborates the sets rather than reddening
/// with them.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn a_program_holds_no_capabilities_and_cannot_regain_any() {
    let docker = sandbox().await;

    // `/dev/shm` rather than a tmpfs this profile asks for: the runtime mounts
    // it in every container, so the probe runs against the profile a submission
    // actually gets. busybox has both `grep` and `mknod`.
    let outcome = docker
        .run(&shell(
            "grep ^Cap /proc/self/status; \
             mknod /dev/shm/disk b 8 0 2>/dev/null && echo MADE || echo REFUSED",
        ))
        .await
        .expect("the run");

    let said = String::from_utf8_lossy(&outcome.stdout);

    // Every `Cap*` line the kernel reports, rather than a list written here:
    // `CapAmb` is covered without this test naming a field a kernel may add or
    // drop.
    let sets: Vec<(&str, &str)> = said
        .lines()
        .filter(|line| line.starts_with("Cap"))
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim(), value.trim()))
        .collect();

    for &(name, value) in &sets {
        assert!(
            value.chars().all(|c| c == '0'),
            "{name} is {value}, so this container holds capabilities. \
             00000000a80425fb is the runtime's own default set, which is what \
             CapBnd reads when cap_drop stops being applied",
        );
    }

    // Not vacuous: an empty stdout satisfies the loop above, and a run killed
    // part way through would produce one.
    assert!(
        sets.iter().any(|&(name, _)| name == "CapBnd"),
        "no bounding set was reported, so the loop above proved nothing: {said}",
    );

    assert_eq!(
        said.lines().last().map(str::trim),
        Some("REFUSED"),
        "a block device node for the host's disk was created: {said}",
    );

    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

// ── A11 — it tries to gain a privilege ──────────────────────────────────────

/// **No privilege may be gained after the program starts** — asked of the
/// kernel, rather than assumed from the flag we passed it.
///
/// `no_new_privs` is what makes every other restriction here final. Dropping
/// every capability and running as 65534 are both undone by a single `execve`
/// of a setuid-root binary; this bit is what makes the kernel refuse that
/// transition, for the program and for every descendant, permanently — it is
/// inherited across `exec` and there is no call that clears it.
///
/// **Not hypothetical.** `alpine:3` ships no setuid binary, but the four images
/// that actually run submissions are Debian, and `debian:trixie-slim` ships
/// eight — `mount`, `su`, `passwd` and five more. An empty bounding set means
/// such an `execve` gains no *capabilities* here, but it runs as uid 0, which
/// decides every file-permission question inside the container. That is where
/// an escalation starts, not where it ends.
///
/// `docs/SECURITY.md` lists this flag as one of four rows with no test at all.
/// This is that row.
///
/// **The behavioural half is not reachable from `alpine:3`, and that is
/// reported rather than faked.** Watching a setuid-root binary fail to raise
/// privilege needs one to exist, and the third assertion below is the measured
/// claim that this image has none. Nor can the program make one: `chmod u+s` on
/// its own file succeeds and grants it itself, `chown` to root is refused
/// without `CAP_CHOWN`, and the only writable paths are a `nosuid` tmpfs and a
/// `nosuid` `/dev/shm` over a read-only root. It **has** been measured against
/// real ones — `docs/spikes/ISOLATE.md` records setuid-root `isolate` stopping
/// at `Must be started as root` under this profile, and `debian:trixie-slim`'s
/// `mount` says `must be superuser to use mount` here against
/// `drop permissions failed` without the flag. Proving it in the suite would
/// cost a second image and a dependency on util-linux's wording; the kernel's
/// own bit is exact and does not move.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn no_privilege_can_be_gained_after_the_program_starts() {
    let docker = sandbox().await;

    // `cut -f2` because `/proc/self/status` is tab-separated. The second shell
    // is a real `execve`, and it is the half worth asking about: a bit set on
    // the entrypoint alone is one a program sheds by starting a child.
    let outcome = docker
        .run(&shell(
            "echo shell=$(grep ^NoNewPrivs: /proc/self/status | cut -f2); \
             sh -c 'echo child=$(grep ^NoNewPrivs: /proc/self/status | cut -f2)'; \
             echo setuid=$(find / -xdev -perm -4000 -type f 2>/dev/null | wc -l)",
        ))
        .await
        .expect("the run");

    let said = String::from_utf8_lossy(&outcome.stdout);

    assert!(
        said.contains("shell=1"),
        "no_new_privs is not set on the process the sandbox started: {said}",
    );
    assert!(
        said.contains("child=1"),
        "no_new_privs did not survive an exec, so a program sheds it by starting \
         a child: {said}",
    );
    assert!(
        said.contains("setuid=0"),
        "alpine:3 has gained a setuid binary, so the note above is stale and the \
         behavioural half of this property can now be tested for real: {said}",
    );

    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}

// ── One core, and the one that was asked for ────────────────────────────────

/// **Capping CPU is not pinning it**, asked of the kernel inside the container.
///
/// A submission that calls `nproc`, `std::thread::available_parallelism` or
/// `multiprocessing.cpu_count()` is asking one question — *how many cores may I
/// run on* — and `--cpus=1` does not answer it. The cap is a CFS quota: so much
/// CPU time per period, spent across as many cores as the host has. Sixteen
/// threads on sixteen cores is a legal way to spend it, and anything finishing
/// inside one 100 ms period finishes in a sixteenth of the wall clock. The
/// verdict a participant reads comes from the wall clock, so that is time the
/// single-thread rule says they may not have. `Profile::cpuset` is what closes
/// it, and `pipeline.rs` is the only caller: every timed run, one core each.
///
/// **Observed rather than timed, deliberately.** The timing version was measured
/// first and it is not a gate: four spinners burning 1.4 s of CPU under
/// `--cpus=1` took 1835/1867/1886 ms unpinned against 1902/1909/1919 ms pinned,
/// because over that many periods the quota alone already equalises the wall
/// clock. The advantage exists only in a burst shorter than one period, which is
/// far smaller than the container start-up it would have to be measured through.
/// An affinity mask is exact and costs two containers.
///
/// `nproc` is busybox here and it reads `sched_getaffinity`, not `/sys`: pinned
/// to core 15 on this sixteen-core host, `/sys/devices/system/cpu/online` still
/// said `0-15` while `nproc` said `1`.
///
/// **On a host that genuinely has one core** every number below is 1 whether the
/// pin applied or not. The assertions still hold, but nothing can fail, so the
/// case says so on stderr rather than passing quietly — it is a gate everywhere
/// else and an honest no-op there.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn a_pinned_run_is_given_one_core_and_the_one_it_asked_for() {
    let docker = sandbox().await;

    // `nproc` is what a submission calls; `Cpus_allowed_list` is the kernel
    // saying the same thing with no library in between.
    const ASK: &str = "echo nproc=$(nproc); \
                       echo list=$(awk '/^Cpus_allowed_list:/{print $2}' /proc/self/status)";

    fn said(stdout: &[u8], key: &str) -> String {
        String::from_utf8_lossy(stdout)
            .lines()
            .find_map(|line| line.strip_prefix(key).map(|value| value.trim().to_owned()))
            .unwrap_or_default()
    }

    // Unpinned first, and **this is the half that makes the test honest**: the
    // same profile, cap and all, with only `cpuset` missing. What it reports is
    // exactly what a dropped pin would report below.
    let unpinned = docker.run(&shell(ASK)).await.expect("the unpinned run");

    let all_list = said(&unpinned.stdout, "list=");
    let all: usize = said(&unpinned.stdout, "nproc=")
        .parse()
        .unwrap_or_else(|_| panic!("no core count from the unpinned run: {all_list:?}"));

    if all == 1 {
        eprintln!(
            "warning: this host offers one core, so a pinned run and an unpinned one \
             cannot differ. What follows still holds and cannot fail — read this case \
             as unenforced here rather than as a gate that passed.",
        );
    }

    // The last core the unpinned run was actually allowed: `0-15` gives 15,
    // `0-3,8-11` gives 11. Taken from the container rather than from this
    // process because the daemon decides what a container may use, and a core
    // outside that set is refused at create time. Deliberately not core 0,
    // which every host has and which a hard-coded pin would also produce.
    let core: usize = all_list
        .rsplit(',')
        .next()
        .and_then(|group| group.rsplit('-').next())
        .and_then(|core| core.parse().ok())
        .unwrap_or_else(|| panic!("no core number in Cpus_allowed_list {all_list:?}"));

    let pinned = docker
        .run(&shell(ASK).cpuset(core))
        .await
        .expect("the pinned run");

    let one_list = said(&pinned.stdout, "list=");
    let one: usize = said(&pinned.stdout, "nproc=")
        .parse()
        .unwrap_or_else(|_| panic!("no core count from the pinned run: {one_list:?}"));

    assert_eq!(
        one, 1,
        "pinned to core {core}, the kernel still offers {one} cores ({one_list:?}); \
         unpinned and otherwise identical it offers {all} ({all_list:?}), so the pin \
         did not apply",
    );

    // **The core asked for, not merely one core.** A pin that always landed on
    // core 0 would satisfy the count above and put every concurrently timed run
    // on the same core, which is the contention this exists to avoid.
    assert_eq!(
        one_list,
        core.to_string(),
        "core {core} was asked for and the kernel reports {one_list:?}",
    );

    assert_eq!(leftovers(&docker).await, 0, "the container was not removed");
}
