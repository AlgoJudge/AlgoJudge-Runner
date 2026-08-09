//! The Runner.
//!
//! **It does not evaluate anything yet, and that is on purpose.** The verdict
//! reported here is fabricated. Finishing the protocol first — registration,
//! approval, the handshake, leasing, the cache, idempotent reporting — meant it
//! could be proven against the accepted specification's conformance suite while
//! the answer to "is this program correct" was still a constant. The evaluation
//! pipeline replaces one function; everything around it is already right.

mod config;
mod run;

use std::sync::Arc;
use std::time::Duration;

use aj_protocol::{Cache, Identity, Server};
use aj_sandbox::Sandbox as _;

use crate::config::Config;

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
    sandbox.preflight().await?;

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

/// Waits between attempts at something that may simply not be ready.
pub(crate) async fn pause(what: &str, delay: Duration) {
    tracing::info!(?delay, "waiting: {what}");
    tokio::time::sleep(delay).await;
}
