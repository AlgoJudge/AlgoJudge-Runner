//! `standard-io@1` — deciding what a run was worth.
//!
//! The parts here are the ones the format pins down exactly: how a program's
//! output is compared, what a checker may say, and how tests become a score.
//! None of them starts a container, so all of them are testable on their own —
//! which matters, because these are the rules a participant's mark comes from
//! and "it looked right when I ran it" is not evidence about a scoring rule.

pub mod checker;
pub mod compare;
pub mod details;
pub mod score;

pub use checker::{checker_said, Checked};
pub use compare::{compare, Comparison};
pub use details::{Details, GroupReport, TestReport};
pub use score::{judge, Judgement, Status, TestOutcome};
