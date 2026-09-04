//! The Runner, as a library. The binary is a thin shell over this.
//!
//! **The split exists for one reason**, and it is worth stating rather than
//! leaving as a matter of taste: an integration test cannot reach into a binary
//! crate. Leasing is the part of this Runner with no observable output — a lease
//! being renewed looks exactly like one that has not expired yet — so the only
//! honest proof is to hold a real lease past its own deadline and then ask the
//! Server whose job it is. That test needs [`keeper::Keeper`], and `tests/` can
//! only see a library.

use std::time::Duration;

pub mod config;
pub mod keeper;
pub mod run;

/// Waits between attempts at something that may simply not be ready.
pub(crate) async fn pause(what: &str, delay: Duration) {
    tracing::info!(?delay, "waiting: {what}");
    tokio::time::sleep(delay).await;
}
