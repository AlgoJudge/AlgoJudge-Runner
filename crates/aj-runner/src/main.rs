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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    let server = Arc::new(Server::new(&config.base_url)?);
    // The same fingerprint the sandbox is given, and for the same reason: a
    // cache volume may be shared between Runners on one host, and an entry one
    // of them is reading must not be evicted by another.
    let cache = Arc::new(Cache::new(
        &config.cache_path,
        config.cache_max_bytes,
        identity.fingerprint(),
    ));
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
    let sandbox = aj_sandbox::Docker::connect(identity.fingerprint())?;
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
