//! What is happening beside a run, for the sandbox to consult while it waits.
//!
//! **The sandbox still knows nothing about problems, tests or verdicts.** What
//! it gains here is two facts it cannot learn from a cgroup: whether anything is
//! passing between this run and whatever is checking it, and whether that
//! checking has finished deciding.
//!
//! Both exist because a judged run stopped being a thing that is collected and
//! became a thing that is *watched*. A program whose answer is already known to
//! be wrong should not keep a Runner for the rest of its limit, and a program
//! talking to an interactor is not idle merely because its processor time is
//! flat — it is waiting for a reply, which is the work.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::watch;

/// Why a caller wants a run stopped before it ends by itself.
///
/// **Two values, and deliberately not one more.** A caller may say *the answer
/// is settled* or *this has printed more than it was allowed*; it may not hand
/// the sandbox a `Memory` or a `TimeLimit`, because those are the sandbox's own
/// findings about a container it is measuring and a caller inventing one would
/// be a verdict with no measurement behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enough {
    /// Whatever was reading this run's output has decided.
    Decided,
    /// It produced more than it was allowed to.
    Output,
}

/// A run and the thing checking it, seen from the sandbox's side.
///
/// Cheap to clone; every clone names the same run.
#[derive(Debug, Clone)]
pub struct Beside {
    /// Bytes that crossed between the run and whatever is checking it.
    ///
    /// **A counter and not a timestamp**, so the reaper can take its own
    /// difference across its own interval, exactly as it does for processor
    /// time. A caller adding to it is making one claim: something moved.
    bytes: Arc<AtomicU64>,
    /// **`watch` rather than `Notify`, and that is the whole reason it is a
    /// channel at all.** A decision reached before the sandbox begins to wait
    /// must not be lost — a program whose first token is wrong is decided
    /// against in microseconds, quite possibly before the container is even
    /// running. `Notify` drops a notification nobody is waiting for; a `watch`
    /// holds the value.
    asked: Arc<watch::Sender<Option<Enough>>>,
}

impl Default for Beside {
    fn default() -> Self {
        Self::new()
    }
}

impl Beside {
    pub fn new() -> Self {
        Self {
            bytes: Arc::new(AtomicU64::new(0)),
            asked: Arc::new(watch::channel(None).0),
        }
    }

    /// A run nothing is happening beside: a build, a checker, anything the
    /// caller starts and simply waits for. What [`crate::Sandbox::run`] passes.
    ///
    /// Named rather than defaulted at the call site so that a run with no
    /// second party says so, instead of saying nothing.
    pub fn alone() -> Self {
        Self::new()
    }

    /// Something crossed between the two sides.
    pub fn moved(&self, bytes: usize) {
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// How much has crossed since the run began.
    pub fn so_far(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The checking side has decided; the run may be stopped.
    ///
    /// Saying it twice is not an error and the first one wins — a comparator
    /// that decides and a relay that then meets the output cap are both telling
    /// the truth about the same run.
    pub fn enough(&self, why: Enough) {
        self.asked.send_if_modified(|held| {
            if held.is_none() {
                *held = Some(why);
                true
            } else {
                false
            }
        });
    }

    /// Resolves when, and only when, somebody has asked.
    ///
    /// Never resolves for a run nobody is watching, which is what makes it safe
    /// as one arm of the `select!` that waits for a container.
    pub(crate) async fn asked(&self) -> Enough {
        let mut seen = self.asked.subscribe();
        loop {
            if let Some(why) = *seen.borrow_and_update() {
                return why;
            }
            if seen.changed().await.is_err() {
                // The sender lives in this same `Arc`, so it cannot have been
                // dropped while this future is held — but a branch of a
                // `select!` that resolved on a closed channel would silently
                // kill every run, so it waits for ever instead.
                std::future::pending::<()>().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nobody_asking_never_resolves() {
        let beside = Beside::alone();
        let asked = tokio::time::timeout(std::time::Duration::from_millis(50), beside.asked());
        assert!(
            asked.await.is_err(),
            "a run nobody is watching must never be stopped by this",
        );
    }

    #[tokio::test]
    async fn a_decision_taken_before_anybody_waits_is_not_lost() {
        let beside = Beside::new();
        // The order this test exists for: the comparator decides on the first
        // token, which can happen before the sandbox reaches its `select!`.
        beside.enough(Enough::Decided);
        assert_eq!(beside.asked().await, Enough::Decided);
    }

    #[tokio::test]
    async fn the_first_reason_is_the_one_kept() {
        let beside = Beside::new();
        beside.enough(Enough::Decided);
        beside.enough(Enough::Output);
        assert_eq!(
            beside.asked().await,
            Enough::Decided,
            "a second caller does not overwrite the reason the run was stopped",
        );
    }

    #[test]
    fn what_moved_is_a_running_total() {
        let beside = Beside::new();
        assert_eq!(beside.so_far(), 0);
        beside.moved(7);
        beside.moved(35);
        assert_eq!(beside.so_far(), 42);
    }

    #[test]
    fn a_clone_names_the_same_run() {
        let beside = Beside::new();
        let theirs = beside.clone();
        theirs.moved(11);
        assert_eq!(beside.so_far(), 11);
    }
}
