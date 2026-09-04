//! Being told to stop, and hearing it everywhere at once.
//!
//! **Here rather than in one Runner**, because both of them need it and neither
//! is more entitled to it: a Runner waiting on a container and an External
//! Runner waiting on somebody else's archive are stopped the same way and owe
//! the queue the same thing.
//!
//! **A Runner that is stopped in the middle of a job gives that job back.** The
//! alternative — and what happened before this existed — is going quiet and
//! letting the lease expire, which costs a participant up to ten minutes while
//! idle Runners wait for a deadline nobody is going to miss.
//!
//! The signal has to reach **every wait**, not two of them. It said "two
//! places" — the poll and the evaluation — until 2026-09-04, and the sentence
//! was read as a list rather than as an example: three other waits were sleeping
//! straight through a stop, and each of them is longer than either of the two
//! that were covered.
//!
//!   - the retry that carries an answer already computed, bounded by the lease:
//!     **ten minutes** at the shipped default;
//!   - the wait on a Server that is deliberately down, which honours the
//!     operator's own `Retry-After` and so has **no bound this side sets**;
//!   - the registration loop, which a Runner re-enters whenever its token is
//!     forgotten, and which took no handle at all.
//!
//! A container runtime allows thirty seconds. Any of those three turns a stop
//! into a `SIGKILL`, and a `SIGKILL` is not a slower release — it is none: the
//! jobs in hand stay leased for their full lease, which is the cost this module
//! exists to remove.
//!
//! [`Stopping::sleep`] is how a wait becomes stop-aware, and the reason it is
//! here rather than written out at each site is that it was written out at two
//! sites and forgotten at three.

use std::sync::Arc;

/// The word from outside that this Runner is to stop.
///
/// Cloneable and cheap: every place that has to notice holds one.
#[derive(Clone)]
pub struct Stopping {
    /// **Held here as well as by the listener**, so the channel never closes.
    /// A receiver whose sender is gone reports a change that never comes, and
    /// a Runner must not read that as having been told to stop.
    _tell: Arc<tokio::sync::watch::Sender<bool>>,
    told: tokio::sync::watch::Receiver<bool>,
}

impl Stopping {
    /// Starts listening, and answers before anything has been heard.
    ///
    /// **`SIGTERM` is what a container runtime sends**, and it is the one that
    /// matters: `docker stop`, `compose down` and an operator's `systemctl` all
    /// arrive that way. Ctrl-C is the same thing from a developer's terminal.
    ///
    /// A second signal is not caught, deliberately. The default disposition
    /// takes the process down immediately, which is exactly what somebody
    /// sending it a second time is asking for.
    pub fn listen() -> Self {
        let (stopping, teller) = Self::told();

        tokio::spawn(async move {
            wait_for_a_signal().await;
            tracing::info!("told to stop; the work in hand goes back to the queue");
            teller.stop();
        });

        stopping
    }

    /// A handle that hears the word from its own caller rather than from the
    /// operating system, and the thing that says it.
    ///
    /// **Because raising a signal is not a test.** What a Runner does to the
    /// jobs in hand when it is stopped is the whole point of this module, and
    /// the only other way to reach it from a test in the same process is
    /// `raise(SIGTERM)` — which arrives at the process rather than at the test,
    /// and races the handler's own registration. Lose that race and the default
    /// disposition takes the whole test binary down.
    ///
    /// `listen` is this plus a signal, so a test driving a loop through here
    /// drives it down the path production uses rather than beside it.
    pub fn told() -> (Self, Teller) {
        let (tell, told) = tokio::sync::watch::channel(false);
        let tell = Arc::new(tell);

        (
            Self {
                _tell: Arc::clone(&tell),
                told,
            },
            Teller(tell),
        )
    }

    /// Whether the word has already come.
    pub fn now(&self) -> bool {
        *self.told.borrow()
    }

    /// Resolves when it comes, and stays resolved afterwards.
    ///
    /// Takes `&self` and clones the receiver, so a caller holding one handle can
    /// wait in several `select!`s without threading a mutable borrow through
    /// every one of them.
    pub async fn wait(&self) {
        let mut told = self.told.clone();
        while !*told.borrow_and_update() {
            if told.changed().await.is_err() {
                // Cannot happen while `_tell` is held, and if it somehow did,
                // waiting for ever is the safe reading: nobody said stop.
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Stopping {
    /// Waits out `delay`, or returns early because the word came.
    ///
    /// Answers **whether the whole delay elapsed** — so a caller can tell a
    /// backoff that ran its course from one that was cut short, which is
    /// usually the difference between trying again and going home.
    pub async fn sleep(&self, delay: std::time::Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => true,
            _ = self.wait() => false,
        }
    }
}

/// Says the word to every handle made alongside it.
pub struct Teller(Arc<tokio::sync::watch::Sender<bool>>);

impl Teller {
    /// Says it, once and for good: a handle that starts waiting afterwards
    /// still hears it.
    pub fn stop(&self) {
        let _ = self.0.send(true);
    }
}

#[cfg(unix)]
async fn wait_for_a_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    match signal(SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Err(e) => {
            // Nothing else can be done, and a Runner that cannot hear `SIGTERM`
            // is killed by it, which is where every Runner started.
            tracing::warn!(%e, "cannot listen for SIGTERM; a stop will be a kill");
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// The Runner ships as a Linux container; this is for `cargo test` on a
/// developer's machine, where Ctrl-C is the only signal there is.
#[cfg(not(unix))]
async fn wait_for_a_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A wait that was cut short says so**, and one that ran its course says
    /// that instead. Every caller of `sleep` branches on the answer, so getting
    /// the two the wrong way round would turn "we are stopping" into "try
    /// again" at three sites at once.
    #[tokio::test]
    async fn a_sleep_says_whether_it_was_cut_short() {
        let (stopping, teller) = Stopping::told();

        assert!(
            stopping.sleep(std::time::Duration::from_millis(10)).await,
            "a delay nobody interrupted has elapsed",
        );

        teller.stop();
        let began = tokio::time::Instant::now();
        assert!(
            !stopping.sleep(std::time::Duration::from_secs(30)).await,
            "a delay the word interrupted has not elapsed",
        );
        assert!(
            began.elapsed() < std::time::Duration::from_secs(1),
            "it waited {:?}, so the word did not cut it short",
            began.elapsed(),
        );
    }

    /// **Nothing has been said until something is said.** A Runner that read a
    /// fresh handle as a stop would exit before claiming anything.
    #[tokio::test]
    async fn a_fresh_one_has_heard_nothing() {
        let stopping = Stopping::listen();
        assert!(!stopping.now());

        // And waiting on it does not resolve on its own.
        tokio::select! {
            _ = stopping.wait() => panic!("it resolved with nothing said"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
    }

    /// Every holder hears it, and hears it however late it starts listening.
    #[tokio::test]
    async fn the_word_reaches_a_handle_that_was_not_waiting_yet() {
        let (stopping, teller) = Stopping::told();
        let elsewhere = stopping.clone();

        teller.stop();

        assert!(stopping.now());
        assert!(elsewhere.now());
        // Resolves immediately, rather than waiting for a change that has been
        // and gone.
        tokio::time::timeout(std::time::Duration::from_millis(50), elsewhere.wait())
            .await
            .expect("it had already been said");
    }
}
