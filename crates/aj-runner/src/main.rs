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
        "starting",
    );

    let server = Arc::new(Server::new(&config.base_url)?);
    let cache = Arc::new(Cache::new(&config.cache_path, config.cache_max_bytes));

    // Checked before anything is claimed, and loudly. A sandbox that silently
    // cannot enforce a limit does not produce errors — it produces wrong
    // verdicts, which look like somebody's solution being wrong.
    let sandbox = aj_sandbox::Docker::connect()?;
    if let Err(e) = sandbox.preflight().await {
        if !config.allow_cgroup_v1 {
            return Err(e.into());
        }
        // Said on every start, at the loudest level there is, because a
        // development override that is quiet is a production setting waiting to
        // happen. It cannot be reported to the Server's panel: `MachineDto` is
        // a closed shape and drops anything it does not name.
        tracing::error!(
            "STARTING BELOW SPECIFICATION — {e}.              AJ_Sandbox__AllowCgroupV1 is set. Times and memory reported beside              a verdict on this host are not to be trusted."
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
    run::admitted(&server, &identity, &config).await?;

    run::work(&server, &cache, &pipeline, &config).await
}
