//! The Server–Runner contract.
//!
//! The specification is
//! `AlgoJudge-Design/specifications/server-runner/SERVER_RUNNER_API.md`, v1.1,
//! accepted 2026-08-08 and amended since, with its conformance cases in
//! `AlgoJudge.Server.Tests/RunnerConformanceTests.cs`. **Where this crate and
//! that document disagree, the document wins**, and its amendment table is what
//! to read first: the body of an amended section states the earlier form.
//!
//! Nothing here knows what a submission is, how it is compiled or what a
//! verdict means. This layer moves bytes and holds a lease; deciding what to
//! report is somebody else's job. That separation is what lets the evaluation
//! pipeline be replaced without touching the protocol, and it is why L1 could
//! be finished and proven before a sandbox existed.

pub mod backoff;
pub mod cache;
pub mod client;
pub mod error;
pub mod identity;
pub mod stopping;
pub mod wire;

pub use backoff::Backoff;
pub use cache::{Cache, Entry, Locked};
pub use client::Server;
pub use error::{Error, Result};
pub use identity::Identity;
