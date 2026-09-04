//! The Runner, as a library. The binary is a thin shell over this.
//!
//! **The split exists for one reason**, and it is worth stating rather than
//! leaving as a matter of taste: an integration test cannot reach into a binary
//! crate. Leasing is the part of this Runner with no observable output — a lease
//! being renewed looks exactly like one that has not expired yet — so the only
//! honest proof is to hold a real lease past its own deadline and then ask the
//! Server whose job it is. That test needs [`keeper::Keeper`], and `tests/` can
//! only see a library.

use aj_protocol::stopping::Stopping;
use std::time::Duration;

pub mod config;
pub mod keeper;
pub mod run;

/// Waits between attempts at something that may simply not be ready.
///
/// Answers whether the wait ran its course. Every caller is inside a loop that
/// would otherwise keep a stopping process alive for the length of a backoff
/// against a Server that is, by the time it matters, deliberately down.
pub(crate) async fn pause(what: &str, delay: Duration, stopping: &Stopping) -> bool {
    tracing::info!(?delay, "waiting: {what}");
    stopping.sleep(delay).await
}
