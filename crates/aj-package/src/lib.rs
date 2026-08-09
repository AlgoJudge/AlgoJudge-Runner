//! The `standard-io@1` package.
//!
//! The format is `docs/specs/PACKAGE_FORMAT.md`, accepted 2026-08-08. This crate
//! reads it and nothing else: it does not compile, run, score or talk to a
//! Server. That separation is deliberate — **the extraction defences are a
//! security boundary**, and a boundary that can only be tested by running a
//! whole evaluation is a boundary nobody tests.
//!
//! A package arrives from a problem author, not from a participant, so it is
//! semi-trusted. That is not the same as trusted: an author can be careless, an
//! author's tools can be wrong, and a package travels through a Server that
//! never opens it. Everything here refuses rather than repairs.

pub mod archive;
pub mod config;
pub mod error;
pub mod testset;

pub use archive::{extract, Limits as ArchiveLimits};
pub use config::Config;
pub use error::{Error, Result};
pub use testset::{Test, TestSet};
