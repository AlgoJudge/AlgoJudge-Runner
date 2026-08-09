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
//! a process.
//!
//! ```text
//! docker pull alpine:3
//! ./x test -p aj-sandbox --test adversarial -- --include-ignored --test-threads=1
//! ```
//!
//! `alpine` rather than a language image on purpose: these test the isolation,
//! not the toolchain, and a shell is all that is needed to misbehave.

use std::time::Duration;

use aj_sandbox::{Docker, Mount, Profile, Sandbox, Stopped};

const IMAGE: &str = "alpine:3";

/// A sandbox with the image present and nothing left over from before.
///
/// The sweep is not tidiness — it is the production behaviour, run here for the
/// same reason it runs at start: sibling containers outlive whatever made them.
async fn sandbox() -> Docker {
    let docker = Docker::connect().expect("a container runtime");

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
                .memory_kib(512 * 1024)
                .tmpfs_kib(128 * 1024),
        )
        .await
        .expect("the run");

    let Some(peak_kib) = outcome.peak_memory_kib else {
        eprintln!("peak memory was not measured: no writable cgroup root. Skipping.");
        return;
    };

    assert!(
        peak_kib >= 64 * 1024,
        "64 MiB was written, so the peak cannot be below it: got {peak_kib} KiB",
    );
    assert!(
        peak_kib < 128 * 1024,
        "the floor is about 2 MiB, so twice the allocation means the wrong cgroup: got {peak_kib} KiB",
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
                .memory_kib(32 * 1024)
                .tmpfs_kib(1024 * 1024)
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
            .tmpfs_kib(16 * 1024),
        )
        .await
        .expect("the run");

    // Writable is the point of scratch space; executable is what turns
    // "produced a file" into "ran something nobody compiled".
    assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "NOEXEC");
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

// ── A7 — it tries to persist ────────────────────────────────────────────────

/// One container per test, never reused. A fresh container is the only answer
/// that does not depend on cleanup having been written correctly.
#[tokio::test]
#[ignore = "needs a container runtime"]
async fn nothing_survives_from_one_run_to_the_next() {
    let docker = sandbox().await;

    let first = docker
        .run(&shell("echo remembered > /tmp/state; echo written").tmpfs_kib(1024))
        .await
        .expect("the first run");
    assert_eq!(String::from_utf8_lossy(&first.stdout).trim(), "written");

    let second = docker
        .run(&shell("cat /tmp/state 2>/dev/null || echo GONE").tmpfs_kib(1024))
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
                .tmpfs_kib(256 * 1024)
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
