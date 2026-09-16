//! The Runner.
//!
//! Registration, approval, the handshake, leasing, the cache, idempotent
//! reporting — and, through `aj-standard-io`, the evaluation itself.
//!
//! **The protocol was finished before anything was evaluated, and that order is
//! worth knowing.** For a while the verdict reported here was a constant, which
//! let every part of the contract be proven against the specification's
//! conformance suite while "is this program correct" was still nobody's
//! problem. The pipeline then replaced one function, and nothing around it
//! moved.

use std::sync::Arc;

use aj_protocol::{Cache, Identity, Server};
use aj_sandbox::Sandbox as _;

use aj_runner::config::Config;
use aj_runner::run;

/// **The runtime is built here rather than by `#[tokio::main]`**, for one line:
/// the `shutdown_timeout` below.
///
/// Dropping a runtime waits for every blocking task to finish, and this Runner
/// has two that a stop cannot interrupt — unpacking a package, which may be a
/// gigabyte, and the relay threads a container that never started leaves
/// waiting. A container runtime allows thirty seconds between `SIGTERM` and
/// `SIGKILL`, and the whole of the stopping arrangement is about giving the
/// jobs in hand back inside it. Waiting out an extraction nobody wants any more
/// would spend that grace on work whose result is already abandoned.
///
/// Five seconds after `work` has returned, which is long past anything that is
/// still doing something useful: the job has been handed back and the report
/// loop has ended before this is reached.
fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let worked = runtime.block_on(started());
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    worked
}

async fn started() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,aj_protocol=info".into()),
        )
        .init();

    let config = Config::from_environment()?;
    let identity = Identity::load_or_create(&config.key_path)?;

    tracing::info!(
        name = %config.name,
        fingerprint = %identity.fingerprint(),
        server = %config.base_url,
        problem_types = ?config.problem_types,
        tags = ?config.tags,
        "starting",
    );

    // **Refused here rather than cut too thin, and refused at start rather than
    // one submission at a time.** A lane is a processor's worth of judging: ask
    // for more lanes than this Runner has processors and every lane is given
    // the whole set, so two judged runs share execution units and spend more
    // processor time on the same work -- and a time limit is processor time, so
    // what an operator would see is correct solutions told they were too slow.
    // There is nothing in a verdict that would say why.
    let processors = aj_sandbox::affinity::width().or_else(aj_sandbox::affinity::on_this_host);
    if let Some(processors) = processors {
        if config.tests_at_once > processors {
            anyhow::bail!(
                "AJ_Runner__TestsAtOnce is {}, and this Runner has {processors} processor(s){}.                  Each test judged at once wants a processor of its own: two judged runs sharing                  one spend more processor time on the same work, and a time limit is processor                  time. Widen this Runner's cpuset, or lower AJ_Runner__TestsAtOnce",
                config.tests_at_once,
                match aj_sandbox::affinity::allowed() {
                    Some(set) => format!(" ({set})"),
                    None => String::new(),
                },
            );
        }
    }
    let lanes = aj_sandbox::affinity::cut(config.tests_at_once);
    tracing::info!(
        tests_at_once = config.tests_at_once,
        lanes = ?lanes,
        "how many tests of one submission are judged at once, and where",
    );
    // **Said rather than refused.** A lane of one thread judges correctly; what
    // it does is charge more processor time for the same work, and a time limit
    // is processor time. Measured 2026-09-15 on one submission of 72 tests: a
    // median 318 ms per test in lanes of one thread against 196 ms in lanes of a
    // whole core, which put 71% of its tests over a limit none of them reached
    // at the wider setting. An operator may have no siblings to give, so this
    // names what it would rather have instead of standing in the way.
    for said in aj_sandbox::affinity::threads_not_cores(&lanes) {
        tracing::warn!(
            "{said}. A lane holds a judged run, the judge reading it and the measuring shim, so a lane of one thread is charged more processor time for the same work than a lane of a whole core -- and a time limit is processor time. Give each lane both threads of a core: read /sys/devices/system/cpu/cpu0/topology/thread_siblings_list and write this Runner's cpuset with siblings together",
        );
    }

    let server = Arc::new(Server::new(&config.base_url)?);
    // The same fingerprint the sandbox is given, and for the same reason: a
    // cache volume may be shared between Runners on one host, and an entry one
    // of them is reading must not be evicted by another.
    let cache = Arc::new(
        Cache::new(
            &config.cache_path,
            config.cache_max_bytes,
            identity.fingerprint(),
        )
        // What the **daemon** calls the same directory. A judge's container is
        // given the package unpacked here and the program built from it, and
        // the daemon is what resolves a bind mount.
        .with_host_root(&config.cache_host_path),
    );
    // What a previous incarnation of this Runner was reading when it stopped.
    // Nobody else can release those, and an entry nobody can evict is a disk
    // that fills.
    cache.sweep();

    // Checked before anything is claimed, and loudly. A sandbox that silently
    // cannot enforce a limit does not produce errors — it produces wrong
    // verdicts, which look like somebody's solution being wrong.
    // The fingerprint names this Runner's own containers, so a second Runner on
    // the host sweeps its orphans and not this one's evaluations. It is on disk
    // and survives a restart, which is the case the sweep exists for.
    let sandbox = aj_sandbox::Docker::connect(identity.fingerprint())?.across(config.tests_at_once);
    if let Err(e) = sandbox.preflight().await {
        if !below_specification(&e, config.allow_unmeasured) {
            return Err(e.into());
        }
        // Said on every start, at the loudest level there is, because a
        // development override that is quiet is a production setting waiting to
        // happen. It cannot be reported to the Server's panel: `MachineDto` is
        // a closed shape and drops anything it does not name.
        tracing::error!(
            "STARTING BELOW SPECIFICATION — {e}. AJ_Sandbox__AllowUnmeasured is set. \
             A time limit is decided on processor time read from this host's \
             cgroups, so this Runner registers and answers the protocol and then \
             fails every job it claims."
        );
    }

    // **Before anything is judged, because the failure it catches is silent.**
    // The cache is where a package is unpacked and its judge built, and both
    // are mounted into the container that judges with them — so a path the
    // daemon cannot open is every submission to every checker problem failing,
    // in words that blame the package's author.
    // One image is enough — what is being asked about is the path — and any of
    // them will do, so the first that is on this host already answers it.
    for image in config.images.all() {
        if sandbox.can_mount(&config.cache_host_path, &image).await? {
            break;
        }
    }

    // Job containers are siblings, so they outlive the process that made them.
    // Anything left by a previous incarnation goes before this one starts.
    let swept = sandbox.sweep().await?;
    if swept > 0 {
        tracing::warn!(swept, "sandbox containers from a previous run were removed");
    }

    let pipeline = aj_standard_io::Pipeline::new(sandbox, config.images.clone());

    // A Runner that cannot reach the Server yet is not a Runner that has
    // failed: a Compose stack brings both up at once, and the one that wins the
    // race would otherwise exit before the other finished migrating.
    //
    // **Nothing is listening yet, and that is the right answer here.** A Runner
    // waiting to be approved holds nothing, so an uncaught `SIGTERM` takes the
    // process down at once — which is what somebody stopping it wants, and
    // faster than any handler could manage. Installing one first would only put
    // machinery between the signal and an exit that costs nothing. The External
    // Runner says the same where it does the same thing.
    //
    // The handle below is therefore one nothing ever says the word to. It is
    // not decoration: `admitted` is **re-entered from inside `work`**, where a
    // handler *is* installed, and there the same waits have to hear it — the
    // comment under this one used to claim they did.
    let (before_anything_is_held, _never) = aj_protocol::stopping::Stopping::told();
    run::admitted(&server, &identity, &config, &before_anything_is_held).await?;

    // **Listening starts once there is something to lose**, which is the first
    // claim. Until 2026-09-04 the comment here said it started before the wait
    // for approval as well; it did not, and now it deliberately does not.
    let stopping = aj_protocol::stopping::Stopping::listen();

    let worked = run::work(&server, &cache, &pipeline, &config, &stopping).await;

    // **What this Runner started, this Runner ends.** A job container is the
    // daemon's child rather than the Runner's, so nothing else would stop one
    // that is still computing for a job already given back — it would run to
    // the end, on a host that has been told to stop, for an answer nobody will
    // read. The next start sweeps whatever this could not.
    if stopping.now() {
        match pipeline.sandbox().sweep().await {
            Ok(swept) => tracing::info!(swept, "cleared the containers this Runner had running"),
            Err(e) => tracing::warn!(%e, "could not clear the containers; the next start will"),
        }
    }

    worked
}

/// Whether a failed preflight is the one failure the development override is
/// allowed to start past.
///
/// **`Refused` and nothing else.** That variant is preflight's own verdict on
/// the host — cgroup v1, a driver that is not `cgroupfs`, or a cgroup tree it
/// cannot write to — and it is what `AJ_Sandbox__AllowUnmeasured` is documented
/// to permit, in `config.rs` and in `docs/SECURITY.md` §5 alike.
///
/// The switch used to suppress every failure the check could produce. `Runtime`
/// and `Io` are the container runtime being unreachable, which on a development
/// stack usually means the socket is mounted wrong; a Runner that starts past
/// that can judge nothing at all, and reported it as running below
/// specification.
fn below_specification(error: &aj_sandbox::Error, allow_unmeasured: bool) -> bool {
    allow_unmeasured && matches!(error, aj_sandbox::Error::Refused(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_starts_past_the_cgroup_verdict() {
        let refused = aj_sandbox::Error::Refused("this host reports cgroup version 1".into());
        assert!(below_specification(&refused, true));
        assert!(!below_specification(&refused, false));
    }

    /// The half the switch was never for: a Runner that cannot reach the
    /// container runtime judges nothing, and must say so by exiting.
    #[test]
    fn no_override_starts_past_a_runtime_that_is_not_there() {
        let unreachable =
            aj_sandbox::Error::Io(std::io::Error::other("/var/run/docker.sock: not found"));
        assert!(!below_specification(&unreachable, true));
        assert!(!below_specification(&unreachable, false));
    }
}
