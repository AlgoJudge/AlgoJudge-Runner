//! L3 — where untrusted code actually runs.
//!
//! **This crate does not write a sandbox.** Execution is delegated to an
//! existing, maintained isolation mechanism; what is here is the profile that
//! configures one and the reading of what came back. That is a decision
//! (D-1/§2 of the technology analysis, 2026-08-06), and it matches every mature
//! judge: a high-level orchestrator plus a narrow, native isolation component.
//!
//! One implementation today, over containers. `isolate` 2.x is provisionally
//! accepted as a deeper supervisor pending a spike on cgroup delegation, and the
//! [`Sandbox`] trait is the seam it lands behind — the only place in the product
//! that knows what a container is.
//!
//! Nothing here understands a problem, a test or a verdict. It is handed a
//! command and limits, and reports what happened.

pub mod cgroups;
pub mod docker;
pub mod profile;

pub use cgroups::Cgroups;
pub use docker::Docker;
pub use profile::{Mount, Outcome, Profile, Stopped};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The container runtime itself failed — it would not start, the socket was
    /// not there, the image was missing. **Never a verdict**: the submission was
    /// not judged, and saying it was wrong would be a fabricated statement about
    /// somebody's work.
    #[error("the container runtime failed: {0}")]
    Runtime(#[from] bollard::errors::Error),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Refused(String),
}

/// Runs one command under limits, once.
#[async_trait::async_trait]
pub trait Sandbox: Send + Sync {
    /// What this is, for a log and for the capabilities a Runner publishes.
    fn name(&self) -> &'static str;

    /// Everything the mechanism needs before it can be trusted with untrusted
    /// code — a reachable runtime, a kernel that supports the limits. Checked at
    /// start and reported loudly, because a sandbox that silently does not
    /// enforce a limit produces wrong verdicts rather than errors.
    async fn preflight(&self) -> Result<()>;

    async fn run(&self, profile: &Profile) -> Result<Outcome>;
}
